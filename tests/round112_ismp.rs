//! Round-112 `ISMP` (SMPTE timecode) AVI tests.
//!
//! `ISMP` is a member of the RIFF *Hdrl Tags* namespace — `TimeCode`
//! per `docs/container/riff/metadata/exiftool-riff-tags.html` §"RIFF
//! Hdrl Tags". It is a direct child chunk of `LIST hdrl` (the sibling
//! of `IDIT` / `avih` / `strl` / `LIST odml` / `LIST INFO`) carrying the
//! file's first-frame SMPTE timecode as a text string. The staged docs
//! do not pin a canonical on-disk format, so the demuxer surfaces the
//! body verbatim (trailing NUL / whitespace stripped, UTF-8-lossy) and
//! the muxer writes the caller's string verbatim. Exercises:
//!
//! - **Mux → demux round-trip** of a SMPTE non-drop-frame colon-form
//!   timecode via the typed `smpte_timecode()` accessor and the
//!   `avi:ismp` metadata key.
//! - **Drop-frame round-trip** (`;` before the frame field) showing the
//!   parser is format-agnostic.
//! - **No-ISMP baseline**: a file written without `with_smpte_timecode`
//!   carries no ISMP chunk; accessor returns `None` and no `avi:ismp`
//!   key is emitted; file is byte-smaller.
//! - **Empty string parses as `None`** (NUL-only body) so present-but-
//!   empty reads the same as absent.
//! - **Builder dedup**: the last `with_smpte_timecode(...)` wins.
//! - **ISMP + IDIT coexist**: both Hdrl Tags round-trip independently in
//!   the same file.
//! - **Hand-rolled fixture**: trailing-newline + multi-NUL padding is
//!   peeled off, surfacing the bare timecode.
//! - **Hand-rolled fixture**: an all-whitespace ISMP body parses as
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
// Round-trip: a SMPTE non-drop-frame timecode survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn ismp_smpte_roundtrip_accessor_and_metadata() {
    // SMPTE non-drop-frame "HH:MM:SS:FF" colon form.
    let tc = "01:00:00:00";
    let tmp = std::env::temp_dir().join("oxideav-avi-r112-ismp-ndf.avi");
    let opts = AviMuxOptions::new().with_smpte_timecode(tc);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.smpte_timecode(), Some(tc));

    let md = dmx.metadata();
    assert!(
        md.iter().any(|(k, v)| k == "avi:ismp" && v == tc),
        "missing avi:ismp metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k == "avi:ismp")
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// The parser is format-agnostic: a drop-frame ';' form round-trips too.
// ---------------------------------------------------------------------------

#[test]
fn ismp_dropframe_roundtrip() {
    // SMPTE drop-frame uses ';' before the frame field.
    let tc = "01:00:00;02";
    let tmp = std::env::temp_dir().join("oxideav-avi-r112-ismp-df.avi");
    let opts = AviMuxOptions::new().with_smpte_timecode(tc);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.smpte_timecode(), Some(tc));
}

// ---------------------------------------------------------------------------
// No-ISMP baseline: pre-round-112 byte layout (no builder call).
// ---------------------------------------------------------------------------

#[test]
fn no_ismp_yields_none_and_no_meta_key_and_smaller_file() {
    let baseline = std::env::temp_dir().join("oxideav-avi-r112-no-ismp.avi");
    write_minimal(&baseline, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&baseline).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.smpte_timecode(), None);
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k == "avi:ismp"),
        "no `with_smpte_timecode` ⇒ no `avi:ismp` key"
    );

    // Setting the timecode must grow the file by at least one chunk
    // (header + body), confirming the ISMP chunk is only emitted when
    // requested.
    let stamped = std::env::temp_dir().join("oxideav-avi-r112-stamped.avi");
    write_minimal(
        &stamped,
        AviMuxOptions::new().with_smpte_timecode("01:00:00:00"),
    );
    let baseline_len = std::fs::metadata(&baseline).unwrap().len();
    let stamped_len = std::fs::metadata(&stamped).unwrap().len();
    assert!(
        stamped_len > baseline_len,
        "an ISMP chunk must grow the file: baseline={baseline_len}, stamped={stamped_len}"
    );
}

// ---------------------------------------------------------------------------
// Empty string emits a NUL-only body that parses back as `None`.
// ---------------------------------------------------------------------------

#[test]
fn empty_ismp_payload_parses_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r112-ismp-empty.avi");
    let opts = AviMuxOptions::new().with_smpte_timecode("");
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.smpte_timecode(),
        None,
        "empty ISMP body must parse as None"
    );
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k == "avi:ismp"),
        "empty ISMP body emits no avi:ismp metadata key"
    );
}

// ---------------------------------------------------------------------------
// Builder dedup: the last `with_smpte_timecode(...)` wins.
// ---------------------------------------------------------------------------

#[test]
fn with_smpte_timecode_dedups() {
    let opts = AviMuxOptions::new()
        .with_smpte_timecode("00:00:00:00")
        .with_smpte_timecode("01:00:00:00");
    assert_eq!(opts.smpte_timecode.as_deref(), Some("01:00:00:00"));
}

// ---------------------------------------------------------------------------
// ISMP and IDIT are independent Hdrl Tags; both round-trip in one file.
// ---------------------------------------------------------------------------

#[test]
fn ismp_and_idit_coexist() {
    let tc = "01:23:45:12";
    let date = "Wed Jan 02 02:03:55 2002";
    let tmp = std::env::temp_dir().join("oxideav-avi-r112-ismp-idit.avi");
    let opts = AviMuxOptions::new()
        .with_smpte_timecode(tc)
        .with_digitization_date(date);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.smpte_timecode(), Some(tc));
    assert_eq!(dmx.digitization_date(), Some(date));

    let md = dmx.metadata();
    assert!(md.iter().any(|(k, v)| k == "avi:ismp" && v == tc));
    assert!(md.iter().any(|(k, v)| k == "avi:idit" && v == date));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: build a complete tiny AVI carrying an ISMP chunk
// whose body is terminated with "\n\0" and padded with an extra NUL. The
// demuxer must peel the trailing newline + NULs and surface the bare
// timecode.
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

/// Assemble a one-stream MJPG AVI whose `hdrl` carries a raw `ISMP`
/// chunk with the exact `ismp_body` bytes (no implicit terminator —
/// caller controls the trailing bytes).
fn build_avi_with_raw_ismp(ismp_body: &[u8]) -> Vec<u8> {
    // strl = strh + strf.
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_vids());
    push_chunk(&mut strl_body, b"strf", &strf_mjpg());
    let strl = list(b"strl", &strl_body);

    // hdrl = avih + strl + ISMP.
    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih_one_stream());
    hdrl_body.extend_from_slice(&strl);
    push_chunk(&mut hdrl_body, b"ISMP", ismp_body);
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
fn ismp_trailing_newline_and_nuls_stripped() {
    // SMPTE colon form with a trailing newline, plus a NUL terminator,
    // plus an extra NUL pad. cb = 13 (odd, RIFF would pad — but we
    // append the pad explicitly to control bytes, making cb even).
    let tc = "01:00:00:00";
    let mut body = tc.as_bytes().to_vec();
    body.push(b'\n');
    body.push(0);
    body.push(0);
    assert!(body.len() % 2 == 0, "test fixture body should be even");

    let bytes = build_avi_with_raw_ismp(&body);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.smpte_timecode(),
        Some(tc),
        "trailing newline + NUL padding must be stripped"
    );
}

#[test]
fn ismp_all_whitespace_body_parses_as_none() {
    // A body of only spaces / tabs / NULs has no usable timecode.
    let body = b"   \t \0\0".to_vec();
    let bytes = build_avi_with_raw_ismp(&body);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.smpte_timecode(), None);
    assert!(!dmx.metadata().iter().any(|(k, _)| k == "avi:ismp"));
}
