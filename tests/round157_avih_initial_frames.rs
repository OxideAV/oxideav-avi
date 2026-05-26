//! Round-157 file-global `avih.dwInitialFrames` AVI tests.
//!
//! `dwInitialFrames` is the 32-bit DWORD at byte offset 16 of the
//! 56-byte AVIMAINHEADER body (byte 24 of the `avih` chunk) per AVI 1.0
//! §"AVIMAINHEADER" (`docs/container/riff/avi-riff-file-reference.md`,
//! Appendix A `dwInitialFrames` row, line 200): *"Initial frame for
//! interleaved files. Noninterleaved files should specify zero. If
//! creating interleaved files, specify the number of frames in the file
//! prior to the initial frame of the AVI sequence."*
//!
//! `0` is the documented "noninterleaved / unspecified" sentinel,
//! mapped here to `None` so an unspecified skew reads the same as an
//! absent one — mirroring the round-153 per-stream `strh.dwInitialFrames`
//! convention (and the broader round-119 `wLanguage` / round-115
//! `rcFrame` / round-80 `strn` "default == absent" idiom).
//!
//! This is the file-global counterpart of the per-stream
//! `strh.dwInitialFrames` (round 153). The two fields are independent —
//! the muxer writes whatever 32-bit value the caller supplies verbatim
//! and performs no rate-conversion or validation against any per-stream
//! `dwLength`.
//!
//! The demuxer surfaces it via the typed `initial_frames()` accessor and
//! the `avi:initial_frames` metadata key; the muxer can stamp a skew via
//! `AviMuxOptions::with_initial_frames`. Exercises:
//!
//! - **Mux → demux round-trip** of a non-zero file-global skew via the
//!   typed accessor and the metadata key.
//! - **No-override baseline**: with no override, the muxer keeps the
//!   `dwInitialFrames = 0` default which the demuxer maps to `None` and
//!   the metadata-key loop omits.
//! - **Builder idempotency**: the last `with_initial_frames(...)` wins.
//! - **Explicit zero override** reads back as `None` (default == absent).
//! - **0xFFFF_FFFF round-trip**: every bit of the 32-bit field survives.
//! - **Independence from per-stream**: stamping the file-global value
//!   doesn't perturb the per-stream `strh.dwInitialFrames` accessors,
//!   and vice versa.
//! - **Hand-rolled fixture**: an explicit non-zero `dwInitialFrames` in
//!   a 56-byte avih decodes to the expected raw u32.
//! - **Hand-rolled fixture**: an all-zero `dwInitialFrames` parses as
//!   `None`.

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
// Round-trip: a non-zero file-global skew survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn avih_initial_frames_override_roundtrip_accessor_and_metadata() {
    // 18 = the AVI 1.0 spec's quoted "typical 0.75 seconds" of audio
    // skew at 24fps interleave granularity. Pinned here as a concrete
    // non-zero literal — the muxer writes whatever 32-bit value the
    // caller supplies verbatim and does no rate-conversion.
    let tmp = std::env::temp_dir().join("oxideav-avi-r157-avih-init.avi");
    let opts = AviMuxOptions::new().with_initial_frames(18);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.initial_frames(), Some(18));

    let md = dmx.metadata();
    assert!(
        md.iter()
            .any(|(k, v)| k == "avi:initial_frames" && v == "18"),
        "missing avi:initial_frames metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k.contains("initial_frames"))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Default baseline: no override ⇒ dwInitialFrames = 0 ⇒ None and no key.
// ---------------------------------------------------------------------------

#[test]
fn default_no_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r157-default-avih-init.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.initial_frames(), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:initial_frames"));
}

// ---------------------------------------------------------------------------
// Builder idempotency: the last with_initial_frames(...) wins.
// ---------------------------------------------------------------------------

#[test]
fn with_initial_frames_last_call_wins() {
    let opts = AviMuxOptions::new()
        .with_initial_frames(5)
        .with_initial_frames(18);
    assert_eq!(opts.initial_frames, Some(18));
}

// ---------------------------------------------------------------------------
// An explicit zero override reads back as None (zero == absent).
// ---------------------------------------------------------------------------

#[test]
fn explicit_zero_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r157-avih-zero-override.avi");
    let opts = AviMuxOptions::new().with_initial_frames(0);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.initial_frames(),
        None,
        "an all-zero avih.dwInitialFrames must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:initial_frames"));
}

// ---------------------------------------------------------------------------
// 0xFFFF_FFFF round-trip: every bit of the 32-bit field survives.
// ---------------------------------------------------------------------------

#[test]
fn all_bits_set_avih_initial_frames_roundtrip() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r157-avih-all-bits.avi");
    let opts = AviMuxOptions::new().with_initial_frames(0xFFFF_FFFF);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.initial_frames(), Some(0xFFFF_FFFF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:initial_frames" && v == "4294967295"));
}

// ---------------------------------------------------------------------------
// Independence: file-global override doesn't perturb per-stream values,
// and vice versa. The two DWORDs live at different bytes (avih offset 16
// vs strh offset 16) and round-trip independently.
// ---------------------------------------------------------------------------

#[test]
fn avih_and_strh_initial_frames_are_independent() {
    // File-global only: per-stream must remain None.
    let tmp1 = std::env::temp_dir().join("oxideav-avi-r157-avih-only.avi");
    let opts1 = AviMuxOptions::new().with_initial_frames(7);
    write_minimal(&tmp1, opts1);

    let reg = CodecRegistry::new();
    let rs1: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp1).unwrap());
    let dmx1 = demuxer_open_avi(rs1, &reg).unwrap();
    assert_eq!(dmx1.initial_frames(), Some(7));
    assert_eq!(dmx1.stream_initial_frames(0), None);
    assert_eq!(dmx1.stream_initial_frames(1), None);

    // Per-stream only: file-global must remain None.
    let tmp2 = std::env::temp_dir().join("oxideav-avi-r157-strh-only.avi");
    let opts2 = AviMuxOptions::new().with_stream_initial_frames(1, 21);
    write_minimal(&tmp2, opts2);

    let rs2: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp2).unwrap());
    let dmx2 = demuxer_open_avi(rs2, &reg).unwrap();
    assert_eq!(dmx2.initial_frames(), None);
    assert_eq!(dmx2.stream_initial_frames(1), Some(21));

    // Both together: each reads back independently.
    let tmp3 = std::env::temp_dir().join("oxideav-avi-r157-avih-and-strh.avi");
    let opts3 = AviMuxOptions::new()
        .with_initial_frames(3)
        .with_stream_initial_frames(1, 18);
    write_minimal(&tmp3, opts3);

    let rs3: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp3).unwrap());
    let dmx3 = demuxer_open_avi(rs3, &reg).unwrap();
    assert_eq!(dmx3.initial_frames(), Some(3));
    assert_eq!(dmx3.stream_initial_frames(0), None);
    assert_eq!(dmx3.stream_initial_frames(1), Some(18));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact avih dwInitialFrames bytes.
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

/// Build a 56-byte AVISTREAMHEADER body for a video stream. Other fields
/// use parseable defaults; we don't exercise strh here.
fn strh_video() -> Vec<u8> {
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids"); // fccType
    strh.extend_from_slice(b"MJPG"); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames (strh-side)
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

/// Assemble an entire AVI 1.0 file in memory with one video stream and
/// the requested `avih.dwInitialFrames` value LE-stamped at byte offset
/// 16 of the 56-byte AVIMAINHEADER body.
fn build_avi_with_avih_initial_frames(initial_frames: u32) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40000u32.to_le_bytes()); // dwMicroSecPerFrame (25fps)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&0x0010u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwTotalFrames
    avih.extend_from_slice(&initial_frames.to_le_bytes()); // dwInitialFrames (avih's own, body offset 16)
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    avih.extend_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih.extend_from_slice(&48u32.to_le_bytes()); // dwHeight
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    assert_eq!(avih.len(), 56);

    let strh_body = strh_video();
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
fn handrolled_explicit_nonzero_avih_initial_frames_decodes() {
    let buf = build_avi_with_avih_initial_frames(0xDEAD_BEEF);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.initial_frames(), Some(0xDEAD_BEEF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:initial_frames" && v == "3735928559"));
}

#[test]
fn handrolled_zero_avih_initial_frames_parses_as_none() {
    let buf = build_avi_with_avih_initial_frames(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.initial_frames(), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:initial_frames"));
}
