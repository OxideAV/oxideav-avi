//! Round-260 file-global `avih.dwMaxBytesPerSec` typed accessor.
//!
//! `dwMaxBytesPerSec` is the 32-bit DWORD at byte offset 4 of the
//! 56-byte AVIMAINHEADER body (byte 12 of the `avih` chunk) per AVI 1.0
//! §"AVIMAINHEADER" (`docs/container/riff/avi-riff-file-reference.md`,
//! Appendix A `dwMaxBytesPerSec` row, line 196): *"Approximate maximum
//! data rate of the file. Number of bytes per second the system must
//! handle to present an AVI sequence as specified by the other
//! parameters in the main header and stream header chunks."*
//!
//! `0` is the writer-skips-it sentinel mapped here to `None`, mirroring
//! the round-256 `dwMicroSecPerFrame` / round-249 `(dwScale, dwRate)` /
//! round-247 `dwFlags` / round-229 `dwLength` / round-153
//! `dwInitialFrames` / round-119 `wLanguage` / round-115 `rcFrame` /
//! round-80 `strn` "default == absent" idiom.
//!
//! Pre-round-260 the demuxer already parsed `dwMaxBytesPerSec` and
//! surfaced it as the `avi:max_bytes_per_sec` decimal metadata key
//! (round-14), and the muxer's
//! [`AviMuxOptions::with_max_bytes_per_sec`] builder was already
//! wired (round-14). Round-260 closes the typed-accessor gap so a
//! caller can reach `Option<u32>` without scanning the metadata Vec:
//!
//! - `AviDemuxer::max_bytes_per_sec() -> Option<u32>` raw accessor
//!   returning the verbatim 32-bit value at byte offset 4 of the
//!   56-byte AVIMAINHEADER body.
//! - The all-zero sentinel maps to `None`, the metadata key is omitted
//!   in the same case, and `max_bytes_per_sec()` agrees with the
//!   metadata view byte-for-byte.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip** of a non-default rate via the typed
//!   accessor and the metadata key.
//! - **No-override baseline**: with no override, the muxer's auto-
//!   computed value is surfaced verbatim via the accessor.
//! - **Builder idempotency**: the last `with_max_bytes_per_sec(...)`
//!   wins.
//! - **Explicit zero override** reads back as `None` and the metadata
//!   key is absent.
//! - **0xFFFF_FFFF round-trip**: every bit of the 32-bit field
//!   survives.
//! - **Independence**: the typed accessor doesn't perturb (or get
//!   perturbed by) the file-global `dwMicroSecPerFrame` from
//!   round-256, the file-global `dwPaddingGranularity` (round-92), or
//!   the per-stream `(dwScale, dwRate)` (round-249).
//! - **Hand-rolled fixture**: an explicit non-zero `dwMaxBytesPerSec`
//!   in a 56-byte avih decodes to the expected raw u32.
//! - **Hand-rolled fixture**: an all-zero `dwMaxBytesPerSec` parses
//!   as `None`.

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
// Round-trip: a non-default data-rate hint survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn avih_max_bytes_per_sec_override_roundtrip_accessor_and_metadata() {
    // 1_500_000 = ~1.5 MB/s nominal DV rate; deliberately doesn't match
    // the trivial fixture's computed default so the override path is
    // exercised end-to-end.
    let tmp = std::env::temp_dir().join("oxideav-avi-r260-avih-mbps.avi");
    let opts = AviMuxOptions::new().with_max_bytes_per_sec(1_500_000);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.max_bytes_per_sec(), Some(1_500_000));

    let md = dmx.metadata();
    assert!(
        md.iter()
            .any(|(k, v)| k == "avi:max_bytes_per_sec" && v == "1500000"),
        "missing avi:max_bytes_per_sec=1500000 metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k.contains("max_bytes_per_sec"))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Default baseline: no override ⇒ the muxer's auto-computed value
// surfaces verbatim via the typed accessor (so the accessor and the
// metadata key agree on the on-disk byte pattern, whatever the muxer
// computed).
// ---------------------------------------------------------------------------

#[test]
fn default_no_override_accessor_agrees_with_metadata_key() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r260-default-mbps.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Accessor returns whatever was written to disk. We don't pin the
    // exact muxer-computed value here (it depends on per-track byte
    // totals + the computed file duration); we only require accessor /
    // metadata agreement and a positive value (the fixture writes one
    // 64-byte video packet + one 8-byte audio packet at 25fps, so the
    // computed rate is comfortably > 0).
    let from_accessor = dmx.max_bytes_per_sec();
    assert!(from_accessor.is_some(), "computed default should be > 0");
    let from_md: Option<u32> = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "avi:max_bytes_per_sec")
        .and_then(|(_, v)| v.parse::<u32>().ok());
    assert_eq!(
        from_accessor, from_md,
        "typed accessor must agree with avi:max_bytes_per_sec metadata"
    );
}

// ---------------------------------------------------------------------------
// Builder idempotency: the last with_max_bytes_per_sec(...) wins.
// ---------------------------------------------------------------------------

#[test]
fn with_max_bytes_per_sec_last_call_wins() {
    let opts = AviMuxOptions::new()
        .with_max_bytes_per_sec(1_000_000)
        .with_max_bytes_per_sec(2_000_000);
    assert_eq!(opts.max_bytes_per_sec_override, Some(2_000_000));
}

// ---------------------------------------------------------------------------
// An explicit zero override reads back as None (zero == absent).
// ---------------------------------------------------------------------------

#[test]
fn explicit_zero_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r260-mbps-zero-override.avi");
    let opts = AviMuxOptions::new().with_max_bytes_per_sec(0);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.max_bytes_per_sec(),
        None,
        "an all-zero avih.dwMaxBytesPerSec must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:max_bytes_per_sec"));
}

// ---------------------------------------------------------------------------
// 0xFFFF_FFFF round-trip: every bit of the 32-bit field survives.
// ---------------------------------------------------------------------------

#[test]
fn all_bits_set_avih_max_bytes_per_sec_roundtrip() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r260-avih-mbps-all-bits.avi");
    let opts = AviMuxOptions::new().with_max_bytes_per_sec(0xFFFF_FFFF);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.max_bytes_per_sec(), Some(0xFFFF_FFFF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:max_bytes_per_sec" && v == "4294967295"));
}

// ---------------------------------------------------------------------------
// Independence: the rate accessor doesn't perturb neighbouring file-global
// AVIMAINHEADER fields (round-256 / round-92) or per-stream timebase
// (round-249).
// ---------------------------------------------------------------------------

#[test]
fn avih_max_bytes_per_sec_independent_of_other_avih_fields() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r260-mbps-vs-others.avi");
    // Stamp all four: max_bytes_per_sec + micro_sec_per_frame +
    // padding_granularity (these three live at non-overlapping byte
    // ranges inside the 56-byte AVIMAINHEADER body). Each must
    // round-trip independently and the per-stream (scale, rate) pair
    // must remain the packaging-derived video (1, 25) / audio
    // (1, 48000).
    let opts = AviMuxOptions::new()
        .with_max_bytes_per_sec(1_500_000)
        .with_micro_sec_per_frame(33333)
        .with_padding_granularity(2048);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.max_bytes_per_sec(), Some(1_500_000));
    assert_eq!(dmx.micro_sec_per_frame(), Some(33333));
    assert_eq!(dmx.padding_granularity(), 2048);
    assert_eq!(dmx.stream_timebase(0), Some((1, 25)));
    assert_eq!(dmx.stream_timebase(1), Some((1, 48_000)));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact avih dwMaxBytesPerSec bytes.
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

/// Assemble an entire AVI 1.0 file in memory with one video stream and
/// the requested `avih.dwMaxBytesPerSec` value LE-stamped at byte
/// offset 4 of the 56-byte AVIMAINHEADER body.
fn build_avi_with_avih_max_bytes_per_sec(max_bytes_per_sec: u32) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame (body offset 0)
    avih.extend_from_slice(&max_bytes_per_sec.to_le_bytes()); // dwMaxBytesPerSec (body offset 4)
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
fn handrolled_explicit_nonzero_avih_max_bytes_per_sec_decodes() {
    let buf = build_avi_with_avih_max_bytes_per_sec(0xDEAD_BEEF);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.max_bytes_per_sec(), Some(0xDEAD_BEEF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:max_bytes_per_sec" && v == "3735928559"));
}

#[test]
fn handrolled_zero_avih_max_bytes_per_sec_parses_as_none() {
    let buf = build_avi_with_avih_max_bytes_per_sec(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.max_bytes_per_sec(), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:max_bytes_per_sec"));
}
