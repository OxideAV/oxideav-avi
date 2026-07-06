//! Round 394 — black-box validation of the round-394 writer shapes
//! against ffmpeg/ffprobe as an opaque referee (skipped gracefully
//! when the binaries are missing).
//!
//! ffmpeg is used strictly as a black-box conformance oracle — no
//! implementation details are consulted, only its observable demux
//! behaviour on our writer's output:
//! * multi-segment OpenDML with spec-correct super-index targets,
//! * per-stream `indx` (video + audio),
//! * the compact in-`strl` standard index,
//! * first-class `txts` text streams,
//!
//! and one demux + remux round-trip over an ffmpeg-encoded source.

use std::path::PathBuf;
use std::process::Command;

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer as _, MediaType,
    Muxer as _, Packet, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};

/// Locate a binary across the usual install prefixes (macOS Homebrew,
/// /usr/local, distro /usr/bin) — the older `avi_ffmpeg.rs` suite
/// hardcodes `/usr/bin/ffmpeg` and silently skips everywhere else.
fn find_bin(name: &str) -> Option<PathBuf> {
    for prefix in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        let p = PathBuf::from(prefix).join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// `ffprobe -count_packets` per stream: returns
/// `(codec_type, nb_read_packets)` per stream in index order, or
/// `None` when ffprobe is unavailable / fails.
fn ffprobe_streams(path: &std::path::Path) -> Option<Vec<(String, u64)>> {
    let ffprobe = find_bin("ffprobe")?;
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-count_packets",
            "-show_entries",
            "stream=codec_type,nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut v = Vec::new();
    for line in text.lines() {
        let mut it = line.trim().split(',');
        let kind = it.next()?.to_string();
        let count: u64 = it.next()?.trim().parse().ok()?;
        v.push((kind, count));
    }
    Some(v)
}

fn video_stream(index: u32) -> StreamInfo {
    // MJPG FourCC so ffprobe recognises the stream kind; payloads are
    // opaque (no decode is requested — `-count_packets` only demuxes).
    let mut params =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(64);
    params.height = Some(64);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn audio_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn payload(seed: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed.wrapping_mul(0x9E37_79B9).wrapping_add(23);
    for _ in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 24) as u8);
    }
    out
}

fn write_av_file(path: &std::path::Path, n: usize, kind: AviKind, opts: AviMuxOptions) {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = oxideav_avi::muxer::open_avi(ws, &streams, kind, opts).unwrap();
    mux.write_header().unwrap();
    for i in 0..n {
        let mut v = Packet::new(0, streams[0].time_base, payload(i as u32, 600));
        v.pts = Some(i as i64);
        v.flags.keyframe = true;
        mux.write_packet(&v).unwrap();
        let mut a = Packet::new(1, streams[1].time_base, payload(0xC000 + i as u32, 192));
        a.pts = Some(i as i64 * 48);
        a.flags.keyframe = true;
        mux.write_packet(&a).unwrap();
    }
    mux.write_trailer().unwrap();
}

#[test]
fn ffprobe_walks_multi_segment_opendml_with_per_stream_indx() {
    if find_bin("ffprobe").is_none() {
        eprintln!("skip: ffprobe not found");
        return;
    }
    let tmp = std::env::temp_dir().join("oxideav-avi-r394-bb-odml.avi");
    // 4 KiB ceiling → several RIFF AVIX segments; every stream carries
    // its own indx whose entries point at the ix## chunks.
    write_av_file(
        &tmp,
        24,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(4 * 1024)),
        AviMuxOptions::default(),
    );
    let streams = ffprobe_streams(&tmp).expect("ffprobe parses our OpenDML output");
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(streams.len(), 2, "both streams visible");
    assert_eq!(streams[0].0, "video");
    assert_eq!(streams[1].0, "audio");
    assert_eq!(
        streams[0].1, 24,
        "every video packet across AVIX segments reachable"
    );
    assert_eq!(streams[1].1, 24, "every audio packet reachable");
}

#[test]
fn ffprobe_walks_in_strl_std_index_file() {
    if find_bin("ffprobe").is_none() {
        eprintln!("skip: ffprobe not found");
        return;
    }
    let tmp = std::env::temp_dir().join("oxideav-avi-r394-bb-sstd.avi");
    write_av_file(
        &tmp,
        10,
        AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        AviMuxOptions::default().with_strl_std_index(64),
    );
    let streams = ffprobe_streams(&tmp).expect("ffprobe parses the compact-index output");
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].1, 10);
    assert_eq!(streams[1].1, 10);
}

#[test]
fn ffprobe_sees_txts_stream_as_subtitle() {
    if find_bin("ffprobe").is_none() {
        eprintln!("skip: ffprobe not found");
        return;
    }
    let streams = [video_stream(0), {
        let codec_id = CodecId::new("avi:txts");
        let mut params = CodecParameters::audio(codec_id);
        params.media_type = MediaType::Subtitle;
        StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params,
        }
    }];
    let tmp = std::env::temp_dir().join("oxideav-avi-r394-bb-txts.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux =
            oxideav_avi::muxer::open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::default())
                .unwrap();
        mux.write_header().unwrap();
        for i in 0..5 {
            let mut v = Packet::new(0, streams[0].time_base, payload(i, 400));
            v.pts = Some(i as i64);
            v.flags.keyframe = true;
            mux.write_packet(&v).unwrap();
            let mut t = Packet::new(1, streams[1].time_base, format!("line {i}").into_bytes());
            t.pts = Some(i as i64 * 500);
            t.flags.keyframe = true;
            mux.write_packet(&t).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let probed = ffprobe_streams(&tmp).expect("ffprobe parses the txts output");
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(probed.len(), 2, "text stream visible as a stream");
    assert_eq!(probed[0].0, "video");
    assert_eq!(
        probed[1].0, "subtitle",
        "ffprobe classifies the txts stream as subtitle"
    );
    assert_eq!(probed[1].1, 5, "all text packets reachable");
}

#[test]
fn remux_of_ffmpeg_source_validates_with_ffprobe() {
    let (Some(ffmpeg), Some(_)) = (find_bin("ffmpeg"), find_bin("ffprobe")) else {
        eprintln!("skip: ffmpeg/ffprobe not found");
        return;
    };
    // 1. ffmpeg encodes a reference AVI (mjpeg + pcm_s16le).
    let src = std::env::temp_dir().join("oxideav-avi-r394-bb-src.avi");
    let status = Command::new(&ffmpeg)
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=64x64:r=10",
            "-f",
            "lavfi",
            "-i",
            "sine=d=1:r=48000",
            "-c:v",
            "mjpeg",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&src)
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        eprintln!("skip: ffmpeg could not produce the reference file");
        return;
    }

    // 2. Our demuxer reads it; our OpenDML muxer rewrites it.
    let reg = {
        let mut reg = CodecRegistry::new();
        reg.register(CodecInfo::new(CodecId::new("mjpeg")).tag(CodecTag::fourcc(b"MJPG")));
        reg
    };
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(std::fs::read(&src).unwrap()));
    let mut dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();
    let in_streams: Vec<StreamInfo> = dmx.streams().to_vec();
    let dst = std::env::temp_dir().join("oxideav-avi-r394-bb-remux.avi");
    {
        let f = std::fs::File::create(&dst).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(
            ws,
            &in_streams,
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::default(),
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut counts = vec![0u64; in_streams.len()];
        loop {
            match dmx.next_packet() {
                Ok(p) => {
                    counts[p.stream_index as usize] += 1;
                    mux.write_packet(&p).unwrap();
                }
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("demux error: {e}"),
            }
        }
        mux.write_trailer().unwrap();
        assert!(counts.iter().all(|&c| c > 0), "both streams remuxed");

        // 3. ffprobe agrees the remux carries the same packet counts.
        let probed = ffprobe_streams(&dst).expect("ffprobe parses the remux");
        assert_eq!(probed.len(), in_streams.len());
        for (i, (_, n)) in probed.iter().enumerate() {
            assert_eq!(
                *n, counts[i],
                "stream {i}: remux packet count matches what we wrote"
            );
        }
    }
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}
