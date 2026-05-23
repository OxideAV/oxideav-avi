//! Round-107 `IDIT` (digitization-date) AVI tests.
//!
//! `IDIT` is a member of the RIFF *Hdrl Tags* namespace —
//! `DateTimeOriginal` per
//! `docs/container/riff/metadata/exiftool-riff-tags.html` §"RIFF Hdrl
//! Tags". It is a direct child chunk of `LIST hdrl` (a sibling of
//! `avih` / `strl` / `LIST odml` / `LIST INFO`) carrying the capture /
//! digitization timestamp as a text string. The staged docs do not pin
//! a canonical on-disk format, so the demuxer surfaces the body
//! verbatim (trailing NUL / whitespace stripped, UTF-8-lossy) and the
//! muxer writes the caller's string verbatim. Exercises:
//!
//! - **Mux → demux round-trip** of an `asctime`-style timestamp via the
//!   typed `digitization_date()` accessor and the `avi:idit` metadata
//!   key.
//! - **ISO-8601 round-trip** showing the parser is format-agnostic.
//! - **No-IDIT baseline**: a file written without
//!   `with_digitization_date` carries no IDIT chunk; accessor returns
//!   `None` and no `avi:idit` key is emitted; file is byte-smaller.
//! - **Empty string parses as `None`** (NUL-only body) so present-but-
//!   empty reads the same as absent.
//! - **Builder dedup**: the last `with_digitization_date(...)` wins.
//! - **Hand-rolled fixture**: trailing-newline + multi-NUL padding
//!   (the C `asctime` "...\n\0" form padded to a WORD boundary) is
//!   peeled off, surfacing the bare timestamp.
//! - **Hand-rolled fixture**: an all-whitespace IDIT body parses as
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
    params.width = Some(16);
    params.height = Some(16);
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
// Round-trip: an asctime-style IDIT survives mux → demux byte-equal.
// ---------------------------------------------------------------------------

#[test]
fn idit_asctime_roundtrip_accessor_and_metadata() {
    // The VfW asctime form most capture filters emit.
    let date = "Wed Jan 02 02:03:55 2002";
    let tmp = std::env::temp_dir().join("oxideav-avi-r107-idit-asctime.avi");
    let opts = AviMuxOptions::new().with_digitization_date(date);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.digitization_date(), Some(date));

    let md = dmx.metadata();
    assert!(
        md.iter().any(|(k, v)| k == "avi:idit" && v == date),
        "missing avi:idit metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k == "avi:idit")
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// The parser is format-agnostic: ISO-8601 round-trips just the same.
// ---------------------------------------------------------------------------

#[test]
fn idit_iso8601_roundtrip() {
    let date = "2002-01-02T02:03:55";
    let tmp = std::env::temp_dir().join("oxideav-avi-r107-idit-iso.avi");
    let opts = AviMuxOptions::new().with_digitization_date(date);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.digitization_date(), Some(date));
}

// ---------------------------------------------------------------------------
// No-IDIT baseline: pre-round-107 byte layout (no builder call).
// ---------------------------------------------------------------------------

#[test]
fn no_idit_yields_none_and_no_meta_key_and_smaller_file() {
    let baseline = std::env::temp_dir().join("oxideav-avi-r107-no-idit.avi");
    write_minimal(&baseline, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&baseline).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.digitization_date(), None);
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k == "avi:idit"),
        "no `with_digitization_date` ⇒ no `avi:idit` key"
    );

    // Setting the date must grow the file by at least one chunk
    // (header + body), confirming the IDIT chunk is only emitted when
    // requested.
    let dated = std::env::temp_dir().join("oxideav-avi-r107-dated.avi");
    write_minimal(
        &dated,
        AviMuxOptions::new().with_digitization_date("2002-01-02T02:03:55"),
    );
    let baseline_len = std::fs::metadata(&baseline).unwrap().len();
    let dated_len = std::fs::metadata(&dated).unwrap().len();
    assert!(
        dated_len > baseline_len,
        "an IDIT chunk must grow the file: baseline={baseline_len}, dated={dated_len}"
    );
}

// ---------------------------------------------------------------------------
// Empty string emits a NUL-only body that parses back as `None`.
// ---------------------------------------------------------------------------

#[test]
fn empty_idit_payload_parses_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r107-idit-empty.avi");
    let opts = AviMuxOptions::new().with_digitization_date("");
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.digitization_date(),
        None,
        "empty IDIT body must parse as None"
    );
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k == "avi:idit"),
        "empty IDIT body emits no avi:idit metadata key"
    );
}

// ---------------------------------------------------------------------------
// Builder dedup: the last `with_digitization_date(...)` wins.
// ---------------------------------------------------------------------------

#[test]
fn with_digitization_date_dedups() {
    let opts = AviMuxOptions::new()
        .with_digitization_date("first")
        .with_digitization_date("second");
    assert_eq!(opts.digitization_date.as_deref(), Some("second"));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: build a complete tiny AVI carrying an IDIT chunk
// whose body is the asctime form terminated with "\n\0" and padded with
// an extra NUL. The demuxer must peel the trailing newline + NULs and
// surface the bare timestamp.
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

/// Minimal AVISTREAMHEADER (`strh`) for a `vids` MJPG stream.
fn strh_vids() -> Vec<u8> {
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
    b.extend_from_slice(&[0u8; 8]); // rcFrame (4 × u16)
    b
}

/// Minimal BITMAPINFOHEADER (`strf`) for a 16×16 MJPG stream.
fn strf_mjpg() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&40u32.to_le_bytes()); // biSize
    b.extend_from_slice(&16i32.to_le_bytes()); // biWidth
    b.extend_from_slice(&16i32.to_le_bytes()); // biHeight
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

/// AVIMAINHEADER (`avih`, 56 bytes) with one stream.
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
    b.extend_from_slice(&16u32.to_le_bytes()); // dwWidth
    b.extend_from_slice(&16u32.to_le_bytes()); // dwHeight
    b.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    b
}

/// Assemble a one-stream MJPG AVI whose `hdrl` carries a raw `IDIT`
/// chunk with the exact `idit_body` bytes (no implicit terminator —
/// caller controls the trailing bytes).
fn build_avi_with_raw_idit(idit_body: &[u8]) -> Vec<u8> {
    // strl = strh + strf.
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_vids());
    push_chunk(&mut strl_body, b"strf", &strf_mjpg());
    let strl = list(b"strl", &strl_body);

    // hdrl = avih + strl + IDIT.
    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih_one_stream());
    hdrl_body.extend_from_slice(&strl);
    push_chunk(&mut hdrl_body, b"IDIT", idit_body);
    let hdrl = list(b"hdrl", &hdrl_body);

    // movi = one 00dc keyframe.
    let mut movi_body = Vec::new();
    push_chunk(&mut movi_body, b"00dc", &[0x55u8; 4]);
    let movi = list(b"movi", &movi_body);

    // RIFF AVI body.
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
fn idit_trailing_newline_and_nuls_stripped() {
    // asctime form with the conventional trailing newline, plus a NUL
    // terminator, plus an extra NUL pad. cb = 26 (even, no RIFF pad).
    let date = "Wed Jan 02 02:03:55 2002";
    let mut body = date.as_bytes().to_vec();
    body.push(b'\n');
    body.push(0);
    assert!(body.len() % 2 == 0, "test fixture body should be even");

    let bytes = build_avi_with_raw_idit(&body);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.digitization_date(),
        Some(date),
        "trailing newline + NUL padding must be stripped"
    );
}

#[test]
fn idit_all_whitespace_body_parses_as_none() {
    // A body of only spaces / tabs / NULs has no usable timestamp.
    let body = b"   \t \0\0".to_vec();
    let bytes = build_avi_with_raw_idit(&body);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.digitization_date(), None);
    assert!(!dmx.metadata().iter().any(|(k, _)| k == "avi:idit"));
}
