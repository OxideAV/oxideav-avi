//! Integration tests for the AVI demuxer using ffmpeg-generated reference
//! files. These tests require `/usr/bin/ffmpeg`; they are skipped gracefully
//! if the binary or the reference file is missing.

use std::path::Path;
use std::process::Command;

use oxideav_core::{CodecId, CodecInfo, CodecRegistry, CodecTag, ReadSeek};

/// Build a registry with a synthetic `MJPG` ↔ `"mjpeg"` tag claim
/// so the resolver surfaces `mjpeg` for the FOURCC. Avoids a
/// producer-crate dev-dep — real MJPEG decode coverage lives in
/// `crates/oxideav-tests`. ffv1 is intentionally not registered;
/// the `avi:FFV1` synthetic fallback is the test-correct
/// expectation in that case.
fn registry_with_mjpeg() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("mjpeg")).tag(CodecTag::fourcc(b"MJPG"));
    reg.register(info);
    reg
}

const FFMPEG: &str = "/usr/bin/ffmpeg";

fn ffmpeg_available() -> bool {
    Path::new(FFMPEG).exists()
}

fn ensure_ref_avi(path: &str, args: &[&str]) -> bool {
    if !ffmpeg_available() {
        return false;
    }
    if Path::new(path).exists() {
        return true;
    }
    let status = Command::new(FFMPEG)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .arg(path)
        .status();
    matches!(status, Ok(s) if s.success()) && Path::new(path).exists()
}

#[test]
fn demux_ffmpeg_mjpeg_avi() {
    let path = "/tmp/ref-avi-mjpeg.avi";
    let ok = ensure_ref_avi(
        path,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=64x64:r=10",
            "-c:v",
            "mjpeg",
        ],
    );
    if !ok {
        eprintln!("skip: ffmpeg not available or could not produce {path}");
        return;
    }

    let reg = registry_with_mjpeg();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(path).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).expect("AVI demuxer open");
    assert_eq!(dmx.format_name(), "avi");
    let streams = dmx.streams().to_vec();
    assert_eq!(streams.len(), 1, "expected one stream");
    assert_eq!(streams[0].params.codec_id.as_str(), "mjpeg");
    assert_eq!(streams[0].params.width, Some(64));
    assert_eq!(streams[0].params.height, Some(64));
    let fr = streams[0].params.frame_rate.expect("frame_rate");
    let approx_fps = fr.num as f64 / fr.den as f64;
    assert!(
        (approx_fps - 10.0).abs() < 0.1,
        "expected ~10 fps, got {approx_fps}"
    );

    let first = dmx.next_packet().expect("at least one packet");
    assert!(first.data.len() >= 2);
    assert_eq!(
        &first.data[0..2],
        &[0xFF, 0xD8],
        "MJPEG packet must start with JPEG SOI"
    );
}

#[test]
fn demux_ffmpeg_ffv1_avi() {
    let path = "/tmp/ref-avi-ffv1.avi";
    let ok = ensure_ref_avi(
        path,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=64x64:r=10",
            "-c:v",
            "ffv1",
            "-level",
            "3",
        ],
    );
    if !ok {
        eprintln!("skip: ffmpeg not available or could not produce {path}");
        return;
    }

    // ffv1 isn't registered as a dev-dep here, so the resolver
    // surfaces the synthetic `avi:FFV1` placeholder. Pipelines that
    // include `oxideav-ffv1` would resolve this to `ffv1` via the
    // registry; this test asserts the placeholder fallback shape.
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(path).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).expect("AVI demuxer open");
    let streams = dmx.streams().to_vec();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].params.codec_id.as_str(), "avi:FFV1");
    assert_eq!(streams[0].params.width, Some(64));
    assert_eq!(streams[0].params.height, Some(64));

    let _pkt = dmx.next_packet().expect("at least one packet");
}

#[test]
fn demux_ffmpeg_av_avi() {
    let path = "/tmp/ref-avi-av.avi";
    let ok = ensure_ref_avi(
        path,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=64x64:r=10",
            "-f",
            "lavfi",
            "-i",
            "sine=f=440:d=1",
            "-c:v",
            "mjpeg",
            "-c:a",
            "pcm_s16le",
        ],
    );
    if !ok {
        eprintln!("skip: ffmpeg not available or could not produce {path}");
        return;
    }

    let reg = registry_with_mjpeg();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(path).unwrap());
    let dmx = oxideav_avi::demuxer::open(rs, &reg).expect("AVI demuxer open");
    let streams = dmx.streams();
    assert_eq!(streams.len(), 2, "expected video + audio stream");
    // Declaration order: ffmpeg writes video first, then audio.
    assert_eq!(streams[0].params.codec_id.as_str(), "mjpeg");
    // pcm_s16le falls through the demuxer's depth-aware
    // WAVE_FORMAT_PCM synthesiser; the codec crate doesn't need to
    // be registered for this to work.
    assert_eq!(streams[1].params.codec_id.as_str(), "pcm_s16le");
    assert_eq!(streams[1].params.sample_rate, Some(44_100));
}
