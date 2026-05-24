//! Round-115 `strh.rcFrame` (AVISTREAMHEADER destination rectangle)
//! AVI tests.
//!
//! `rcFrame` is the destination rectangle in the 56-byte AVISTREAMHEADER
//! per AVI 1.0 §"AVISTREAMHEADER" (docs/container/riff/
//! avi-riff-file-reference.md, `rcFrame` row): "Destination rectangle
//! for a text or video stream within the movie rectangle specified by
//! the dwWidth and dwHeight members of the AVI main header structure …
//! typically used in support of multiple video streams … Units for this
//! member are pixels. The upper-left corner of the destination rectangle
//! is relative to the upper-left corner of the movie rectangle." The
//! four values are signed WORDs read little-endian in
//! `[left, top, right, bottom]` order off byte offset 48.
//!
//! The demuxer surfaces it via the typed `stream_frame_rect()` accessor
//! and the `avi:strh.<index>.frame_rect` metadata key; the muxer can
//! override the default rect via `with_stream_frame_rect`. The all-zero
//! "whole movie rectangle" default maps back to `None` so a default rect
//! reads the same as an absent one (mirroring the round-80 `strn` /
//! round-107 `IDIT` "empty == absent" convention). Exercises:
//!
//! - **Mux → demux round-trip** of a custom sub-rectangle on a video
//!   stream via the typed accessor and the metadata key.
//! - **Default video rect**: with no override, a video stream gets the
//!   muxer default `0,0,width,height`, which the demuxer surfaces (it is
//!   non-zero for a non-empty frame).
//! - **No-override audio baseline**: an audio stream gets an all-zero
//!   rect by default, which the demuxer maps to `None`.
//! - **Override on a non-video (audio) stream** round-trips — `rcFrame`
//!   is defined for text or video streams but is a fixed strh field that
//!   the muxer/demuxer carry for any stream type.
//! - **Builder dedup**: the last `with_stream_frame_rect(...)` per stream
//!   index wins.
//! - **Override of `0,0,0,0`** reads back as `None` (all-zero == absent).
//! - **Hand-rolled fixture**: an explicit non-zero rcFrame in a 56-byte
//!   strh is decoded into the expected `[left, top, right, bottom]`.
//! - **Hand-rolled fixture**: an all-zero rcFrame parses as `None`.
//! - **Hand-rolled fixture**: a short 48-byte strh (no rcFrame field)
//!   parses as `None`.
//! - **Negative coordinates** survive the i16 round-trip.

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
// Round-trip: a custom rcFrame on a video stream survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn rcframe_video_override_roundtrip_accessor_and_metadata() {
    // A picture-in-picture sub-rectangle: 8,4 .. 40,28 (32×24 box).
    let tmp = std::env::temp_dir().join("oxideav-avi-r115-rcframe-vid.avi");
    let opts = AviMuxOptions::new().with_stream_frame_rect(0, 8, 4, 40, 28);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_frame_rect(0), Some((8, 4, 40, 28)));

    let md = dmx.metadata();
    assert!(
        md.iter()
            .any(|(k, v)| k == "avi:strh.0.frame_rect" && v == "8,4,40,28"),
        "missing avi:strh.0.frame_rect metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k.starts_with("avi:strh."))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Default video rect: no override ⇒ muxer writes 0,0,width,height, which
// the demuxer surfaces (non-zero for a non-empty frame). The audio stream
// gets an all-zero default rect ⇒ None.
// ---------------------------------------------------------------------------

#[test]
fn default_video_rect_surfaces_audio_rect_is_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r115-default-rect.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Video stream 0: default rect is 0,0,width,height = 0,0,64,48.
    assert_eq!(dmx.stream_frame_rect(0), Some((0, 0, 64, 48)));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.frame_rect" && v == "0,0,64,48"));

    // Audio stream 1: default rect is all-zero ⇒ None, no metadata key.
    assert_eq!(dmx.stream_frame_rect(1), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.1.frame_rect"));
}

// ---------------------------------------------------------------------------
// Override on a non-video (audio) stream round-trips.
// ---------------------------------------------------------------------------

#[test]
fn rcframe_audio_override_roundtrip() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r115-rcframe-aud.avi");
    let opts = AviMuxOptions::new().with_stream_frame_rect(1, 1, 2, 3, 4);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_frame_rect(1), Some((1, 2, 3, 4)));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.1.frame_rect" && v == "1,2,3,4"));
}

// ---------------------------------------------------------------------------
// Builder dedup: the last with_stream_frame_rect per stream index wins.
// ---------------------------------------------------------------------------

#[test]
fn with_stream_frame_rect_dedups() {
    let opts = AviMuxOptions::new()
        .with_stream_frame_rect(0, 1, 1, 2, 2)
        .with_stream_frame_rect(0, 5, 6, 7, 8);
    let entries: Vec<_> = opts
        .stream_frame_rects
        .iter()
        .filter(|(idx, _)| *idx == 0)
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "duplicate index must collapse to one entry"
    );
    assert_eq!(entries[0].1, [5, 6, 7, 8]);
}

// ---------------------------------------------------------------------------
// An explicit all-zero override reads back as None (all-zero == absent).
// ---------------------------------------------------------------------------

#[test]
fn explicit_zero_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r115-zero-override.avi");
    // Override stream 0 (video) to all-zero, suppressing the default rect.
    let opts = AviMuxOptions::new().with_stream_frame_rect(0, 0, 0, 0, 0);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_frame_rect(0),
        None,
        "an all-zero rcFrame must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.frame_rect"));
}

// ---------------------------------------------------------------------------
// Negative coordinates survive the i16 round-trip.
// ---------------------------------------------------------------------------

#[test]
fn negative_coordinates_roundtrip() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r115-neg-rect.avi");
    let opts = AviMuxOptions::new().with_stream_frame_rect(0, -10, -20, 30, 40);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_frame_rect(0), Some((-10, -20, 30, 40)));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.frame_rect" && v == "-10,-20,30,40"));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact strh rcFrame bytes.
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

/// AVISTREAMHEADER (`strh`) for a `vids` MJPG stream, parameterised on
/// the `rcFrame` bytes. When `rc_frame` is `Some([l,t,r,b])` the full
/// 56-byte header is emitted with that rect at offset 48; when `None`
/// only the 48-byte prefix (through dwSampleSize) is emitted so the
/// demuxer sees the short form with no rcFrame field.
fn strh_vids(rc_frame: Option<[i16; 4]>) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"vids"); // fccType
    b.extend_from_slice(b"MJPG"); // fccHandler
    b.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    b.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    b.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    b.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    b.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    b.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    b.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    b.extend_from_slice(&1u32.to_le_bytes()); // dwLength
    b.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    b.extend_from_slice(&0u32.to_le_bytes()); // dwQuality
    b.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
    if let Some([l, t, r, bot]) = rc_frame {
        b.extend_from_slice(&l.to_le_bytes());
        b.extend_from_slice(&t.to_le_bytes());
        b.extend_from_slice(&r.to_le_bytes());
        b.extend_from_slice(&bot.to_le_bytes());
    }
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
/// `rc_frame` (or the short 48-byte form when `None`).
fn build_avi_with_rcframe(rc_frame: Option<[i16; 4]>) -> Vec<u8> {
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_vids(rc_frame));
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
fn handrolled_nonzero_rcframe_decodes() {
    let bytes = build_avi_with_rcframe(Some([100, 200, 300, 400]));
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_frame_rect(0), Some((100, 200, 300, 400)));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.frame_rect" && v == "100,200,300,400"));
}

#[test]
fn handrolled_zero_rcframe_parses_as_none() {
    let bytes = build_avi_with_rcframe(Some([0, 0, 0, 0]));
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_frame_rect(0), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.frame_rect"));
}

#[test]
fn handrolled_short_48byte_strh_has_no_rcframe() {
    // A 48-byte strh (through dwSampleSize) carries no rcFrame field; the
    // demuxer still accepts it (matching the existing `strh.len() < 48`
    // floor) but reports no destination rectangle.
    let bytes = build_avi_with_rcframe(None);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_frame_rect(0), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.frame_rect"));
}

// ---------------------------------------------------------------------------
// Out-of-range stream index returns None.
// ---------------------------------------------------------------------------

#[test]
fn out_of_range_index_is_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r115-oob.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_frame_rect(99), None);
}
