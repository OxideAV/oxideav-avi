//! Zero-length data chunks — the dropped / repeated-frame placeholders
//! capture and transcoding writers leave in `movi`.
//!
//! With `dwSampleSize` 0 "each sample of data (such as a video frame)
//! must be in a separate chunk", and only an `AVIIF_NO_TIME` chunk "does
//! not affect timing" (AVI 1.0 §AVISTREAMHEADER, §AVIOLDINDEX). So a
//! zero-length chunk is a frame-time slot with no frame: the demuxer does
//! not surface it as a packet (there is nothing to decode), the stream
//! clock advances past it, and the next real packet keeps its timestamp.
//! The muxer writes the same placeholders for a gap in `dts`.
//!
//! The two fixtures were made by `ffmpeg` (frames dropped by a `select`
//! filter, passthrough timing); the expected packet timestamps are what
//! `ffprobe -show_entries packet=pts,size,flags` reports for them.

use std::io::Cursor;

use oxideav_avi::demuxer::{AviDemuxer, open_avi};
use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit, open_with_options};
use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, MediaType,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

const MJPEG_FIXTURE: &[u8] = include_bytes!("fixtures/zero_length/mjpeg_dropped_frames.avi");
const MPEG4_FIXTURE: &[u8] = include_bytes!("fixtures/zero_length/mpeg4_pcm_dropped_frames.avi");

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    for (id, tag) in [
        ("mjpeg", CodecTag::fourcc(b"MJPG")),
        ("mpeg4video", CodecTag::fourcc(b"FMP4")),
        ("mpeg4video", CodecTag::fourcc(b"XVID")),
        ("h264", CodecTag::fourcc(b"H264")),
    ] {
        reg.register(CodecInfo::new(CodecId::new(id)).tag(tag));
    }
    reg
}

fn open(bytes: &[u8]) -> AviDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    open_avi(rs, &registry()).unwrap()
}

/// `(stream, pts, size, keyframe)` for every packet, in file order.
fn drain(dmx: &mut AviDemuxer) -> Vec<(u32, i64, usize, bool)> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push((
                p.stream_index,
                p.pts.unwrap(),
                p.data.len(),
                p.flags.keyframe,
            )),
            Err(Error::Eof) => return out,
            Err(e) => panic!("next_packet: {e}"),
        }
    }
}

fn video_only(all: &[(u32, i64, usize, bool)], stream: u32) -> Vec<(i64, usize, bool)> {
    all.iter()
        .filter(|p| p.0 == stream)
        .map(|p| (p.1, p.2, p.3))
        .collect()
}

#[test]
fn mjpeg_fixture_skips_placeholders_and_keeps_timestamps() {
    // movi: five 00dc chunks, two zero-length, five more (10 slots).
    let mut dmx = open(MJPEG_FIXTURE);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mjpeg");
    let v = video_only(&drain(&mut dmx), 0);
    // ffprobe: pts 0,1,2,5,6,7,8,9 with sizes 1699,1696,1688,1690,1686,1679,1676,1661.
    let want = [
        (0, 1699),
        (1, 1696),
        (2, 1688),
        (5, 1690),
        (6, 1686),
        (7, 1679),
        (8, 1676),
        (9, 1661),
    ];
    assert_eq!(
        v.iter().map(|p| (p.0, p.1)).collect::<Vec<_>>(),
        want.iter().map(|&(t, s)| (t, s)).collect::<Vec<_>>()
    );
    assert!(v.iter().all(|p| p.2), "every MJPG frame is a key frame");
    // The keyframe accessor counts the same packets next_packet returns.
    assert_eq!(dmx.keyframe_indexed_packet_count(0), want.len());
}

#[test]
fn mpeg4_fixture_skips_placeholders_between_delta_frames() {
    let mut dmx = open(MPEG4_FIXTURE);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mpeg4video");
    assert_eq!(dmx.streams()[1].params.media_type, MediaType::Audio);
    let all = drain(&mut dmx);
    let v = video_only(&all, 0);
    // ffprobe: pts,size,flags
    let want: [(i64, usize, bool); 9] = [
        (0, 1187, true),
        (1, 77, false),
        (2, 99, false),
        (5, 209, false),
        (6, 1185, true),
        (7, 79, false),
        (8, 102, false),
        (10, 177, false),
        (11, 1171, true),
    ];
    assert_eq!(v, want);
    // Audio is untouched: 10 PCM chunks, contiguous sample timestamps.
    let a: Vec<(i64, usize)> = all
        .iter()
        .filter(|p| p.0 == 1)
        .map(|p| (p.1, p.2))
        .collect();
    assert_eq!(a.len(), 10);
    let mut t = 0;
    for (pts, size) in a {
        assert_eq!(pts, t);
        t += (size / 2) as i64;
    }
    for (seq, w) in want.iter().enumerate() {
        assert_eq!(dmx.packet_is_keyframe(0, seq), Some(w.2), "packet {seq}");
    }
    assert_eq!(dmx.keyframe_indexed_packet_count(0), want.len());
}

#[test]
fn seek_into_a_placeholder_lands_on_a_real_key_frame() {
    let mut dmx = open(MPEG4_FIXTURE);
    // pts 4 is a placeholder slot inside the first GOP → key frame 0.
    assert_eq!(dmx.seek_to(0, 4).unwrap(), 0);
    let p = dmx.next_packet().unwrap();
    assert_eq!((p.stream_index, p.pts, p.data.len()), (0, Some(0), 1187));
    // pts 9 (placeholder) → key frame 6; the next video packet after it
    // is the delta frame at 7.
    assert_eq!(dmx.seek_to(0, 9).unwrap(), 6);
    let mut video = Vec::new();
    while video.len() < 4 {
        let p = dmx.next_packet().unwrap();
        if p.stream_index == 0 {
            video.push(p.pts.unwrap());
        }
    }
    assert_eq!(video, [6, 7, 8, 10]);

    let mut mj = open(MJPEG_FIXTURE);
    // MJPEG is all-key: pts 4 → the last real frame before it, 2.
    assert_eq!(mj.seek_to(0, 4).unwrap(), 2);
    assert_eq!(mj.next_packet().unwrap().pts, Some(2));
}

// ───────────────────────── muxer side ─────────────────────────

#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for SharedBuf {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

fn video_stream(codec: &str) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new(codec));
    params.width = Some(32);
    params.height = Some(16);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Mux `(dts, keyframe)` frames; the payload of frame `i` is 40 + i bytes.
fn mux(stream: &StreamInfo, frames: &[(i64, bool)], kind: AviKind) -> Vec<u8> {
    let buf = SharedBuf::default();
    let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
    let mut m = open_with_options(
        ws,
        std::slice::from_ref(stream),
        kind,
        AviMuxOptions::default(),
    )
    .unwrap();
    m.write_header().unwrap();
    for (i, &(dts, key)) in frames.iter().enumerate() {
        let mut p = Packet::new(0, stream.time_base, vec![0x5A ^ i as u8; 40 + i]);
        p.pts = Some(dts);
        p.dts = Some(dts);
        p.flags.keyframe = key;
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    buf.0.lock().unwrap().get_ref().clone()
}

/// `(chunk id, size)` of every chunk directly in (the first) `movi`.
fn movi_chunks(avi: &[u8]) -> Vec<([u8; 4], u32)> {
    let at = avi.windows(4).position(|w| w == b"movi").unwrap();
    let list_size = u32::from_le_bytes(avi[at - 4..at].try_into().unwrap()) as usize;
    let end = at + list_size;
    let mut i = at + 4;
    let mut out = Vec::new();
    while i + 8 <= end {
        let id: [u8; 4] = avi[i..i + 4].try_into().unwrap();
        let size = u32::from_le_bytes(avi[i + 4..i + 8].try_into().unwrap());
        if &id == b"LIST" {
            i += 12;
            continue;
        }
        out.push((id, size));
        i += 8 + size as usize + (size as usize & 1);
    }
    out
}

const GAPPED: [(i64, bool); 6] = [
    (0, true),
    (1, false),
    (2, false),
    (5, false),
    (6, true),
    (9, false),
];

#[test]
fn muxer_fills_a_dts_gap_with_zero_length_chunks() {
    let avi = mux(&video_stream("h264"), &GAPPED, AviKind::Avi10);
    let sizes: Vec<u32> = movi_chunks(&avi)
        .into_iter()
        .filter(|c| &c.0 == b"00dc")
        .map(|c| c.1)
        .collect();
    assert_eq!(sizes, [40, 41, 42, 0, 0, 43, 44, 0, 0, 45]);
    // dwLength counts frame slots, placeholders included.
    let strh = avi.windows(4).position(|w| w == b"strh").unwrap() + 8;
    assert_eq!(
        u32::from_le_bytes(avi[strh + 32..strh + 36].try_into().unwrap()),
        10
    );
}

#[test]
fn gapped_stream_round_trips_through_avi10_and_opendml() {
    for kind in [
        AviKind::Avi10,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(256)),
    ] {
        let avi = mux(&video_stream("h264"), &GAPPED, kind);
        let mut dmx = open(&avi);
        assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "h264");
        let got = video_only(&drain(&mut dmx), 0);
        let want: Vec<(i64, usize, bool)> = GAPPED
            .iter()
            .enumerate()
            .map(|(i, &(t, k))| (t, 40 + i, k))
            .collect();
        assert_eq!(got, want, "{kind:?}");
        for (seq, w) in want.iter().enumerate() {
            assert_eq!(dmx.packet_is_keyframe(0, seq), Some(w.2), "{kind:?} #{seq}");
        }
        // Seeking at a placeholder slot lands on the key frame before it.
        assert_eq!(dmx.seek_to(0, 8).unwrap(), 6, "{kind:?}");
        assert_eq!(dmx.next_packet().unwrap().pts, Some(6));
    }
}

#[test]
fn packets_without_dts_or_behind_the_clock_get_no_filler() {
    let stream = video_stream("h264");
    let buf = SharedBuf::default();
    let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
    let mut m = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
    m.write_header().unwrap();
    // pts-only (reordered presentation order) and a repeated dts.
    for (pts, dts) in [
        (Some(0), None),
        (Some(3), None),
        (Some(1), Some(1)),
        (None, Some(1)),
    ] {
        let mut p = Packet::new(0, stream.time_base, vec![1; 10]);
        p.pts = pts;
        p.dts = dts;
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let avi = buf.0.lock().unwrap().get_ref().clone();
    let sizes: Vec<u32> = movi_chunks(&avi).into_iter().map(|c| c.1).collect();
    assert_eq!(sizes, [10, 10, 10, 10]);
}

/// Black-box: `ffprobe` reads the muxer's placeholders the same way.
#[test]
fn ffprobe_agrees_on_the_gapped_timestamps() {
    let avi = mux(&video_stream("mjpeg"), &GAPPED, AviKind::Avi10);
    let path = std::env::temp_dir().join("oxideav-avi-zero-length-ffprobe.avi");
    std::fs::write(&path, &avi).unwrap();
    let out = match std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "packet=pts,size",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("skip: ffprobe not available");
            return;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let got: Vec<&str> = text.lines().collect();
    assert_eq!(got, ["0,40", "1,41", "2,42", "5,43", "6,44", "9,45"]);
}

#[test]
fn seek_finds_key_frames_in_a_riff_avix_continuation() {
    // idx1 covers the primary RIFF only; the key frame at 4 sits in the
    // AVIX continuation, which only the `ix##` standard index reaches.
    let frames = [
        (0, true),
        (1, false),
        (2, false),
        (3, false),
        (4, true),
        (5, false),
    ];
    let avi = mux(
        &video_stream("h264"),
        &frames,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(256)),
    );
    let mut dmx = open(&avi);
    assert!(dmx.movi_segment_count() > 1);
    assert_eq!(dmx.seek_to(0, 5).unwrap(), 4);
    let p = dmx.next_packet().unwrap();
    assert_eq!((p.pts, p.data.len()), (Some(4), 44));
}
