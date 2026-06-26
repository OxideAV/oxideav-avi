//! Round-373: `AVISF_VIDEO_PALCHANGES` flag ↔ `xxpc`-presence cross-check.
//!
//! Per AVI 1.0 §"Stream Data ('movi' List)"
//! (`docs/container/riff/avi-riff-file-reference.md`): *"If a stream
//! contains palette changes, set the AVISF_VIDEO_PALCHANGES flag in the
//! dwFlags member of the AVISTREAMHEADER structure for that stream."*
//!
//! `AviDemuxer::palette_change_flag_violations()` returns one
//! `PaletteChangeFlagViolation` per video stream whose flag disagrees
//! with whether it actually carries `xxpc` chunks. Informational —
//! never fails `open()`. The muxer's `write_palette_change` does not set
//! the flag itself, so a caller wanting a conforming file pairs it with
//! `with_stream_flags(stream, AVISF_VIDEO_PALCHANGES)`.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, MediaType, Muxer, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{open_avi as demuxer_open_avi, AVISF_VIDEO_PALCHANGES};
use oxideav_avi::muxer::{open_avi as muxer_open_avi, AviKind, AviMuxOptions};

fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG"));
    reg.register(info);
    reg
}

fn magicyuv_stream(width: u32, height: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn synth_payload(seed: u32, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..n {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

/// Mux `n_frames` video keyframes with `n_pc` palette-change chunks
/// interleaved, using the supplied options.
fn mux(
    path: &std::path::Path,
    stream: &StreamInfo,
    n_frames: usize,
    n_pc: usize,
    opts: AviMuxOptions,
) {
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = muxer_open_avi(ws, std::slice::from_ref(stream), AviKind::Avi10, opts).unwrap();
    mux.write_header().unwrap();
    // bFirstEntry=0, bNumEntries=2, wFlags=0, two RGBQUAD-ish quads.
    let pal = [0u8, 2, 0, 0, 0xFF, 0, 0, 0, 0, 0xFF, 0, 0];
    for i in 0..n_frames {
        let payload = synth_payload(i as u32 + 1, 64);
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        if i < n_pc {
            mux.write_palette_change(0, &pal).unwrap();
        }
    }
    mux.write_trailer().unwrap();
}

#[test]
fn xxpc_present_without_flag_is_a_violation() {
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();

    // 3 frames, 2 xxpc chunks, but NO AVISF_VIDEO_PALCHANGES flag set.
    let tmp = std::env::temp_dir().join("oxideav-avi-r373-palflag-missing.avi");
    mux(&tmp, &stream, 3, 2, AviMuxOptions::new());

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.palette_change_count(0), 2);

    let v = dmx.palette_change_flag_violations();
    assert_eq!(v.len(), 1, "missing flag must be one violation");
    assert_eq!(v[0].stream_index, 0);
    assert!(!v[0].flag_set);
    assert_eq!(v[0].palette_change_chunks, 2);
}

#[test]
fn xxpc_present_with_flag_is_conforming() {
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();

    // Same file but the caller sets AVISF_VIDEO_PALCHANGES — conforming.
    let tmp = std::env::temp_dir().join("oxideav-avi-r373-palflag-set.avi");
    let opts = AviMuxOptions::new().with_stream_flags(0, AVISF_VIDEO_PALCHANGES);
    mux(&tmp, &stream, 3, 2, opts);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.palette_change_count(0), 2);
    assert!(
        dmx.palette_change_flag_violations().is_empty(),
        "flag set + xxpc present must be conforming"
    );
    // Confirm the flag round-tripped.
    let flags = dmx.stream_flags(0).unwrap();
    assert_ne!(flags & AVISF_VIDEO_PALCHANGES, 0);
}

#[test]
fn flag_set_without_xxpc_is_a_violation() {
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();

    // Flag set but NO xxpc chunks emitted — a spurious hint.
    let tmp = std::env::temp_dir().join("oxideav-avi-r373-palflag-spurious.avi");
    let opts = AviMuxOptions::new().with_stream_flags(0, AVISF_VIDEO_PALCHANGES);
    mux(&tmp, &stream, 3, 0, opts);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.palette_change_count(0), 0);

    let v = dmx.palette_change_flag_violations();
    assert_eq!(v.len(), 1, "spurious flag must be one violation");
    assert!(v[0].flag_set);
    assert_eq!(v[0].palette_change_chunks, 0);
}

#[test]
fn no_flag_no_xxpc_is_conforming() {
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();

    // Neither flag nor xxpc — the common case, conforming.
    let tmp = std::env::temp_dir().join("oxideav-avi-r373-palflag-none.avi");
    mux(&tmp, &stream, 3, 0, AviMuxOptions::new());

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.palette_change_count(0), 0);
    assert!(dmx.palette_change_flag_violations().is_empty());
}
