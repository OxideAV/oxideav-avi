//! Round-298 file-global `avih.dwSuggestedBufferSize` typed accessor.
//!
//! `dwSuggestedBufferSize` is the 32-bit DWORD at byte offset 28 of the
//! 56-byte AVIMAINHEADER body per AVI 1.0 §"AVIMAINHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
//! `dwSuggestedBufferSize` row, line 202): *"Suggested buffer size for
//! reading the file. Generally, large enough to contain the largest
//! chunk in the file. If set to zero or too small, playback software
//! will have to reallocate memory during playback, which will reduce
//! performance. For interleaved files, the buffer size should be large
//! enough to read an entire record (not just a chunk)."*
//!
//! `0` is the writer-skips-it / "do not know" sentinel mapped here to
//! `None`, mirroring the round-292 `dwStreams` / round-275
//! `dwWidth`/`dwHeight` / round-268 `dwTotalFrames` / round-260
//! `dwMaxBytesPerSec` / round-256 `dwMicroSecPerFrame` "default ==
//! absent" idiom. The matching `avi:suggested_buffer_size = "<N>"`
//! metadata key already existed pre-round-298, alongside the legacy
//! bare-`u32` `avih_suggested_buffer_size()` accessor (round-13) which
//! cannot distinguish "stamped 0 because unknown" from "field held 0".
//! Round-298 adds the typed companion:
//!
//! - `AviDemuxer::avih_suggested_buffer_size_typed() -> Option<u32>`
//!   returning the verbatim 32-bit value at byte offset 28 of the
//!   56-byte AVIMAINHEADER body, with `0` folded to `None`.
//!
//! Exercises:
//!
//! - **Hand-rolled fixture**: an explicit non-zero
//!   `dwSuggestedBufferSize` → typed accessor surfaces it, legacy
//!   accessor agrees, metadata key present.
//! - **Hand-rolled fixture**: an all-zero `dwSuggestedBufferSize`
//!   parses as `None` via the typed accessor (legacy accessor returns
//!   `0`), and the metadata key is absent.
//! - **Independence from neighbouring AVIMAINHEADER DWORDs**: the
//!   round-268 `dwTotalFrames` (offset 16), round-157 `dwInitialFrames`
//!   (offset 20), round-292 `dwStreams` (offset 24) and round-275
//!   `dwWidth`/`dwHeight` (offset 32/36) all read back their own bytes
//!   alongside a stamped `dwSuggestedBufferSize`.
//! - **Mux override round-trip**: `AviMuxOptions::with_suggested_buffer_size`
//!   stamps a value surfaced verbatim by the typed accessor, and an
//!   explicit `0` override maps back to `None`.

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

// ---------------------------------------------------------------------------
// Hand-rolled fixture builders: control the exact avih bytes.
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

/// Build a 56-byte AVISTREAMHEADER body for a video stream.
fn strh_video() -> Vec<u8> {
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

/// Build the 56-byte AVIMAINHEADER body with the requested
/// `dwSuggestedBufferSize` LE-stamped at body offset 28, plus distinct
/// values in the neighbouring DWORDs (offset 16 `dwTotalFrames`, offset
/// 20 `dwInitialFrames`, offset 24 `dwStreams`, offset 32/36
/// `dwWidth`/`dwHeight`) so the independence test can read each field
/// back.
fn avih_body(suggested_buffer_size: u32) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame (offset 0)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec (offset 4)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity (offset 8)
    avih.extend_from_slice(&0x0010u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX (offset 12)
    avih.extend_from_slice(&7u32.to_le_bytes()); // dwTotalFrames (offset 16)
    avih.extend_from_slice(&2u32.to_le_bytes()); // dwInitialFrames (offset 20)
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams (offset 24)
    avih.extend_from_slice(&suggested_buffer_size.to_le_bytes()); // dwSuggestedBufferSize (offset 28)
    avih.extend_from_slice(&64u32.to_le_bytes()); // dwWidth (offset 32)
    avih.extend_from_slice(&48u32.to_le_bytes()); // dwHeight (offset 36)
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    assert_eq!(avih.len(), 56);
    avih
}

/// Assemble an entire AVI 1.0 file in memory with one video `strl` LIST
/// and the requested `avih.dwSuggestedBufferSize`.
fn build_avi(suggested_buffer_size: u32) -> Vec<u8> {
    let avih = avih_body(suggested_buffer_size);

    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih);
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_video());
    push_chunk(&mut strl_body, b"strf", &strf_video_mjpg());
    hdrl_body.extend_from_slice(&list(b"strl", &strl_body));
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

// ---------------------------------------------------------------------------
// Hand-rolled non-zero: typed accessor surfaces it, legacy agrees,
// metadata key present.
// ---------------------------------------------------------------------------

#[test]
fn handrolled_nonzero_surfaces_via_typed_and_legacy_and_metadata() {
    let buf = build_avi(65_536);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_suggested_buffer_size_typed(), Some(65_536));
    assert_eq!(
        dmx.avih_suggested_buffer_size(),
        65_536,
        "legacy bare-u32 accessor agrees on the non-zero value"
    );
    assert!(
        dmx.metadata()
            .iter()
            .any(|(k, v)| k == "avi:suggested_buffer_size" && v == "65536"),
        "missing avi:suggested_buffer_size=65536; got {:?}",
        dmx.metadata()
            .iter()
            .filter(|(k, _)| k.contains("suggested"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn typed_accessor_agrees_with_metadata_key() {
    let buf = build_avi(12_345);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let from_accessor = dmx.avih_suggested_buffer_size_typed();
    assert_eq!(from_accessor, Some(12_345));
    let from_md: Option<u32> = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "avi:suggested_buffer_size")
        .and_then(|(_, v)| v.parse::<u32>().ok());
    assert_eq!(
        from_accessor, from_md,
        "typed accessor must agree with avi:suggested_buffer_size metadata"
    );
}

// ---------------------------------------------------------------------------
// Hand-rolled zero: typed accessor None, legacy returns 0, metadata key
// absent.
// ---------------------------------------------------------------------------

#[test]
fn handrolled_zero_parses_as_none() {
    let buf = build_avi(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.avih_suggested_buffer_size_typed(),
        None,
        "an all-zero avih.dwSuggestedBufferSize must read back as None"
    );
    assert_eq!(
        dmx.avih_suggested_buffer_size(),
        0,
        "legacy accessor still returns the raw 0"
    );
    assert!(
        !dmx.metadata()
            .iter()
            .any(|(k, _)| k == "avi:suggested_buffer_size"),
        "the metadata key is omitted entirely for the 0 sentinel"
    );
}

// ---------------------------------------------------------------------------
// Independence from neighbouring AVIMAINHEADER DWORDs.
// ---------------------------------------------------------------------------

#[test]
fn avih_suggested_buffer_size_independent_of_neighbouring_fields() {
    let buf = build_avi(0x0001_0000);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_total_frames(), Some(7)); // offset 16 (round-268)
    assert_eq!(dmx.initial_frames(), Some(2)); // offset 20 (round-157)
    assert_eq!(dmx.avih_declared_stream_count(), Some(1)); // offset 24 (round-292)
    assert_eq!(dmx.avih_suggested_buffer_size_typed(), Some(0x0001_0000)); // offset 28 (round-298)
    assert_eq!(dmx.avih_movie_rect(), Some((64, 48))); // offset 32/36 (round-275)
}

// ---------------------------------------------------------------------------
// Mux override round-trip.
// ---------------------------------------------------------------------------

fn write_two_stream_avi(path: &std::path::Path, opts: AviMuxOptions) {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, opts).unwrap();
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

#[test]
fn mux_override_roundtrips_via_typed_accessor() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r298-sbs-override.avi");
    write_two_stream_avi(
        &tmp,
        AviMuxOptions::new().with_suggested_buffer_size(98_304),
    );

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.avih_suggested_buffer_size_typed(),
        Some(98_304),
        "an explicit with_suggested_buffer_size override must round-trip verbatim"
    );
}

#[test]
fn mux_zero_override_maps_back_to_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r298-sbs-zero.avi");
    write_two_stream_avi(&tmp, AviMuxOptions::new().with_suggested_buffer_size(0));

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.avih_suggested_buffer_size_typed(),
        None,
        "an explicit 0 override stamps the sentinel — typed accessor maps it back to None"
    );
    assert_eq!(dmx.avih_suggested_buffer_size(), 0);
}

#[test]
fn mux_auto_derived_nonzero_roundtrips() {
    // With no override the muxer auto-derives the largest chunk-body
    // size across all streams (the 64-byte video packet here), so the
    // typed accessor surfaces a non-zero value.
    let tmp = std::env::temp_dir().join("oxideav-avi-r298-sbs-auto.avi");
    write_two_stream_avi(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let v = dmx.avih_suggested_buffer_size_typed();
    assert!(
        matches!(v, Some(n) if n >= 64),
        "auto-derived avih.dwSuggestedBufferSize should cover the largest chunk; got {v:?}"
    );
}
