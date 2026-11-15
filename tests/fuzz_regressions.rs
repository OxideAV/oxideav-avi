//! Regression tests for findings of the `mux_roundtrip` /
//! `structured_mutate` / `demux` fuzz targets.
//!
//! Each test rebuilds the minimised recipe through the public muxer API
//! and states the container rule the finding turned on.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_avi::demuxer::{AviDemuxer, open_avi, open_avi_strict};
use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};
use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, Muxer, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Cursor<Vec<u8>>>>);

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

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg
}

fn video_stream() -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.width = Some(112);
    params.height = Some(112);
    params.frame_rate = Some(Rational::new(15, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 15),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// What to write after frame `i`.
#[derive(Clone, Copy, PartialEq)]
enum Side {
    None,
    PaletteAndText,
}

/// Mux `(payload length, keyframe)` frames on one video stream, frame
/// `i` at `pts` `i`, optionally followed by a palette change and a text
/// chunk each.
fn mux(frames: &[(usize, bool)], kind: AviKind, side: Side) -> Vec<u8> {
    let stream = video_stream();
    let buf = SharedBuf::default();
    let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
    let mut m = oxideav_avi::muxer::open_avi(
        ws,
        std::slice::from_ref(&stream),
        kind,
        AviMuxOptions::new(),
    )
    .unwrap();
    m.write_header().unwrap();
    for (i, &(len, key)) in frames.iter().enumerate() {
        let data: Vec<u8> = (0..len).map(|b| (b as u8) ^ (i as u8)).collect();
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = key;
        m.write_packet(&pkt).unwrap();
        if side == Side::PaletteAndText {
            m.write_palette_change(0, &[0, 2, 0, 0, 10, 20, 30, 0, 40, 50, 60, 0])
                .unwrap();
            m.write_text_chunk(0, b"overlay").unwrap();
        }
    }
    m.write_trailer().unwrap();
    drop(m);
    Arc::try_unwrap(buf.0)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap()
        .into_inner()
}

fn open(bytes: &[u8]) -> AviDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    open_avi_strict(rs, &registry()).expect("strict open of muxer output");
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    open_avi(rs, &registry()).unwrap()
}

/// `(pts, size, keyframe)` of every packet until end of file.
fn drain(dmx: &mut AviDemuxer) -> Vec<(i64, usize, bool)> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push((p.pts.unwrap(), p.data.len(), p.flags.keyframe)),
            Err(Error::Eof) => return out,
            Err(e) => panic!("next_packet: {e}"),
        }
    }
}

const KINDS: [AviKind; 3] = [
    AviKind::Avi10,
    AviKind::OpenDml(RiffSegmentLimit::OneGiB),
    // 4 KiB segments: every few frames roll a `RIFF AVIX`.
    AviKind::OpenDml(RiffSegmentLimit::Bytes(4096)),
];

/// Scheduled fuzz run 43601510806, `mux_roundtrip`, input
/// `00 d8 d7 d8 d7 0e f5`: one video stream, twenty zero-length packets.
///
/// With `dwSampleSize` 0 a chunk is one frame-time, and a zero-length
/// chunk is a frame-time with no frame (AVI 1.0 §AVISTREAMHEADER; only
/// an `AVIIF_NO_TIME` chunk "does not affect timing", §AVIOLDINDEX). The
/// file holds twenty time slots and no packet: it must open (strictly),
/// read as immediately at end of file, and refuse a seek without
/// panicking. The harness used to demand twenty empty packets back.
#[test]
fn all_zero_length_video_is_twenty_slots_and_no_packets() {
    for kind in KINDS {
        let bytes = mux(&[(0, true); 20], kind, Side::None);
        let mut dmx = open(&bytes);
        assert_eq!(dmx.streams().len(), 1);
        assert_eq!(drain(&mut dmx), []);
        assert_eq!(dmx.keyframe_indexed_packet_count(0), 0);
        assert!(dmx.seek_to(0, 0).is_err(), "no frame to land on");
        assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    }
}

/// Zero-length packets between real frames: every returned packet keeps
/// the frame slot it was written at, and a seek lands on real frames.
#[test]
fn zero_length_slots_keep_frame_timestamps() {
    let frames = [
        (0, true),
        (40, true),
        (0, false),
        (0, true),
        (9, false),
        (33, true),
        (0, false),
        (5, false),
    ];
    for kind in KINDS {
        let bytes = mux(&frames, kind, Side::None);
        let mut dmx = open(&bytes);
        let want = [(1, 40, true), (4, 9, false), (5, 33, true), (7, 5, false)];
        assert_eq!(drain(&mut dmx), want);
        for (seq, w) in want.iter().enumerate() {
            assert_eq!(dmx.packet_is_keyframe(0, seq), Some(w.2));
        }
        assert_eq!(dmx.keyframe_indexed_packet_count(0), want.len());
        // Slot 0 is empty: the first key frame is the landing point.
        assert_eq!(dmx.seek_to(0, 0).unwrap(), 1);
        assert_eq!(dmx.next_packet().unwrap().pts, Some(1));
        // Slot 3 is an empty "key" slot: land on the frame at 1.
        assert_eq!(dmx.seek_to(0, 4).unwrap(), 1);
        assert_eq!(dmx.seek_to(0, 6).unwrap(), 5);
        assert_eq!(drain(&mut dmx), [(5, 33, true), (7, 5, false)]);
    }
}

/// `mux_roundtrip` neighbour (recipe: palette change + text chunk after
/// every frame): the side-band chunks' `idx1` entries were counted as
/// frames — `packet_is_keyframe` went out of step with the packets
/// returned, and a seek resumed with timestamps advanced by one tick per
/// side-band chunk passed.
///
/// A palette change "does not affect timing" (AVI 1.0 §AVIOLDINDEX,
/// `AVIIF_NO_TIME`), and the OpenDML standard index lists frames only
/// ("Palette changes should not appear in the same Index List as ##dc
/// chunks", OpenDML "Index Locations in RIFF File").
#[test]
fn side_band_chunks_take_no_frame_slot_in_any_index() {
    let frames: Vec<(usize, bool)> = (0..40).map(|i| (300 + i, i % 8 == 0)).collect();
    for kind in KINDS {
        let bytes = mux(&frames, kind, Side::PaletteAndText);
        let mut dmx = open(&bytes);
        let want: Vec<(i64, usize, bool)> = frames
            .iter()
            .enumerate()
            .map(|(i, &(len, key))| (i as i64, len, key))
            .collect();
        assert_eq!(drain(&mut dmx), want);
        assert_eq!(dmx.keyframe_indexed_packet_count(0), frames.len());
        for (seq, w) in want.iter().enumerate() {
            assert_eq!(dmx.packet_is_keyframe(0, seq), Some(w.2), "packet {seq}");
        }
        for target in [0i64, 7, 8, 21, 39, 1000] {
            let key = (target.min(39) / 8) * 8;
            assert_eq!(dmx.seek_to(0, target).unwrap(), key, "seek {target}");
            let rest = drain(&mut dmx);
            assert_eq!(rest, want[key as usize..], "packets after seek {target}");
        }
        // The strict standard-index seek agrees wherever the file has one.
        if !matches!(kind, AviKind::Avi10) {
            let r = dmx.seek_to_keyframe_strict_via_std_index(0, 21).unwrap();
            assert_eq!(r.landed_pts, 16);
            assert_eq!(drain(&mut dmx), want[16..]);
        }
    }
}

/// The idx1-only helper never lands on a side-band chunk or an empty slot.
#[test]
fn first_video_keyframe_after_skips_side_band_and_empty_slots() {
    let frames = [(20, true), (21, false), (0, true), (23, false), (24, true)];
    let bytes = mux(&frames, AviKind::Avi10, Side::PaletteAndText);
    let mut dmx = open(&bytes);
    let r = dmx.seek_to_first_video_keyframe_after(0, 1).unwrap();
    assert_eq!(r.landed_pts, 4);
    assert_eq!(drain(&mut dmx), [(4, 24, true)]);
    // A palette change's position is the count of packets returned
    // before it: the empty slot is not one of them.
    assert_eq!(dmx.palette_change_packet_positions(0), [1, 2, 2, 3, 4]);
}
