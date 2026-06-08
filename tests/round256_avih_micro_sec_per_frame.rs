//! Round-256 file-global `avih.dwMicroSecPerFrame` AVI tests.
//!
//! `dwMicroSecPerFrame` is the 32-bit DWORD at byte offset 0 of the
//! 56-byte AVIMAINHEADER body (byte 8 of the `avih` chunk) per AVI 1.0
//! §"AVIMAINHEADER" (`docs/container/riff/avi-riff-file-reference.md`,
//! Appendix A `dwMicroSecPerFrame` row, line 195): *"Number of
//! microseconds between frames. Indicates the overall timing for the
//! file."*
//!
//! `0` is the writer-skips-it sentinel mapped here to `None`, mirroring
//! the round-249 `(dwScale, dwRate)` / round-247 `dwFlags` / round-229
//! `dwLength` / round-153 `dwInitialFrames` / round-119 `wLanguage` /
//! round-115 `rcFrame` / round-80 `strn` "default == absent" idiom.
//!
//! Pre-round-256 the demuxer already parsed `dwMicroSecPerFrame`
//! internally to derive `duration_micros = total_frames *
//! micro_sec_per_frame`, but did not surface the raw DWORD verbatim.
//! Round-256 adds:
//!
//! - `AviDemuxer::micro_sec_per_frame() -> Option<u32>` raw accessor
//!   returning the verbatim 32-bit value.
//! - `avi:micro_sec_per_frame = "<N>"` decimal metadata key (omitted
//!   when the field carried the all-zero sentinel).
//! - `AviMuxOptions::with_micro_sec_per_frame(n)` builder writing the
//!   supplied 32 bits verbatim at byte offset 0 of the 56-byte
//!   AVIMAINHEADER body — replacing the default value the muxer would
//!   otherwise derive from the first video stream's `(scale, rate)` as
//!   `1_000_000 * scale / rate`.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip** of a non-default frame period via the
//!   typed accessor and the metadata key.
//! - **No-override baseline**: with no override, the muxer keeps the
//!   computed `1_000_000 * stream0_scale / stream0_rate` value (40000
//!   for the 25fps fixture); the demuxer surfaces this verbatim under
//!   `Some(40000)` and via the metadata key.
//! - **Audio-only baseline + non-zero override**: an audio-only file
//!   has no video stream, so the computed default is `0` → demuxer
//!   reads `None`. A non-zero override stamps a nominal period anyway.
//! - **Builder idempotency**: the last `with_micro_sec_per_frame(...)`
//!   wins.
//! - **Explicit zero override** reads back as `None` (default == absent).
//! - **0xFFFF_FFFF round-trip**: every bit of the 32-bit field survives.
//! - **Independence from per-stream timebase**: stamping the file-global
//!   value doesn't perturb the per-stream `strh.{dwScale, dwRate}`
//!   pair surfaced via `stream_timebase` (round-249).
//! - **Hand-rolled fixture**: an explicit non-zero
//!   `dwMicroSecPerFrame` in a 56-byte avih decodes to the expected
//!   raw u32.
//! - **Hand-rolled fixture**: an all-zero `dwMicroSecPerFrame` parses
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

fn write_audio_only(path: &std::path::Path, options: AviMuxOptions) {
    let streams = [audio_stream(0)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();

    let mut a = Packet::new(0, streams[0].time_base, vec![0u8; 8]);
    a.pts = Some(0);
    a.flags.keyframe = true;
    mux.write_packet(&a).unwrap();

    mux.write_trailer().unwrap();
}

// ---------------------------------------------------------------------------
// Round-trip: a non-default frame period survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn avih_micro_sec_per_frame_override_roundtrip_accessor_and_metadata() {
    // 33333 = ~30fps NTSC nominal frame period; deliberately doesn't
    // match the 25fps video stream's computed default of 40000, so the
    // override path is exercised (not the same value the muxer would
    // have computed anyway).
    let tmp = std::env::temp_dir().join("oxideav-avi-r256-avih-upf.avi");
    let opts = AviMuxOptions::new().with_micro_sec_per_frame(33333);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.micro_sec_per_frame(), Some(33333));

    let md = dmx.metadata();
    assert!(
        md.iter()
            .any(|(k, v)| k == "avi:micro_sec_per_frame" && v == "33333"),
        "missing avi:micro_sec_per_frame=33333 metadata key; got {:?}",
        md.iter()
            .filter(|(k, _)| k.contains("micro_sec_per_frame"))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Default baseline: no override ⇒ muxer computes 1_000_000 * scale / rate.
// For the 25fps fixture, that's 40000us per frame; surfaces as Some(40000)
// AND under the metadata key (40000 is not the sentinel).
// ---------------------------------------------------------------------------

#[test]
fn default_no_override_reads_as_computed_frame_period() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r256-default-upf.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // 25fps video → 1_000_000 / 25 = 40000 microseconds per frame.
    assert_eq!(dmx.micro_sec_per_frame(), Some(40000));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:micro_sec_per_frame" && v == "40000"));
}

// ---------------------------------------------------------------------------
// Audio-only baseline: no video stream → computed default is 0 → None.
// A non-zero override stamps the nominal frame period anyway.
// ---------------------------------------------------------------------------

#[test]
fn audio_only_no_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r256-audio-only-default.avi");
    write_audio_only(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // No video stream → muxer computed micro_per_frame = 0 → demuxer None.
    assert_eq!(
        dmx.micro_sec_per_frame(),
        None,
        "audio-only file should default to dwMicroSecPerFrame = 0 → None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:micro_sec_per_frame"));
}

#[test]
fn audio_only_with_override_stamps_nominal_period() {
    // Audio-only fixtures sometimes still want to advertise a nominal
    // frame period (e.g. 1ms / 1000us per "frame" for a sub-millisecond
    // pacing hint). Round-256's override is the only way to express that
    // without a video stream — the muxer's default for audio-only is 0.
    let tmp = std::env::temp_dir().join("oxideav-avi-r256-audio-only-override.avi");
    let opts = AviMuxOptions::new().with_micro_sec_per_frame(1000);
    write_audio_only(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.micro_sec_per_frame(), Some(1000));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:micro_sec_per_frame" && v == "1000"));
}

// ---------------------------------------------------------------------------
// Builder idempotency: the last with_micro_sec_per_frame(...) wins.
// ---------------------------------------------------------------------------

#[test]
fn with_micro_sec_per_frame_last_call_wins() {
    let opts = AviMuxOptions::new()
        .with_micro_sec_per_frame(40000)
        .with_micro_sec_per_frame(33333);
    assert_eq!(opts.micro_sec_per_frame_override, Some(33333));
}

// ---------------------------------------------------------------------------
// An explicit zero override reads back as None (zero == absent).
// ---------------------------------------------------------------------------

#[test]
fn explicit_zero_override_reads_as_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r256-upf-zero-override.avi");
    let opts = AviMuxOptions::new().with_micro_sec_per_frame(0);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.micro_sec_per_frame(),
        None,
        "an all-zero avih.dwMicroSecPerFrame must read back as None"
    );
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:micro_sec_per_frame"));
}

// ---------------------------------------------------------------------------
// 0xFFFF_FFFF round-trip: every bit of the 32-bit field survives.
// ---------------------------------------------------------------------------

#[test]
fn all_bits_set_avih_micro_sec_per_frame_roundtrip() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r256-avih-upf-all-bits.avi");
    let opts = AviMuxOptions::new().with_micro_sec_per_frame(0xFFFF_FFFF);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.micro_sec_per_frame(), Some(0xFFFF_FFFF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:micro_sec_per_frame" && v == "4294967295"));
}

// ---------------------------------------------------------------------------
// Independence: file-global override doesn't perturb per-stream timebase.
// The avih DWORD lives at body offset 0 of avih, the per-stream
// (dwScale, dwRate) DWORDs at body offsets 20 and 24 of each strh —
// different bytes, round-trip independently. Round-249 surfaces the
// per-stream pair via stream_timebase().
// ---------------------------------------------------------------------------

#[test]
fn avih_micro_sec_per_frame_and_strh_timebase_are_independent() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r256-upf-vs-strh.avi");
    // Stamp the file-global frame period to a value that deliberately
    // disagrees with the 25fps video stream's natural computation
    // (40000us). Per-stream (scale, rate) must remain the packaging-
    // derived (1, 25) for the video stream.
    let opts = AviMuxOptions::new().with_micro_sec_per_frame(33333);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // File-global avih.dwMicroSecPerFrame = 33333 (the override).
    assert_eq!(dmx.micro_sec_per_frame(), Some(33333));
    // Per-stream (scale, rate) untouched: video stream 0 still
    // packaging-derived (1, 25) → 25fps.
    assert_eq!(dmx.stream_timebase(0), Some((1, 25)));
    // Audio stream 1: (1, 48000) sample rate.
    assert_eq!(dmx.stream_timebase(1), Some((1, 48_000)));
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact avih dwMicroSecPerFrame bytes.
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
/// the requested `avih.dwMicroSecPerFrame` value LE-stamped at byte
/// offset 0 of the 56-byte AVIMAINHEADER body.
fn build_avi_with_avih_micro_sec_per_frame(micro_sec_per_frame: u32) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&micro_sec_per_frame.to_le_bytes()); // dwMicroSecPerFrame (body offset 0)
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
fn handrolled_explicit_nonzero_avih_micro_sec_per_frame_decodes() {
    let buf = build_avi_with_avih_micro_sec_per_frame(0xDEAD_BEEF);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.micro_sec_per_frame(), Some(0xDEAD_BEEF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:micro_sec_per_frame" && v == "3735928559"));
}

#[test]
fn handrolled_zero_avih_micro_sec_per_frame_parses_as_none() {
    let buf = build_avi_with_avih_micro_sec_per_frame(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.micro_sec_per_frame(), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:micro_sec_per_frame"));
}
