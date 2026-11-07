//! Round-trip matrix: every codec id the muxer accepts — mux → demux →
//! the same codec id, stream parameters, packet bytes and timestamps.
//!
//! Streams are built the way an encoder states them (codec id + shape,
//! no container tag) unless the codec has no canonical tag, in which
//! case `params.tag` is set the way an encoder's `output_params` does.
//! The demuxer resolves tags through a registry holding the same
//! codec-id ↔ tag claims the codec crates register.
//!
//! Clean-room sources: `docs/container/riff/avi-riff-file-reference.md`,
//! `docs/container/riff/rfc2361-wav.txt`, `docs/container/vfw/`.

use std::io::Cursor;

use oxideav_avi::demuxer::open_avi;
use oxideav_avi::muxer::{AviKind, AviMuxOptions, open_with_options};
use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, MediaType,
    Packet, PixelFormat, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

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

/// Untagged video codec id → the FourCC the muxer writes for it.
const VIDEO: &[(&str, &[u8; 4])] = &[
    ("mjpeg", b"MJPG"),
    ("cinepak", b"CVID"),
    ("h261", b"H261"),
    ("h263", b"H263"),
    ("indeo3", b"IV32"),
    ("indeo5", b"IV50"),
    ("huffyuv", b"HFYU"),
    ("lagarith", b"LAGS"),
    ("msmpeg4v1", b"MPG4"),
    ("msmpeg4v2", b"MP42"),
    ("msmpeg4v3", b"MP43"),
    ("mpeg4video", b"XVID"),
    ("vp6", b"VP62"),
    ("h264", b"H264"),
    ("h265", b"HEVC"),
    ("vp8", b"VP80"),
    ("vp9", b"VP90"),
    ("av1", b"AV01"),
    ("ffv1", b"FFV1"),
    ("mpeg1video", b"MPG1"),
    ("mpeg2video", b"MPG2"),
    ("svq1", b"SVQ1"),
];

/// Video codecs written only with an explicit FourCC tag.
const TAGGED_VIDEO: &[(&str, &[u8; 4])] = &[
    ("magicyuv", b"M8RG"),
    ("utvideo", b"ULY2"),
    ("prores", b"APCN"),
    ("wmv2", b"WMV2"),
];

/// Untagged compressed audio codec id → its `wFormatTag`.
const AUDIO: &[(&str, u16)] = &[
    ("pcm_alaw", 0x0006),
    ("pcm_mulaw", 0x0007),
    ("mp1", 0x0050),
    ("mp2", 0x0050),
    ("mp3", 0x0055),
];

/// Audio codecs written only with an explicit `wFormatTag` tag.
const TAGGED_AUDIO: &[(&str, u16)] = &[
    ("aac", 0x00FF),
    ("ac3", 0x2000),
    ("flac", 0xF1AC),
    ("adpcm_ima_wav", 0x0011),
];

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    for (id, f) in VIDEO.iter().chain(TAGGED_VIDEO) {
        reg.register(CodecInfo::new(CodecId::new(*id)).tag(CodecTag::fourcc(f)));
    }
    for (id, t) in AUDIO.iter().chain(TAGGED_AUDIO) {
        // mp1 and mp2 share 0x0050: register only mp2's claim.
        if *id == "mp1" {
            continue;
        }
        reg.register(CodecInfo::new(CodecId::new(*id)).tag(CodecTag::wave_format(*t)));
    }
    reg
}

/// One packet to write: payload, pts, dts, duration, keyframe.
#[derive(Clone, Debug)]
struct Pkt {
    data: Vec<u8>,
    pts: i64,
    duration: Option<i64>,
    key: bool,
}

/// Payload bytes that no parameter-set probe can mistake for a header
/// (all ≥ 0xC0: no start codes, no valid OBU / VP9 frame marker).
fn opaque(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| 0xC0 | ((i * 7 + seed * 13) as u8 & 0x3F))
        .collect()
}

fn video_params(codec: &str) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new(codec));
    params.width = Some(64);
    params.height = Some(48);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn audio_params(codec: &str, rate: u32, channels: u16) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new(codec));
    params.sample_rate = Some(rate);
    params.channels = Some(channels);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, i64::from(rate)),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn video_packets(all_key: bool) -> Vec<Pkt> {
    (0..6)
        .map(|i| Pkt {
            data: opaque(i, 50 + 3 * i + (i & 1)),
            pts: i as i64,
            duration: Some(1),
            key: all_key || i % 3 == 0,
        })
        .collect()
}

/// Write `stream` with `pkts` and read it back.
fn round_trip(stream: &StreamInfo, pkts: &[Pkt], kind: AviKind) -> (StreamInfo, Vec<Packet>) {
    let buf = SharedBuf::default();
    let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
    let mut m = open_with_options(
        ws,
        std::slice::from_ref(stream),
        kind,
        AviMuxOptions::default(),
    )
    .unwrap_or_else(|e| panic!("{}: open: {e}", stream.params.codec_id));
    m.write_header().unwrap();
    for p in pkts {
        let mut pkt = Packet::new(0, stream.time_base, p.data.clone());
        pkt.pts = Some(p.pts);
        pkt.dts = Some(p.pts);
        pkt.duration = p.duration;
        pkt.flags.keyframe = p.key;
        m.write_packet(&pkt).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = buf.0.lock().unwrap().get_ref().clone();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = open_avi(rs, &registry()).unwrap();
    let info = dmx.streams()[0].clone();
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => break,
            Err(e) => panic!("{}: next_packet: {e}", stream.params.codec_id),
        }
    }
    (info, out)
}

/// A timestamp in seconds, exact as a rational `(num, den)`.
fn seconds(tb: TimeBase, ts: i64) -> (i128, i128) {
    let r = tb.as_rational();
    let (n, d) = (i128::from(ts) * i128::from(r.num), i128::from(r.den));
    let g = gcd(n.abs(), d.abs()).max(1);
    (n / g, d / g)
}

fn gcd(a: i128, b: i128) -> i128 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn check_packets(name: &str, sent_tb: TimeBase, sent: &[Pkt], got: &[Packet]) {
    assert_eq!(got.len(), sent.len(), "{name}: packet count");
    for (i, (s, g)) in sent.iter().zip(got).enumerate() {
        assert_eq!(g.data, s.data, "{name}: packet {i} bytes");
        let pts = g
            .pts
            .unwrap_or_else(|| panic!("{name}: packet {i} has no pts"));
        assert_eq!(
            seconds(g.time_base, pts),
            seconds(sent_tb, s.pts),
            "{name}: packet {i} pts {pts} in {:?}",
            g.time_base
        );
        assert_eq!(g.flags.keyframe, s.key, "{name}: packet {i} keyframe");
    }
}

const KINDS: [AviKind; 2] = [
    AviKind::Avi10,
    AviKind::OpenDml(oxideav_avi::muxer::RiffSegmentLimit::OneGiB),
];

#[test]
fn untagged_video_codecs_round_trip() {
    for kind in KINDS {
        for &(codec, fourcc) in VIDEO {
            let stream = video_params(codec);
            let pkts = video_packets(codec == "mjpeg");
            let (info, got) = round_trip(&stream, &pkts, kind);
            let p = &info.params;
            let name = format!("{codec} {kind:?}");
            assert_eq!(p.codec_id.as_str(), codec, "{name}");
            assert_eq!(p.media_type, MediaType::Video, "{name}");
            assert_eq!(p.tag, Some(CodecTag::fourcc(fourcc)), "{name}");
            assert_eq!((p.width, p.height), (Some(64), Some(48)), "{name}");
            assert_eq!(p.frame_rate, Some(Rational::new(25, 1)), "{name}");
            check_packets(&name, stream.time_base, &pkts, &got);
        }
    }
}

#[test]
fn tagged_video_codecs_round_trip() {
    for &(codec, fourcc) in TAGGED_VIDEO {
        let mut stream = video_params(codec);
        // Without a tag there is no single FourCC to pick.
        assert!(
            open_with_options(
                Box::new(SharedBuf::default()),
                std::slice::from_ref(&stream),
                AviKind::Avi10,
                AviMuxOptions::default(),
            )
            .is_err(),
            "{codec}: untagged must be refused"
        );
        stream.params.tag = Some(CodecTag::fourcc(fourcc));
        let pkts = video_packets(false);
        let (info, got) = round_trip(&stream, &pkts, AviKind::Avi10);
        assert_eq!(info.params.codec_id.as_str(), codec);
        assert_eq!(info.params.tag, Some(CodecTag::fourcc(fourcc)));
        check_packets(codec, stream.time_base, &pkts, &got);
    }
}

#[test]
fn rawvideo_pixel_formats_round_trip() {
    for (fmt, bpp) in [
        (PixelFormat::Bgr24, 3),
        (PixelFormat::Bgrx, 4),
        (PixelFormat::Rgb555Le, 2),
        (PixelFormat::Rgb565Le, 2),
        (PixelFormat::Rgba, 4),
    ] {
        // Odd width: DIB rows need DWORD padding on the wire.
        let (w, h) = (5u32, 3u32);
        let mut stream = video_params("rawvideo");
        stream.params.width = Some(w);
        stream.params.height = Some(h);
        stream.params.pixel_format = Some(fmt);
        let frame = (w * h * bpp) as usize;
        let pkts: Vec<Pkt> = (0..3)
            .map(|i| Pkt {
                data: (0..frame).map(|b| (b * 5 + i * 17) as u8).collect(),
                pts: i as i64,
                duration: Some(1),
                key: true,
            })
            .collect();
        let (info, got) = round_trip(&stream, &pkts, AviKind::Avi10);
        let name = format!("rawvideo {fmt:?}");
        assert_eq!(info.params.codec_id.as_str(), "rawvideo", "{name}");
        assert_eq!(info.params.pixel_format, Some(fmt), "{name}");
        assert_eq!((info.params.width, info.params.height), (Some(w), Some(h)));
        check_packets(&name, stream.time_base, &pkts, &got);
    }
}

#[test]
fn pcm_codecs_round_trip() {
    for (codec, fmt, bytes) in [
        ("pcm_u8", SampleFormat::U8, 1),
        ("pcm_s16le", SampleFormat::S16, 2),
        ("pcm_s24le", SampleFormat::S24, 3),
        ("pcm_s32le", SampleFormat::S32, 4),
        ("pcm_f32le", SampleFormat::F32, 4),
        ("pcm_f64le", SampleFormat::F64, 8),
    ] {
        let mut stream = audio_params(codec, 8000, 2);
        stream.params.sample_format = Some(fmt);
        let mut t = 0;
        let pkts: Vec<Pkt> = (0..4)
            .map(|i| {
                let frames = 100 + 20 * i;
                let p = Pkt {
                    data: (0..frames * 2 * bytes).map(|b| (b + i) as u8).collect(),
                    pts: t,
                    duration: Some(frames as i64),
                    key: true,
                };
                t += frames as i64;
                p
            })
            .collect();
        for kind in KINDS {
            let (info, got) = round_trip(&stream, &pkts, kind);
            let name = format!("{codec} {kind:?}");
            assert_eq!(info.params.codec_id.as_str(), codec, "{name}");
            assert_eq!(info.params.sample_rate, Some(8000), "{name}");
            assert_eq!(info.params.channels, Some(2), "{name}");
            assert_eq!(info.params.sample_format, Some(fmt), "{name}");
            check_packets(&name, stream.time_base, &pkts, &got);
        }
    }
}

/// Compressed audio: one codec frame per chunk, each `frame` samples.
fn frames_of(frame: i64, n: usize) -> Vec<Pkt> {
    (0..n)
        .map(|i| Pkt {
            data: opaque(i, 96 + 5 * i),
            pts: i as i64 * frame,
            duration: Some(frame),
            key: true,
        })
        .collect()
}

fn samples_per_frame(codec: &str) -> i64 {
    match codec {
        "mp1" => 384,
        "mp2" | "mp3" => 1152,
        "aac" => 1024,
        "ac3" => 1536,
        "flac" => 4096,
        "adpcm_ima_wav" => 505,
        // G.711: one byte per sample per channel, frames of 160.
        _ => 160,
    }
}

#[test]
fn compressed_audio_codecs_round_trip() {
    for &(codec, tag) in AUDIO.iter().chain(TAGGED_AUDIO) {
        let mut stream = audio_params(codec, 44_100, 2);
        if TAGGED_AUDIO.iter().any(|t| t.0 == codec) {
            stream.params.tag = Some(CodecTag::wave_format(tag));
        }
        let spf = samples_per_frame(codec);
        let pkts = if codec.starts_with("pcm_") {
            // Companded PCM is CBR: a chunk is `samples × channels` bytes.
            let mut t = 0;
            (0..4)
                .map(|i| {
                    let n = 160 + 40 * i;
                    let p = Pkt {
                        data: (0..n * 2).map(|b| (b * 3 + i) as u8).collect(),
                        pts: t,
                        duration: Some(n as i64),
                        key: true,
                    };
                    t += n as i64;
                    p
                })
                .collect()
        } else {
            frames_of(spf, 5)
        };
        for kind in KINDS {
            let (info, got) = round_trip(&stream, &pkts, kind);
            let name = format!("{codec} {kind:?}");
            let want_id = if codec == "mp1" { "mp2" } else { codec };
            assert_eq!(info.params.codec_id.as_str(), want_id, "{name}");
            assert_eq!(info.params.tag, Some(CodecTag::wave_format(tag)), "{name}");
            assert_eq!(info.params.sample_rate, Some(44_100), "{name}");
            assert_eq!(info.params.channels, Some(2), "{name}");
            check_packets(&name, stream.time_base, &pkts, &got);
        }
    }
}

/// Black-box: `ffprobe` reads a compressed-audio stream's chunk tick as
/// one codec frame (1152 / 44100 = 32 / 1225 s) and its length as the
/// frame count. Skipped when `ffprobe` is absent.
#[test]
fn ffprobe_reads_the_compressed_audio_frame_tick() {
    let stream = audio_params("mp2", 44_100, 2);
    let pkts = frames_of(1152, 5);
    let buf = SharedBuf::default();
    let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
    let mut m = open_with_options(
        ws,
        std::slice::from_ref(&stream),
        AviKind::Avi10,
        AviMuxOptions::default(),
    )
    .unwrap();
    m.write_header().unwrap();
    for p in &pkts {
        let mut pkt = Packet::new(0, stream.time_base, p.data.clone());
        pkt.pts = Some(p.pts);
        pkt.duration = p.duration;
        m.write_packet(&pkt).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let path = std::env::temp_dir().join("oxideav-avi-matrix-mp2-ffprobe.avi");
    std::fs::write(&path, buf.0.lock().unwrap().get_ref()).unwrap();
    let out = match std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-show_entries",
            "stream=codec_name,time_base,nb_frames",
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
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "mp2,32/1225,5");
}

#[test]
fn interleaved_video_and_audio_streams_round_trip() {
    let mut v = video_params("mjpeg");
    v.index = 0;
    let mut a = audio_params("mp3", 48_000, 2);
    a.index = 1;
    let mut pcm = audio_params("pcm_s16le", 48_000, 1);
    pcm.params.sample_format = Some(SampleFormat::S16);
    pcm.index = 2;
    let streams = [v.clone(), a.clone(), pcm.clone()];
    // 25 fps video, 1152-sample mp3 frames, 1920-sample PCM chunks.
    let vp = video_packets(true);
    let ap = frames_of(1152, 5);
    let pp: Vec<Pkt> = (0..3)
        .map(|i| Pkt {
            data: vec![i as u8; 1920 * 2],
            pts: i as i64 * 1920,
            duration: Some(1920),
            key: true,
        })
        .collect();
    for kind in KINDS {
        let buf = SharedBuf::default();
        let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
        let mut m = open_with_options(ws, &streams, kind, AviMuxOptions::default()).unwrap();
        m.write_header().unwrap();
        let n = vp.len().max(ap.len()).max(pp.len());
        for i in 0..n {
            for (s, list) in [(0usize, &vp), (1, &ap), (2, &pp)] {
                if let Some(p) = list.get(i) {
                    let mut pkt = Packet::new(s as u32, streams[s].time_base, p.data.clone());
                    pkt.pts = Some(p.pts);
                    pkt.dts = Some(p.pts);
                    pkt.duration = p.duration;
                    pkt.flags.keyframe = p.key;
                    m.write_packet(&pkt).unwrap();
                }
            }
        }
        m.write_trailer().unwrap();
        drop(m);
        let bytes = buf.0.lock().unwrap().get_ref().clone();
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
        let mut dmx = open_avi(rs, &registry()).unwrap();
        let ids: Vec<&str> = dmx
            .streams()
            .iter()
            .map(|s| s.params.codec_id.as_str())
            .collect();
        assert_eq!(ids, ["mjpeg", "mp3", "pcm_s16le"], "{kind:?}");
        let mut got: [Vec<Packet>; 3] = Default::default();
        loop {
            match dmx.next_packet() {
                Ok(p) => got[p.stream_index as usize].push(p),
                Err(Error::Eof) => break,
                Err(e) => panic!("{e}"),
            }
        }
        check_packets(&format!("video {kind:?}"), v.time_base, &vp, &got[0]);
        check_packets(&format!("mp3 {kind:?}"), a.time_base, &ap, &got[1]);
        check_packets(&format!("pcm {kind:?}"), pcm.time_base, &pp, &got[2]);
        // Seeking video to 0.12 s lands on frame 3 (all-key MJPEG); the
        // audio streams resume at the chunk interleaved there.
        assert_eq!(dmx.seek_to(0, 3).unwrap(), 3, "{kind:?}");
    }
}
