//! Round-292 file-global `avih.dwStreams` typed accessor +
//! declared-vs-actual stream-count cross-check.
//!
//! `dwStreams` is the 32-bit DWORD at byte offset 24 of the 56-byte
//! AVIMAINHEADER body per AVI 1.0 §"AVIMAINHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
//! `dwStreams` row, line 201): *"Number of streams in the file. For
//! example, a file with audio and video has two streams."*
//!
//! `0` is the writer-skips-it / unspecified sentinel mapped here to
//! `None`, mirroring the round-275 `dwWidth`/`dwHeight` / round-268
//! `dwTotalFrames` / round-260 `dwMaxBytesPerSec` / round-256
//! `dwMicroSecPerFrame` "default == absent" idiom. The matching
//! `avi:streams = "<N>"` metadata key already existed pre-round-292;
//! round-292 adds the two typed surfaces:
//!
//! - `AviDemuxer::avih_declared_stream_count() -> Option<u32>` raw
//!   accessor returning the verbatim 32-bit value at byte offset 24 of
//!   the 56-byte AVIMAINHEADER body.
//! - `AviDemuxer::declared_vs_actual_stream_count_mismatch()
//!   -> Option<(u32, u32)>` informational cross-check, returning
//!   `(declared, actual)` whenever the non-zero declared count disagrees
//!   with the number of `strl` LISTs the demuxer actually walked in
//!   `hdrl`. The mismatch is a hallmark of a truncated capture crash
//!   dump (header stamped up-front for N streams, file cut off before
//!   all N `strl` LISTs were written) or a hand-edited header; it never
//!   fails `open()`. The demuxer always trusts the streams it actually
//!   parsed — `dwStreams` is advisory.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip**: the muxer's auto-derived stamp
//!   (`tracks.len()`, written at `write_header`) surfaces verbatim via
//!   the typed accessor + metadata key, and the cross-check reports no
//!   mismatch for a well-formed file.
//! - **Hand-rolled fixture**: an explicit non-zero `dwStreams` matching
//!   the physical `strl` count → accessor surfaces it, no mismatch.
//! - **Hand-rolled fixture**: an all-zero `dwStreams` parses as `None`,
//!   the metadata key is absent, and the cross-check is silent (no claim
//!   to validate against).
//! - **Hand-rolled fixture**: a `dwStreams` over-declaring the physical
//!   `strl` count (the truncated-capture shape) → accessor surfaces the
//!   declared count and the cross-check returns `(declared, actual)`.
//! - **Independence from neighbouring AVIMAINHEADER DWORDs**: the
//!   round-268 `dwTotalFrames` (offset 16), round-157 `dwInitialFrames`
//!   (offset 20) and round-275 `dwWidth`/`dwHeight` (offset 32/36) all
//!   read back their own bytes alongside a stamped `dwStreams`.

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

/// Mux a video+audio AVI 1.0 file (two streams), so the muxer's
/// auto-derived `dwStreams` stamp is `2`.
fn write_two_stream_avi(path: &std::path::Path) {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::new()).unwrap();
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
// Mux → demux round-trip: the muxer's auto-derived dwStreams stamp
// (tracks.len(), written at write_header) surfaces verbatim via the
// typed accessor + metadata key, and the cross-check is silent.
// ---------------------------------------------------------------------------

#[test]
fn muxer_auto_stamp_roundtrips_via_accessor_and_metadata() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r292-avih-streams.avi");
    write_two_stream_avi(&tmp);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_declared_stream_count(), Some(2));
    assert_eq!(dmx.streams().len(), 2);
    assert_eq!(
        dmx.declared_vs_actual_stream_count_mismatch(),
        None,
        "a well-formed two-stream file declares exactly what it carries"
    );
    assert!(
        dmx.metadata()
            .iter()
            .any(|(k, v)| k == "avi:streams" && v == "2"),
        "missing avi:streams=2 metadata key; got {:?}",
        dmx.metadata()
            .iter()
            .filter(|(k, _)| k.contains("streams"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn accessor_agrees_with_metadata_key() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r292-streams-agree.avi");
    write_two_stream_avi(&tmp);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let from_accessor = dmx.avih_declared_stream_count();
    assert!(from_accessor.is_some(), "two-stream fixture stamps a count");
    let from_md: Option<u32> = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "avi:streams")
        .and_then(|(_, v)| v.parse::<u32>().ok());
    assert_eq!(
        from_accessor, from_md,
        "typed accessor must agree with avi:streams metadata"
    );
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact avih dwStreams bytes and the
// number of physical `strl` LISTs.
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

/// Build the 56-byte AVIMAINHEADER body with the requested `dwStreams`
/// LE-stamped at body offset 24, plus distinct values in the
/// neighbouring DWORDs (offset 16 `dwTotalFrames`, offset 20
/// `dwInitialFrames`, offset 32/36 `dwWidth`/`dwHeight`) so the
/// independence test can read each field back.
fn avih_body(streams: u32) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame (offset 0)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec (offset 4)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity (offset 8)
    avih.extend_from_slice(&0x0010u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX (offset 12)
    avih.extend_from_slice(&7u32.to_le_bytes()); // dwTotalFrames (offset 16)
    avih.extend_from_slice(&2u32.to_le_bytes()); // dwInitialFrames (offset 20)
    avih.extend_from_slice(&streams.to_le_bytes()); // dwStreams (offset 24)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize (offset 28)
    avih.extend_from_slice(&64u32.to_le_bytes()); // dwWidth (offset 32)
    avih.extend_from_slice(&48u32.to_le_bytes()); // dwHeight (offset 36)
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    assert_eq!(avih.len(), 56);
    avih
}

/// Assemble an entire AVI 1.0 file in memory with `n_strl` physical
/// video `strl` LISTs and the requested `avih.dwStreams` declared count
/// (which may deliberately disagree with `n_strl`).
fn build_avi(declared_streams: u32, n_strl: u32) -> Vec<u8> {
    let avih = avih_body(declared_streams);

    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih);
    for _ in 0..n_strl {
        let mut strl_body = Vec::new();
        push_chunk(&mut strl_body, b"strh", &strh_video());
        push_chunk(&mut strl_body, b"strf", &strf_video_mjpg());
        hdrl_body.extend_from_slice(&list(b"strl", &strl_body));
    }
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
fn handrolled_declared_matches_actual_no_mismatch() {
    // dwStreams = 2 and exactly two physical `strl` LISTs → accessor
    // surfaces 2, cross-check is silent.
    let buf = build_avi(2, 2);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_declared_stream_count(), Some(2));
    assert_eq!(dmx.streams().len(), 2);
    assert_eq!(dmx.declared_vs_actual_stream_count_mismatch(), None);
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:streams" && v == "2"));
}

#[test]
fn handrolled_zero_dwstreams_parses_as_none_no_claim() {
    // dwStreams = 0 (writer-skips-it sentinel) with one physical
    // `strl` → accessor is None, metadata key absent, and the
    // cross-check is silent (a 0 declared count carries no claim).
    let buf = build_avi(0, 1);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.avih_declared_stream_count(),
        None,
        "an all-zero avih.dwStreams must read back as None"
    );
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(
        dmx.declared_vs_actual_stream_count_mismatch(),
        None,
        "a 0 declared count is not a mismatch — it makes no claim"
    );
    assert!(!dmx.metadata().iter().any(|(k, _)| k == "avi:streams"));
}

#[test]
fn handrolled_overdeclared_streams_surfaces_mismatch() {
    // The truncated-capture shape: the header was stamped up-front for
    // 3 streams but only one `strl` LIST was physically written before
    // the file was cut off. The accessor surfaces the declared count,
    // the demuxer trusts the one stream it actually parsed, and the
    // cross-check reports the (declared, actual) divergence.
    let buf = build_avi(3, 1);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_declared_stream_count(), Some(3));
    assert_eq!(
        dmx.streams().len(),
        1,
        "the demuxer trusts the streams it physically parsed"
    );
    assert_eq!(
        dmx.declared_vs_actual_stream_count_mismatch(),
        Some((3, 1)),
        "over-declared dwStreams must surface as (declared, actual)"
    );
    // The raw declared count still round-trips through the metadata key.
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:streams" && v == "3"));
}

#[test]
fn handrolled_underdeclared_streams_surfaces_mismatch() {
    // The opposite skew: a hand-edited header declaring fewer streams
    // than are physically present. Still a mismatch; the demuxer trusts
    // the two streams it actually walked.
    let buf = build_avi(1, 2);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_declared_stream_count(), Some(1));
    assert_eq!(dmx.streams().len(), 2);
    assert_eq!(
        dmx.declared_vs_actual_stream_count_mismatch(),
        Some((1, 2)),
        "under-declared dwStreams must surface as (declared, actual)"
    );
}

// ---------------------------------------------------------------------------
// Independence from neighbouring AVIMAINHEADER DWORDs: offsets 16 / 20 /
// 24 / 32 / 36 each read back their own bytes.
// ---------------------------------------------------------------------------

#[test]
fn avih_streams_independent_of_neighbouring_fields() {
    let buf = build_avi(2, 2);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_total_frames(), Some(7)); // offset 16 (round-268)
    assert_eq!(dmx.initial_frames(), Some(2)); // offset 20 (round-157)
    assert_eq!(dmx.avih_declared_stream_count(), Some(2)); // offset 24 (round-292)
    assert_eq!(dmx.avih_movie_rect(), Some((64, 48))); // offset 32/36 (round-275)
}
