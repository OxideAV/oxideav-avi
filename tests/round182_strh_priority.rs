//! Round-182 per-stream `strh.wPriority` AVI tests.
//!
//! `wPriority` is the 16-bit DWORD at byte offset 12 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, Appendix B
//! `wPriority` row, line 238): *"Priority of a stream type. For
//! example, in a file with multiple audio streams, the one with the
//! highest priority might be the default stream."*
//!
//! The spec describes the field as a selection hint among
//! same-`fccType` streams (the file with several audio streams picking
//! a default-playback one), not a sortable global priority. It does
//! not normatively pin a value range or a tie-break rule, so the
//! demuxer surfaces the raw 16-bit DWORD verbatim and the muxer writes
//! whatever 16-bit value the caller supplies — applications that use
//! the field for ad-hoc tagging round-trip exactly.
//!
//! `0` is the legacy writer default (the muxer has stamped a zero
//! priority since round-3) and maps to `None` so an unspecified
//! priority reads the same as an absent one, mirroring the
//! round-176 `dwQuality` / round-153 `dwInitialFrames` / round-119
//! `wLanguage` / round-115 `rcFrame` / round-80 `strn` / round-107
//! `IDIT` "default == absent" convention.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip** of a non-default per-stream priority
//!   via the typed accessor and the metadata key.
//! - **No-override baseline**: with no override, the muxer keeps the
//!   `wPriority = 0` default which the demuxer maps to `None` and the
//!   metadata-key loop omits.
//! - **Builder idempotency**: the last `with_stream_priority(...)`
//!   wins per stream index.
//! - **Explicit `0` override** reads back as `None` (default ==
//!   absent).
//! - **Boundary values** (`1`, `u16::MAX`) round-trip verbatim — the
//!   spec does not pin a range so neither extreme is special-cased.
//! - **Independence across streams**: priority on stream 1 doesn't
//!   perturb stream 0's `None`, and vice versa.
//! - **Independence from sibling DWORDs**: stamping `wPriority`
//!   doesn't perturb `dwQuality` / `dwInitialFrames` / `wLanguage`
//!   readbacks.
//! - **Hand-rolled fixtures**: an explicit non-zero `wPriority` in a
//!   56-byte strh decodes to the expected raw u16; an all-zeros
//!   `wPriority` parses as `None`.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer, Packet, Rational,
    ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn video_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(64);
    params.height = Some(48);
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
    params.media_type = MediaType::Audio;
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

fn write_minimal(path: &std::path::Path, options: AviMuxOptions) {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();

    let mut v = Packet::new(0, streams[0].time_base, vec![0x55u8; 64]);
    v.pts = Some(0);
    v.flags.keyframe = true;
    mux.write_packet(&v).unwrap();

    let mut a = Packet::new(1, streams[1].time_base, vec![0u8; 8]);
    a.pts = Some(0);
    a.flags.keyframe = true;
    mux.write_packet(&a).unwrap();

    mux.write_trailer().unwrap();
}

// ---------------------------------------------------------------------------
// Round-trip: a non-default per-stream priority survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn strh_priority_override_roundtrip_accessor_and_metadata() {
    // `2` is a typical "secondary" priority pick when two audio
    // streams in a file want to disambiguate which is the default —
    // the spec's own usage illustration ("in a file with multiple
    // audio streams, the one with the highest priority might be the
    // default stream").
    let tmp = std::env::temp_dir().join("oxideav-avi-r182-strh-priority.avi");
    let opts = AviMuxOptions::new().with_stream_priority(1, 2);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_priority(1), Some(2));
    // Stream 0 keeps the legacy zero ⇒ reads as None.
    assert_eq!(dmx.stream_priority(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter()
            .any(|(k, v)| k == "avi:strh.1.priority" && v == "2"),
        "missing avi:strh.1.priority metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k.contains("priority"))
            .collect::<Vec<_>>()
    );
    // Stream 0's key must not surface.
    assert!(!md.iter().any(|(k, _)| k == "avi:strh.0.priority"));
}

// ---------------------------------------------------------------------------
// Default baseline: no override ⇒ wPriority = 0 ⇒ None and no key.
// ---------------------------------------------------------------------------

#[test]
fn default_no_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r182-default-strh-priority.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_priority(0), None);
    assert_eq!(dmx.stream_priority(1), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k.starts_with("avi:strh.") && k.ends_with(".priority")));
}

// ---------------------------------------------------------------------------
// Builder idempotency: last with_stream_priority(idx, ...) wins.
// ---------------------------------------------------------------------------

#[test]
fn with_stream_priority_last_call_wins_per_index() {
    // Third call for stream 0 must replace the first; stream 1's
    // separate entry survives. Only one entry per stream after
    // retain-then-push.
    let opts = AviMuxOptions::new()
        .with_stream_priority(0, 10)
        .with_stream_priority(1, 20)
        .with_stream_priority(0, 99);
    assert_eq!(opts.stream_priorities.len(), 2);
    assert!(opts
        .stream_priorities
        .iter()
        .any(|(idx, p)| *idx == 0 && *p == 99));
    assert!(opts
        .stream_priorities
        .iter()
        .any(|(idx, p)| *idx == 1 && *p == 20));
    // Older 10 has been retained out.
    assert!(!opts.stream_priorities.iter().any(|(_, p)| *p == 10));
}

// ---------------------------------------------------------------------------
// Explicit 0 override reads back as None (default == absent).
// ---------------------------------------------------------------------------

#[test]
fn explicit_zero_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r182-strh-priority-zero.avi");
    let opts = AviMuxOptions::new().with_stream_priority(0, 0);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_priority(0),
        None,
        "the legacy `0` writer default must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.priority"));
}

// ---------------------------------------------------------------------------
// Boundary values: `1` (smallest non-default) and `u16::MAX` both
// round-trip verbatim. The spec does not pin a value range so neither
// extreme is special-cased.
// ---------------------------------------------------------------------------

#[test]
fn boundary_values_roundtrip_verbatim() {
    // Smallest non-default: 1 stays as Some(1) — distinct from the
    // absent sentinel.
    let tmp_lo = std::env::temp_dir().join("oxideav-avi-r182-strh-p-one.avi");
    let opts_lo = AviMuxOptions::new().with_stream_priority(0, 1);
    write_minimal(&tmp_lo, opts_lo);

    let reg = CodecRegistry::new();
    let rs_lo: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp_lo).unwrap());
    let dmx_lo = demuxer_open_avi(rs_lo, &reg).unwrap();
    assert_eq!(dmx_lo.stream_priority(0), Some(1));
    assert!(dmx_lo
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.priority" && v == "1"));

    // Largest representable: u16::MAX round-trips intact — the
    // demuxer does not clamp or normalise.
    let tmp_hi = std::env::temp_dir().join("oxideav-avi-r182-strh-p-max.avi");
    let opts_hi = AviMuxOptions::new().with_stream_priority(1, u16::MAX);
    write_minimal(&tmp_hi, opts_hi);

    let rs_hi: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp_hi).unwrap());
    let dmx_hi = demuxer_open_avi(rs_hi, &reg).unwrap();
    assert_eq!(dmx_hi.stream_priority(1), Some(u16::MAX));
    assert!(dmx_hi
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.1.priority" && v == "65535"));
}

// ---------------------------------------------------------------------------
// Per-stream independence: stamping stream 1's priority leaves stream
// 0 at the absent default, and vice versa.
// ---------------------------------------------------------------------------

#[test]
fn per_stream_priorities_are_independent() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r182-strh-p-per-stream.avi");
    let opts = AviMuxOptions::new()
        .with_stream_priority(0, 7)
        .with_stream_priority(1, 3);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_priority(0), Some(7));
    assert_eq!(dmx.stream_priority(1), Some(3));
    let md = dmx.metadata();
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:strh.0.priority" && v == "7"));
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:strh.1.priority" && v == "3"));
}

// ---------------------------------------------------------------------------
// Out-of-range stream-index accessor returns None (consistent with the
// other round-176 / 153 / 119 / 115 per-stream accessors).
// ---------------------------------------------------------------------------

#[test]
fn out_of_range_stream_index_returns_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r182-strh-p-oor.avi");
    let opts = AviMuxOptions::new().with_stream_priority(0, 5);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_priority(0), Some(5));
    assert_eq!(dmx.stream_priority(99), None);
}

// ---------------------------------------------------------------------------
// Independence from sibling per-stream DWORDs: setting `wPriority`
// doesn't perturb `dwQuality` / `dwInitialFrames` / `wLanguage`, and
// none of those perturb the `wPriority` accessor.
// ---------------------------------------------------------------------------

#[test]
fn priority_does_not_leak_into_sibling_strh_dwords() {
    // Priority-only: every other per-stream DWORD must read as default.
    let tmp1 = std::env::temp_dir().join("oxideav-avi-r182-strh-p-only.avi");
    let opts1 = AviMuxOptions::new().with_stream_priority(1, 4);
    write_minimal(&tmp1, opts1);

    let reg = CodecRegistry::new();
    let rs1: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp1).unwrap());
    let dmx1 = demuxer_open_avi(rs1, &reg).unwrap();
    assert_eq!(dmx1.stream_priority(1), Some(4));
    assert_eq!(dmx1.stream_quality(1), None);
    assert_eq!(dmx1.stream_initial_frames(1), None);
    assert_eq!(dmx1.stream_language(1), None);

    // Mixed: each per-stream field reads back independently.
    let tmp2 = std::env::temp_dir().join("oxideav-avi-r182-strh-p-mixed.avi");
    let opts2 = AviMuxOptions::new()
        .with_stream_priority(1, 4)
        .with_stream_quality(1, 6000)
        .with_stream_initial_frames(1, 18)
        .with_stream_language(1, 0x0409); // en-US
    write_minimal(&tmp2, opts2);

    let rs2: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp2).unwrap());
    let dmx2 = demuxer_open_avi(rs2, &reg).unwrap();
    assert_eq!(dmx2.stream_priority(1), Some(4));
    assert_eq!(dmx2.stream_quality(1), Some(6000));
    assert_eq!(dmx2.stream_initial_frames(1), Some(18));
    assert_eq!(dmx2.stream_language(1), Some(0x0409));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact strh wPriority bytes.
// ---------------------------------------------------------------------------

/// Push a chunk (`id` + LE size + body, RIFF word-pad) onto `out`.
fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() & 1 == 1 {
        out.push(0);
    }
}

/// Wrap `body` in a `LIST <form> ...` (LE size = 4 + body, word-pad).
fn list(form: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"LIST");
    v.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
    v.extend_from_slice(form);
    v.extend_from_slice(body);
    if (4 + body.len()) & 1 == 1 {
        v.push(0);
    }
    v
}

/// Build a 56-byte AVISTREAMHEADER body for a video stream with the
/// requested `wPriority` value LE-stamped at byte offset 12.
fn strh_video_with_priority(priority: u16) -> Vec<u8> {
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids"); // fccType
    strh.extend_from_slice(b"MJPG"); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&priority.to_le_bytes()); // wPriority (byte offset 12)
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwLength
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwQuality (= -1 default)
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
    strh.extend_from_slice(&0i16.to_le_bytes()); // rcFrame.left
    strh.extend_from_slice(&0i16.to_le_bytes()); // rcFrame.top
    strh.extend_from_slice(&64i16.to_le_bytes()); // rcFrame.right
    strh.extend_from_slice(&48i16.to_le_bytes()); // rcFrame.bottom
    assert_eq!(strh.len(), 56);
    strh
}

/// Build a minimal BITMAPINFOHEADER strf for an MJPG video stream.
fn strf_video_mjpg() -> Vec<u8> {
    let mut strf = Vec::with_capacity(40);
    strf.extend_from_slice(&40u32.to_le_bytes()); // biSize
    strf.extend_from_slice(&64u32.to_le_bytes()); // biWidth
    strf.extend_from_slice(&48u32.to_le_bytes()); // biHeight
    strf.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    strf.extend_from_slice(b"MJPG"); // biCompression
    strf.extend_from_slice(&(64u32 * 48 * 3).to_le_bytes()); // biSizeImage
    strf.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
    strf.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    strf
}

/// Assemble an entire AVI 1.0 file in memory with one video stream and
/// the requested `wPriority` value LE-stamped at byte offset 12 of the
/// 56-byte AVISTREAMHEADER.
fn build_avi_with_strh_priority(priority: u16) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40000u32.to_le_bytes()); // dwMicroSecPerFrame (25fps)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&0x0010u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwTotalFrames
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    avih.extend_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih.extend_from_slice(&48u32.to_le_bytes()); // dwHeight
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    assert_eq!(avih.len(), 56);

    let strh_body = strh_video_with_priority(priority);
    let strf_body = strf_video_mjpg();
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_body);
    push_chunk(&mut strl_body, b"strf", &strf_body);
    let strl = list(b"strl", &strl_body);

    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih);
    hdrl_body.extend_from_slice(&strl);
    let hdrl = list(b"hdrl", &hdrl_body);

    let mut movi_body = Vec::new();
    push_chunk(&mut movi_body, b"00dc", &[0x55u8; 64]);
    let movi = list(b"movi", &movi_body);

    let mut riff_body = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(&hdrl);
    riff_body.extend_from_slice(&movi);
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);
    out
}

#[test]
fn handrolled_explicit_priority_decodes() {
    // `0x1234` is a non-default selection-hint value chosen as a
    // distinctive bit pattern; the demuxer surfaces it verbatim
    // through both the typed accessor and the metadata key.
    let buf = build_avi_with_strh_priority(0x1234);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_priority(0), Some(0x1234));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.priority" && v == "4660"));
}

#[test]
fn handrolled_zero_priority_parses_as_none() {
    let buf = build_avi_with_strh_priority(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.stream_priority(0),
        None,
        "hand-rolled `0` (legacy default) must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.priority"));
}
