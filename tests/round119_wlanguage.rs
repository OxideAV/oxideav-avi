//! Round-119 `strh.wLanguage` (AVISTREAMHEADER language tag) AVI tests.
//!
//! `wLanguage` is the 16-bit LANGID at byte offset 14 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, `wLanguage` row):
//! *"Language tag (BCP 47 / RFC 1766 / similar; AVI does not normatively
//! pin a registry)."* Microsoft writers conventionally pack a Win32
//! LANGID — low 10 bits a `LANG_*` primary id and upper 6 bits a
//! `SUBLANG_*` dialect id — while non-MS writers may pack different
//! values; the demuxer surfaces the raw 16-bit DWORD verbatim and
//! leaves interpretation to the caller.
//!
//! The `0` ("LANG_NEUTRAL / SUBLANG_NEUTRAL", the writer-skips-it
//! default) is mapped to `None` so an unspecified language reads the
//! same as an absent one — mirroring the round-80 `strn` / round-107
//! `IDIT` / round-115 `rcFrame` "default == absent" convention.
//!
//! The demuxer surfaces it via the typed `stream_language()` accessor
//! and the `avi:strh.<index>.language` metadata key; the muxer can
//! stamp a LANGID via `AviMuxOptions::with_stream_language`. Exercises:
//!
//! - **Mux → demux round-trip** of a non-zero LANGID on a video stream
//!   via the typed accessor and the metadata key.
//! - **No-override baseline**: with no override, streams get the
//!   `wLanguage = 0` default, which the demuxer maps to `None` and the
//!   metadata-key loop omits.
//! - **Override on an audio stream** round-trips — `wLanguage` is a
//!   fixed strh field carried for any stream type.
//! - **Builder dedup**: the last `with_stream_language(...)` per stream
//!   index wins.
//! - **Explicit zero override** reads back as `None` (default == absent).
//! - **Hand-rolled fixture**: an explicit non-zero `wLanguage` in a
//!   56-byte strh decodes to the expected raw u16.
//! - **Hand-rolled fixture**: an all-zero `wLanguage` parses as `None`.
//! - **0xFFFF round-trip**: every bit of the 16-bit field survives.
//! - **Per-stream independence**: distinct LANGIDs on two streams
//!   each round-trip independently and don't bleed into the other.

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
// Round-trip: a non-zero LANGID on a video stream survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn wlanguage_video_override_roundtrip_accessor_and_metadata() {
    // 0x0409 = LANG_ENGLISH (0x09) | SUBLANG_ENGLISH_US (0x01 << 10),
    // the conventional Microsoft LANGID for US English.
    let tmp = std::env::temp_dir().join("oxideav-avi-r119-wlang-vid.avi");
    let opts = AviMuxOptions::new().with_stream_language(0, 0x0409);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(0), Some(0x0409));

    let md = dmx.metadata();
    assert!(
        md.iter()
            .any(|(k, v)| k == "avi:strh.0.language" && v == "1033"),
        "missing avi:strh.0.language metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k.starts_with("avi:strh."))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Default baseline: no override ⇒ wLanguage = 0 ⇒ None on both streams.
// ---------------------------------------------------------------------------

#[test]
fn default_no_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r119-default-lang.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(0), None);
    assert_eq!(dmx.stream_language(1), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k.starts_with("avi:strh.") && k.ends_with(".language")));
}

// ---------------------------------------------------------------------------
// Override on an audio stream round-trips.
// ---------------------------------------------------------------------------

#[test]
fn wlanguage_audio_override_roundtrip() {
    // 0x0411 = LANG_JAPANESE (0x11) | SUBLANG_DEFAULT (0x01 << 10) ⇒ 0x0411
    // (per Microsoft conventions; the muxer writes whatever 16-bit value
    // the caller supplies verbatim and does not validate against a
    // registry).
    let tmp = std::env::temp_dir().join("oxideav-avi-r119-wlang-aud.avi");
    let opts = AviMuxOptions::new().with_stream_language(1, 0x0411);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(1), Some(0x0411));
    // Video stream 0 stays at the default (no metadata key emitted).
    assert_eq!(dmx.stream_language(0), None);
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.1.language" && v == "1041"));
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.language"));
}

// ---------------------------------------------------------------------------
// Builder dedup: the last with_stream_language per stream index wins.
// ---------------------------------------------------------------------------

#[test]
fn with_stream_language_dedups() {
    let opts = AviMuxOptions::new()
        .with_stream_language(0, 0x0409)
        .with_stream_language(0, 0x0411);
    let entries: Vec<_> = opts
        .stream_languages
        .iter()
        .filter(|(idx, _)| *idx == 0)
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "duplicate index must collapse to one entry"
    );
    assert_eq!(entries[0].1, 0x0411);
}

// ---------------------------------------------------------------------------
// An explicit zero override reads back as None (zero == absent).
// ---------------------------------------------------------------------------

#[test]
fn explicit_zero_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r119-zero-override.avi");
    let opts = AviMuxOptions::new().with_stream_language(0, 0);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_language(0),
        None,
        "an all-zero wLanguage must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.language"));
}

// ---------------------------------------------------------------------------
// Per-stream independence: distinct LANGIDs on two streams each
// round-trip independently and don't bleed into the other.
// ---------------------------------------------------------------------------

#[test]
fn distinct_per_stream_langids_roundtrip_independently() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r119-distinct-langs.avi");
    let opts = AviMuxOptions::new()
        .with_stream_language(0, 0x0409) // video: en-US
        .with_stream_language(1, 0x0411); // audio: ja
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(0), Some(0x0409));
    assert_eq!(dmx.stream_language(1), Some(0x0411));

    let md = dmx.metadata();
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:strh.0.language" && v == "1033"));
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:strh.1.language" && v == "1041"));
}

// ---------------------------------------------------------------------------
// Out-of-range stream indexes return None.
// ---------------------------------------------------------------------------

#[test]
fn out_of_range_index_is_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r119-oor.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(99), None);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact strh wLanguage bytes.
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

/// 56-byte AVISTREAMHEADER (`strh`) for a `vids` MJPG stream,
/// parameterised on the `wLanguage` u16 at byte offset 14.
fn strh_vids(wlanguage: u16) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"vids"); // fccType
    b.extend_from_slice(b"MJPG"); // fccHandler
    b.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    b.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    b.extend_from_slice(&wlanguage.to_le_bytes()); // wLanguage  ← under test
    b.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    b.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    b.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    b.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    b.extend_from_slice(&1u32.to_le_bytes()); // dwLength
    b.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    b.extend_from_slice(&0u32.to_le_bytes()); // dwQuality
    b.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
    b.extend_from_slice(&[0u8; 8]); // rcFrame: all-zero (whole-movie default)
    b
}

/// Minimal BITMAPINFOHEADER (`strf`) for a 64×48 MJPG stream.
fn strf_mjpg() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&40u32.to_le_bytes()); // biSize
    b.extend_from_slice(&64i32.to_le_bytes()); // biWidth
    b.extend_from_slice(&48i32.to_le_bytes()); // biHeight
    b.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    b.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    b.extend_from_slice(b"MJPG"); // biCompression
    b.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    b.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    b.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    b.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    b.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    b
}

/// AVIMAINHEADER (`avih`, 56 bytes) with one stream (64×48).
fn avih_one_stream() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame
    b.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    b.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    b.extend_from_slice(&0x10u32.to_le_bytes()); // dwFlags (AVIF_HASINDEX)
    b.extend_from_slice(&1u32.to_le_bytes()); // dwTotalFrames
    b.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    b.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    b.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    b.extend_from_slice(&64u32.to_le_bytes()); // dwWidth
    b.extend_from_slice(&48u32.to_le_bytes()); // dwHeight
    b.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    b
}

/// Assemble a one-stream MJPG AVI whose `strh` carries the given
/// `wLanguage` u16.
fn build_avi_with_wlanguage(wlanguage: u16) -> Vec<u8> {
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_vids(wlanguage));
    push_chunk(&mut strl_body, b"strf", &strf_mjpg());
    let strl = list(b"strl", &strl_body);

    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih_one_stream());
    hdrl_body.extend_from_slice(&strl);
    let hdrl = list(b"hdrl", &hdrl_body);

    let mut movi_body = Vec::new();
    push_chunk(&mut movi_body, b"00dc", &[0x55u8; 4]);
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
fn handrolled_nonzero_wlanguage_decodes() {
    let bytes = build_avi_with_wlanguage(0x040C); // LANG_FRENCH | SUBLANG_FRENCH = 1036
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(0), Some(0x040C));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.language" && v == "1036"));
}

#[test]
fn handrolled_zero_wlanguage_parses_as_none() {
    let bytes = build_avi_with_wlanguage(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(0), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.language"));
}

#[test]
fn handrolled_all_bits_set_wlanguage_roundtrips() {
    // Every bit of the 16-bit field — `0xFFFF` is not a defined LANGID
    // under Microsoft conventions but the demuxer surfaces whatever the
    // file declared (no registry validation per the spec's "AVI does not
    // normatively pin a registry" remark).
    let bytes = build_avi_with_wlanguage(0xFFFF);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(0), Some(0xFFFF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.language" && v == "65535"));
}

#[test]
fn mux_roundtrip_0xffff_via_builder() {
    // Cross-check the mux side against 0xFFFF too — the builder must not
    // silently truncate, mask, or validate.
    let tmp = std::env::temp_dir().join("oxideav-avi-r119-ffff.avi");
    let opts = AviMuxOptions::new().with_stream_language(0, 0xFFFF);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_language(0), Some(0xFFFF));
}
