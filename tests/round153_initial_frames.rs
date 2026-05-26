//! Round-153 `strh.dwInitialFrames` (AVISTREAMHEADER interleave skew) AVI tests.
//!
//! `dwInitialFrames` is the 32-bit DWORD at byte offset 16 of the
//! 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, `dwInitialFrames`
//! row): *"How far audio data is skewed ahead of the video frames in
//! interleaved files. Typically, this is about 0.75 seconds. If
//! creating interleaved files, set the value of this member to the
//! number of frames in the file prior to the initial frame of the AVI
//! sequence in this member."* AVIMAINHEADER §`dwInitialFrames` adds:
//! *"Initial frame for interleaved files. Noninterleaved files should
//! specify zero."* — so `0` is the documented "noninterleaved /
//! unspecified" sentinel, mapped here to `None` so an unspecified skew
//! reads the same as an absent one (mirroring the round-119 `wLanguage`
//! / round-115 `rcFrame` / round-80 `strn` "default == absent"
//! convention).
//!
//! The unit is the stream's own `dwRate` / `dwScale` tick (typically
//! frames for video, blocks for audio); the demuxer surfaces the raw
//! u32 verbatim and the muxer writes the caller's 32-bit value verbatim
//! — no rate-conversion, no validation against the per-stream `dwLength`.
//!
//! The demuxer surfaces it via the typed `stream_initial_frames()`
//! accessor and the `avi:strh.<index>.initial_frames` metadata key; the
//! muxer can stamp a skew via `AviMuxOptions::with_stream_initial_frames`.
//! Exercises:
//!
//! - **Mux → demux round-trip** of a non-zero skew on an audio stream
//!   via the typed accessor and the metadata key.
//! - **No-override baseline**: with no override, streams get the
//!   `dwInitialFrames = 0` default, which the demuxer maps to `None`
//!   and the metadata-key loop omits.
//! - **Override on a video stream** round-trips — `dwInitialFrames` is a
//!   fixed strh field carried for any stream type.
//! - **Builder dedup**: the last `with_stream_initial_frames(...)` per
//!   stream index wins.
//! - **Explicit zero override** reads back as `None` (default == absent).
//! - **Hand-rolled fixture**: an explicit non-zero `dwInitialFrames` in
//!   a 56-byte strh decodes to the expected raw u32.
//! - **Hand-rolled fixture**: an all-zero `dwInitialFrames` parses as
//!   `None`.
//! - **0xFFFF_FFFF round-trip**: every bit of the 32-bit field survives.
//! - **Per-stream independence**: distinct skews on two streams each
//!   round-trip independently and don't bleed into the other.

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
// Round-trip: a non-zero skew on an audio stream survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn initial_frames_audio_override_roundtrip_accessor_and_metadata() {
    // 18 = the AVI 1.0 spec's quoted "typical 0.75 seconds" of audio
    // skew at 24fps interleave granularity. Pinned here as a concrete
    // non-zero literal — the muxer writes whatever 32-bit value the
    // caller supplies verbatim and does no rate-conversion.
    let tmp = std::env::temp_dir().join("oxideav-avi-r153-init-aud.avi");
    let opts = AviMuxOptions::new().with_stream_initial_frames(1, 18);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_initial_frames(1), Some(18));
    // Video stream 0 stays at the default; no metadata key emitted.
    assert_eq!(dmx.stream_initial_frames(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter()
            .any(|(k, v)| k == "avi:strh.1.initial_frames" && v == "18"),
        "missing avi:strh.1.initial_frames metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k.contains("initial_frames"))
            .collect::<Vec<_>>()
    );
    assert!(!md.iter().any(|(k, _)| k == "avi:strh.0.initial_frames"));
}

// ---------------------------------------------------------------------------
// Default baseline: no override ⇒ dwInitialFrames = 0 ⇒ None on both streams.
// ---------------------------------------------------------------------------

#[test]
fn default_no_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r153-default-init.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_initial_frames(0), None);
    assert_eq!(dmx.stream_initial_frames(1), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k.starts_with("avi:strh.") && k.ends_with(".initial_frames")));
}

// ---------------------------------------------------------------------------
// Override on a video stream round-trips.
// ---------------------------------------------------------------------------

#[test]
fn initial_frames_video_override_roundtrip() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r153-init-vid.avi");
    let opts = AviMuxOptions::new().with_stream_initial_frames(0, 5);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_initial_frames(0), Some(5));
    // Audio stream 1 stays at the default (no metadata key emitted).
    assert_eq!(dmx.stream_initial_frames(1), None);
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.initial_frames" && v == "5"));
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.1.initial_frames"));
}

// ---------------------------------------------------------------------------
// Builder dedup: the last with_stream_initial_frames per stream index wins.
// ---------------------------------------------------------------------------

#[test]
fn with_stream_initial_frames_dedups() {
    let opts = AviMuxOptions::new()
        .with_stream_initial_frames(0, 5)
        .with_stream_initial_frames(0, 18);
    let entries: Vec<_> = opts
        .stream_initial_frames
        .iter()
        .filter(|(idx, _)| *idx == 0)
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "duplicate index must collapse to one entry"
    );
    assert_eq!(entries[0].1, 18);
}

// ---------------------------------------------------------------------------
// An explicit zero override reads back as None (zero == absent).
// ---------------------------------------------------------------------------

#[test]
fn explicit_zero_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r153-zero-override.avi");
    let opts = AviMuxOptions::new().with_stream_initial_frames(0, 0);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_initial_frames(0),
        None,
        "an all-zero dwInitialFrames must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.initial_frames"));
}

// ---------------------------------------------------------------------------
// Per-stream independence: distinct skews on two streams each
// round-trip independently and don't bleed into the other.
// ---------------------------------------------------------------------------

#[test]
fn distinct_per_stream_skews_roundtrip_independently() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r153-distinct-init.avi");
    let opts = AviMuxOptions::new()
        .with_stream_initial_frames(0, 3) // video: 3-frame skew
        .with_stream_initial_frames(1, 21); // audio: 21-block skew
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_initial_frames(0), Some(3));
    assert_eq!(dmx.stream_initial_frames(1), Some(21));

    let md = dmx.metadata();
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:strh.0.initial_frames" && v == "3"));
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:strh.1.initial_frames" && v == "21"));
}

// ---------------------------------------------------------------------------
// Out-of-range stream indexes return None.
// ---------------------------------------------------------------------------

#[test]
fn out_of_range_index_is_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r153-oor.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_initial_frames(99), None);
}

// ---------------------------------------------------------------------------
// 0xFFFF_FFFF round-trip: every bit of the 32-bit field survives.
// ---------------------------------------------------------------------------

#[test]
fn all_bits_set_initial_frames_roundtrip() {
    // 0xFFFF_FFFF reads back as Some(u32::MAX) since `0` is the only
    // "unspecified" sentinel; an all-ones value is a legitimate skew
    // and must survive the round-trip bit-for-bit.
    let tmp = std::env::temp_dir().join("oxideav-avi-r153-all-bits.avi");
    let opts = AviMuxOptions::new().with_stream_initial_frames(0, 0xFFFF_FFFF);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_initial_frames(0), Some(0xFFFF_FFFF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.initial_frames" && v == "4294967295"));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact strh dwInitialFrames bytes.
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
/// requested `dwInitialFrames` LE-stamped at byte offset 16. Other
/// fields use the muxer's documented defaults so the resulting strh is
/// parseable.
fn strh_video_with_initial_frames(initial_frames: u32) -> Vec<u8> {
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids"); // fccType
    strh.extend_from_slice(b"MJPG"); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&initial_frames.to_le_bytes()); // dwInitialFrames (byte offset 16)
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwLength
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwQuality
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

/// Assemble an entire AVI 1.0 file in memory with one video stream
/// whose `strh.dwInitialFrames` is `initial_frames`.
fn build_avi_with_initial_frames(initial_frames: u32) -> Vec<u8> {
    // 56-byte avih.
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40000u32.to_le_bytes()); // dwMicroSecPerFrame (25fps)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&0x0010u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwTotalFrames
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames (avih's own)
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    avih.extend_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih.extend_from_slice(&48u32.to_le_bytes()); // dwHeight
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    assert_eq!(avih.len(), 56);

    let strh_body = strh_video_with_initial_frames(initial_frames);
    let strf_body = strf_video_mjpg();
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_body);
    push_chunk(&mut strl_body, b"strf", &strf_body);
    let strl = list(b"strl", &strl_body);

    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih);
    hdrl_body.extend_from_slice(&strl);
    let hdrl = list(b"hdrl", &hdrl_body);

    // One frame in movi.
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
fn handrolled_explicit_nonzero_initial_frames_decodes() {
    let buf = build_avi_with_initial_frames(0xCAFE_BABE);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_initial_frames(0), Some(0xCAFE_BABE));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:strh.0.initial_frames" && v == "3405691582"));
}

#[test]
fn handrolled_zero_initial_frames_parses_as_none() {
    let buf = build_avi_with_initial_frames(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_initial_frames(0), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:strh.0.initial_frames"));
}
