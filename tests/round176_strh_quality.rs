//! Round-176 per-stream `strh.dwQuality` AVI tests.
//!
//! `dwQuality` is the 32-bit DWORD at byte offset 40 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, Appendix B
//! `dwQuality` row, line 246): *"Indicator of the quality of the data
//! in the stream. Quality is represented as a number between 0 and
//! 10,000. For compressed data, this typically represents the value of
//! the quality parameter passed to the compression software. If set to
//! -1, drivers use the default quality value."*
//!
//! `-1` (== `0xFFFF_FFFF` as u32) is the documented "use default driver
//! quality" sentinel — the legacy muxer's own default since round-3 —
//! mapped here to `None` so an unspecified quality reads the same as an
//! absent one, mirroring the round-153 `dwInitialFrames` / round-119
//! `wLanguage` / round-115 `rcFrame` / round-80 `strn` / round-107
//! `IDIT` "default == absent" convention.
//!
//! The documented range is `[0, 10_000]` but the demuxer surfaces the
//! raw 32-bit DWORD verbatim and the muxer writes whatever 32-bit value
//! the caller supplies — anomalous out-of-spec writers round-trip
//! exactly. Exercises:
//!
//! - **Mux → demux round-trip** of a non-default per-stream quality via
//!   the typed accessor and the metadata key.
//! - **No-override baseline**: with no override, the muxer keeps the
//!   `dwQuality = -1` default which the demuxer maps to `None` and the
//!   metadata-key loop omits.
//! - **Builder idempotency**: the last `with_stream_quality(...)` wins
//!   per stream index.
//! - **Explicit `-1` (0xFFFF_FFFF) override** reads back as `None`
//!   (default == absent).
//! - **Documented-range endpoints** (`0` and `10_000`) round-trip
//!   verbatim — the `0` low end is *not* treated as "default" (only
//!   `0xFFFF_FFFF` is the sentinel per spec).
//! - **Out-of-spec values** (e.g. `0x0001_0000`, `0x7FFF_FFFE`) survive
//!   the round-trip; the demuxer does not clamp to `[0, 10_000]`.
//! - **Independence across streams**: quality on stream 1 doesn't
//!   perturb stream 0's `None`, and vice versa.
//! - **Independence from sibling DWORDs**: stamping `dwQuality` doesn't
//!   perturb `dwInitialFrames` / `wLanguage` / `rcFrame` / `dwLength`
//!   readbacks.
//! - **Hand-rolled fixtures**: an explicit non-default `dwQuality` in a
//!   56-byte strh decodes to the expected raw u32; an all-ones
//!   (`0xFFFF_FFFF`) `dwQuality` parses as `None`.

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
// Round-trip: a non-default per-stream quality survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn strh_quality_override_roundtrip_accessor_and_metadata() {
    // 7500 sits inside the documented [0, 10_000] range — a typical
    // "high quality" encoder driving knob. Pinned as a concrete literal:
    // the muxer writes whatever 32-bit value the caller supplies
    // verbatim and does no clamp.
    let tmp = std::env::temp_dir().join("oxideav-avi-r176-strh-quality.avi");
    let opts = AviMuxOptions::new().with_stream_quality(1, 7500);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_quality(1), Some(7500));
    // Stream 0 keeps the default sentinel ⇒ reads as None.
    assert_eq!(dmx.stream_quality(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter()
            .any(|(k, v)| k == "avi:strh.1.quality" && v == "7500"),
        "missing avi:strh.1.quality metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k.contains("quality"))
            .collect::<Vec<_>>()
    );
    // Stream 0's key must not surface.
    assert!(!md.iter().any(|(k, _)| k == "avi:strh.0.quality"));
}

// ---------------------------------------------------------------------------
// Default baseline: no override ⇒ dwQuality = -1 ⇒ None and no key.
// ---------------------------------------------------------------------------

#[test]
fn default_no_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r176-default-strh-quality.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_quality(0), None);
    assert_eq!(dmx.stream_quality(1), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k.starts_with("avi:strh.") && k.ends_with(".quality")));
}

// ---------------------------------------------------------------------------
// Builder idempotency: last with_stream_quality(idx, ...) wins.
// ---------------------------------------------------------------------------

#[test]
fn with_stream_quality_last_call_wins_per_index() {
    // Third call for stream 0 must replace the first; stream 1's
    // separate entry survives. Only one entry per stream after
    // retain-then-push.
    let opts = AviMuxOptions::new()
        .with_stream_quality(0, 1000)
        .with_stream_quality(1, 2000)
        .with_stream_quality(0, 9999);
    assert_eq!(opts.stream_qualities.len(), 2);
    assert!(opts
        .stream_qualities
        .iter()
        .any(|(idx, q)| *idx == 0 && *q == 9999));
    assert!(opts
        .stream_qualities
        .iter()
        .any(|(idx, q)| *idx == 1 && *q == 2000));
    // Older 1000 has been retained out.
    assert!(!opts.stream_qualities.iter().any(|(_, q)| *q == 1000));
}

// ---------------------------------------------------------------------------
// Explicit -1 (0xFFFF_FFFF) override reads back as None (sentinel == absent).
// ---------------------------------------------------------------------------

#[test]
fn explicit_minus_one_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r176-strh-minus-one.avi");
    let opts = AviMuxOptions::new().with_stream_quality(0, 0xFFFF_FFFF);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_quality(0),
        None,
        "the documented `-1` (`0xFFFF_FFFF`) sentinel must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.quality"));
}

// ---------------------------------------------------------------------------
// Documented-range endpoints: `0` and `10_000` both round-trip verbatim.
// `0` is *not* a sentinel — only `0xFFFF_FFFF` is.
// ---------------------------------------------------------------------------

#[test]
fn documented_range_endpoints_roundtrip() {
    // Low end: 0 stays as Some(0) — distinct from the absent sentinel.
    let tmp_lo = std::env::temp_dir().join("oxideav-avi-r176-strh-q-zero.avi");
    let opts_lo = AviMuxOptions::new().with_stream_quality(0, 0);
    write_minimal(&tmp_lo, opts_lo);

    let reg = CodecRegistry::new();
    let rs_lo: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp_lo).unwrap());
    let dmx_lo = demuxer_open_avi(rs_lo, &reg).unwrap();
    assert_eq!(dmx_lo.stream_quality(0), Some(0));
    assert!(dmx_lo
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.quality" && v == "0"));

    // High end: documented maximum 10_000 round-trips intact.
    let tmp_hi = std::env::temp_dir().join("oxideav-avi-r176-strh-q-10000.avi");
    let opts_hi = AviMuxOptions::new().with_stream_quality(1, 10_000);
    write_minimal(&tmp_hi, opts_hi);

    let rs_hi: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp_hi).unwrap());
    let dmx_hi = demuxer_open_avi(rs_hi, &reg).unwrap();
    assert_eq!(dmx_hi.stream_quality(1), Some(10_000));
    assert!(dmx_hi
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.1.quality" && v == "10000"));
}

// ---------------------------------------------------------------------------
// Out-of-spec values: the demuxer surfaces them verbatim. Per the spec
// header's "represented as a number between 0 and 10,000" wording the
// range is documented but not normative — capture drivers and legacy
// VfW tools occasionally stamp arbitrary u32 values and the demuxer
// preserves them.
// ---------------------------------------------------------------------------

#[test]
fn out_of_spec_values_roundtrip_verbatim() {
    // Just above the documented max (10_001..u32::MAX-1 is the
    // "anomalous but observed" range).
    let tmp1 = std::env::temp_dir().join("oxideav-avi-r176-strh-q-65536.avi");
    let opts1 = AviMuxOptions::new().with_stream_quality(0, 0x0001_0000);
    write_minimal(&tmp1, opts1);

    let reg = CodecRegistry::new();
    let rs1: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp1).unwrap());
    let dmx1 = demuxer_open_avi(rs1, &reg).unwrap();
    assert_eq!(
        dmx1.stream_quality(0),
        Some(0x0001_0000),
        "out-of-range values must round-trip verbatim with no clamp"
    );
    assert!(dmx1
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.quality" && v == "65536"));

    // Just below the sentinel — every bit except the `-1` marker is
    // legal data per spec.
    let tmp2 = std::env::temp_dir().join("oxideav-avi-r176-strh-q-near-max.avi");
    let opts2 = AviMuxOptions::new().with_stream_quality(1, 0x7FFF_FFFE);
    write_minimal(&tmp2, opts2);

    let rs2: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp2).unwrap());
    let dmx2 = demuxer_open_avi(rs2, &reg).unwrap();
    assert_eq!(dmx2.stream_quality(1), Some(0x7FFF_FFFE));
    assert!(dmx2
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.1.quality" && v == "2147483646"));
}

// ---------------------------------------------------------------------------
// Per-stream independence: stamping stream 1's quality leaves stream 0
// at the absent default, and vice versa.
// ---------------------------------------------------------------------------

#[test]
fn per_stream_qualities_are_independent() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r176-strh-q-per-stream.avi");
    let opts = AviMuxOptions::new()
        .with_stream_quality(0, 8000)
        .with_stream_quality(1, 2500);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_quality(0), Some(8000));
    assert_eq!(dmx.stream_quality(1), Some(2500));
    let md = dmx.metadata();
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:strh.0.quality" && v == "8000"));
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:strh.1.quality" && v == "2500"));
}

// ---------------------------------------------------------------------------
// Out-of-range stream-index accessor returns None (consistent with the
// other round-153 / 119 / 115 per-stream accessors).
// ---------------------------------------------------------------------------

#[test]
fn out_of_range_stream_index_returns_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r176-strh-q-oor.avi");
    let opts = AviMuxOptions::new().with_stream_quality(0, 5000);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_quality(0), Some(5000));
    assert_eq!(dmx.stream_quality(99), None);
}

// ---------------------------------------------------------------------------
// Independence from sibling per-stream DWORDs: setting `dwQuality`
// doesn't perturb `dwInitialFrames` / `wLanguage` / `rcFrame`, and
// none of those perturb the `dwQuality` accessor.
// ---------------------------------------------------------------------------

#[test]
fn quality_does_not_leak_into_sibling_strh_dwords() {
    // Quality-only: every other per-stream DWORD must read as default.
    let tmp1 = std::env::temp_dir().join("oxideav-avi-r176-strh-q-only.avi");
    let opts1 = AviMuxOptions::new().with_stream_quality(1, 6000);
    write_minimal(&tmp1, opts1);

    let reg = CodecRegistry::new();
    let rs1: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp1).unwrap());
    let dmx1 = demuxer_open_avi(rs1, &reg).unwrap();
    assert_eq!(dmx1.stream_quality(1), Some(6000));
    assert_eq!(dmx1.stream_initial_frames(1), None);
    assert_eq!(dmx1.stream_language(1), None);

    // Mixed: each per-stream field reads back independently.
    let tmp2 = std::env::temp_dir().join("oxideav-avi-r176-strh-q-mixed.avi");
    let opts2 = AviMuxOptions::new()
        .with_stream_quality(1, 6000)
        .with_stream_initial_frames(1, 18)
        .with_stream_language(1, 0x0409); // en-US
    write_minimal(&tmp2, opts2);

    let rs2: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp2).unwrap());
    let dmx2 = demuxer_open_avi(rs2, &reg).unwrap();
    assert_eq!(dmx2.stream_quality(1), Some(6000));
    assert_eq!(dmx2.stream_initial_frames(1), Some(18));
    assert_eq!(dmx2.stream_language(1), Some(0x0409));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact strh dwQuality bytes.
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
/// requested `dwQuality` value LE-stamped at byte offset 40.
fn strh_video_with_quality(quality: u32) -> Vec<u8> {
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids"); // fccType
    strh.extend_from_slice(b"MJPG"); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwLength
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    strh.extend_from_slice(&quality.to_le_bytes()); // dwQuality (byte offset 40)
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
/// the requested `strh.dwQuality` value LE-stamped at byte offset 40 of
/// the 56-byte AVISTREAMHEADER.
fn build_avi_with_strh_quality(quality: u32) -> Vec<u8> {
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

    let strh_body = strh_video_with_quality(quality);
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
fn handrolled_explicit_quality_decodes() {
    // 0xDEAD_BEEF is *not* the documented `-1` sentinel so it surfaces
    // verbatim through both the typed accessor and the metadata key.
    let buf = build_avi_with_strh_quality(0xDEAD_BEEF);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_quality(0), Some(0xDEAD_BEEF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.quality" && v == "3735928559"));
}

#[test]
fn handrolled_minus_one_quality_parses_as_none() {
    let buf = build_avi_with_strh_quality(0xFFFF_FFFF);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.stream_quality(0),
        None,
        "hand-rolled 0xFFFF_FFFF (`-1` sentinel) must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.quality"));
}
