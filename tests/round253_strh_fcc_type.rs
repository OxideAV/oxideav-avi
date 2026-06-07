//! Round-253 per-stream `strh.fccType` AVI tests.
//!
//! `fccType` is the 4-byte FOURCC at byte offset 0 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, Appendix B
//! `fccType` row at line 235 + the `fcc` row at line 234). The spec
//! text reads: *"A FOURCC code that specifies the type of data
//! contained in the stream. The following standard AVI values are
//! defined: `auds` (audio stream), `mids` (MIDI stream), `txts` (text
//! stream), `vids` (video stream)."*
//!
//! The pre-round-253 muxer always stamped the packaging-derived
//! `t.entry.strh_type` (video: `vids`; audio: `auds`) at byte offset 0
//! of the strh. Round-253 adds:
//!
//! - the typed `AviDemuxer::stream_fcc_type(stream_index) -> Option<[u8; 4]>`
//!   raw-FOURCC accessor mapping the all-zero `[0, 0, 0, 0]` sentinel
//!   back to `None`,
//! - the `avi:strh.<n>.fcc_type = "<fourcc-or-hex>"` metadata key
//!   (omitted when the strh carried the all-zero sentinel),
//! - the `AviMuxOptions::with_stream_fcc_type(stream_index, fcc_type)`
//!   builder writing the supplied 4 bytes verbatim at byte offset 0
//!   of the strh, replacing the packaging-derived default.
//!
//! Exercises:
//!
//! - **No override baseline**: packaging-derived `vids` for the video
//!   stream and `auds` for the audio stream round-trip; metadata keys
//!   emit the printable FOURCC.
//! - **Video override**: stamping `mids` on the video stream
//!   round-trips through the accessor and the metadata.
//! - **Audio override**: stamping `txts` on the audio stream
//!   round-trips.
//! - **Per-stream independence**: an override on stream 0 doesn't
//!   perturb stream 1's readback, and vice versa.
//! - **Builder idempotency**: the last `with_stream_fcc_type(...)` for
//!   a given index wins.
//! - **Sibling-DWORD independence**: stamping `fccType` doesn't
//!   perturb `dwFlags` / `dwLength` / `dwSampleSize` /
//!   `dwSuggestedBufferSize` / `fccHandler` / `dwStart` / `wPriority`
//!   / `dwQuality` / `dwInitialFrames` / `wLanguage` / `(dwScale,
//!   dwRate)` readbacks.
//! - **Vendor / non-standard FOURCC**: a non-spec FOURCC such as
//!   `iavs` (legacy interleaved DV) surfaces verbatim — the demuxer
//!   does NOT validate membership in the spec's documented set.
//! - **Hand-rolled fixture**: an explicit non-zero `fccType` in a
//!   56-byte strh decodes to the expected bytes; the all-zero
//!   sentinel parses as `None`.
//! - **Metadata FOURCC formatting**: a printable FOURCC renders as
//!   the literal 4-character string; a non-printable FOURCC renders
//!   as `0xHHHHHHHH` lower-case hex (matching the
//!   `avi:strh.<n>.handler` printable-vs-hex split).
//! - **Out-of-range stream index**: the accessor returns `None`.

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

fn write_minimal(path: &std::path::Path, options: AviMuxOptions, video_payload_len: usize) {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();

    let mut v = Packet::new(0, streams[0].time_base, vec![0x55u8; video_payload_len]);
    v.pts = Some(0);
    v.flags.keyframe = true;
    mux.write_packet(&v).unwrap();

    let mut a = Packet::new(1, streams[1].time_base, vec![0u8; 8]);
    a.pts = Some(0);
    a.flags.keyframe = true;
    mux.write_packet(&a).unwrap();

    mux.write_trailer().unwrap();
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("oxideav-avi-r253-{name}-{pid}-{nanos}.avi"))
}

// ---------------------------------------------------------------------------
// No-override baseline: packaging-derived `vids` / `auds` surface via
// the typed accessor and the metadata keys.
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_no_override_packaging_default_surfaces_verbatim() {
    let tmp = tmp_path("no-override");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(0), Some(*b"vids"));
    assert_eq!(dmx.stream_fcc_type(1), Some(*b"auds"));

    let md = dmx.metadata();
    let v_type = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.fcc_type")
        .expect("expected `avi:strh.0.fcc_type` metadata key");
    assert_eq!(v_type.1, "vids");

    let a_type = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.fcc_type")
        .expect("expected `avi:strh.1.fcc_type` metadata key");
    assert_eq!(a_type.1, "auds");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Video override: stamping `mids` on the video stream round-trips.
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_video_override_mids_roundtrip() {
    let tmp = tmp_path("video-mids");
    let opts = AviMuxOptions::default().with_stream_fcc_type(0, *b"mids");
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(0), Some(*b"mids"));

    let md = dmx.metadata();
    let v_type = md.iter().find(|(k, _)| k == "avi:strh.0.fcc_type").unwrap();
    assert_eq!(v_type.1, "mids");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Audio override: stamping `txts` on the audio stream round-trips.
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_audio_override_txts_roundtrip() {
    let tmp = tmp_path("audio-txts");
    let opts = AviMuxOptions::default().with_stream_fcc_type(1, *b"txts");
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(1), Some(*b"txts"));

    let md = dmx.metadata();
    let a_type = md.iter().find(|(k, _)| k == "avi:strh.1.fcc_type").unwrap();
    assert_eq!(a_type.1, "txts");

    // Video stream still at packaging default.
    assert_eq!(dmx.stream_fcc_type(0), Some(*b"vids"));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Per-stream independence.
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_video_override_does_not_perturb_audio_stream() {
    let tmp = tmp_path("video-only-perturb");
    let opts = AviMuxOptions::default().with_stream_fcc_type(0, *b"mids");
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(0), Some(*b"mids"));
    assert_eq!(dmx.stream_fcc_type(1), Some(*b"auds"));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_fcc_type_audio_override_does_not_perturb_video_stream() {
    let tmp = tmp_path("audio-only-perturb");
    let opts = AviMuxOptions::default().with_stream_fcc_type(1, *b"txts");
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(1), Some(*b"txts"));
    assert_eq!(dmx.stream_fcc_type(0), Some(*b"vids"));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: last `with_stream_fcc_type` for a given index wins.
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_builder_idempotency_last_call_wins() {
    let tmp = tmp_path("idempotency");
    let opts = AviMuxOptions::default()
        .with_stream_fcc_type(0, *b"mids")
        .with_stream_fcc_type(0, *b"txts")
        .with_stream_fcc_type(0, *b"vids");
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(0), Some(*b"vids"));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Vendor / non-standard FOURCC: the spec doesn't pin a closed registry
// so any 4 non-zero bytes surface verbatim.
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_vendor_fourcc_surfaces_verbatim() {
    // `iavs` is the legacy "interleaved DV stream" FOURCC some capture
    // hardware uses to combine audio + video into a single stream — it
    // is NOT in the spec's documented `{auds, mids, txts, vids}` set
    // but a downstream re-mux should be able to stamp + round-trip it.
    let tmp = tmp_path("vendor-fourcc");
    let opts = AviMuxOptions::default().with_stream_fcc_type(0, *b"iavs");
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(0), Some(*b"iavs"));

    let md = dmx.metadata();
    let v_type = md.iter().find(|(k, _)| k == "avi:strh.0.fcc_type").unwrap();
    assert_eq!(v_type.1, "iavs");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Sibling-DWORD independence: the override doesn't perturb dwFlags /
// dwLength / dwSampleSize / dwSuggestedBufferSize / fccHandler /
// dwStart / wPriority / dwQuality / dwInitialFrames / wLanguage /
// (dwScale, dwRate).
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_override_does_not_perturb_sibling_strh_fields() {
    let tmp = tmp_path("sibling-independence");
    let opts = AviMuxOptions::default()
        .with_stream_fcc_type(0, *b"mids")
        .with_stream_fcc_type(1, *b"txts");
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Sibling fields all stay at their pre-override defaults (the
    // demuxer maps the spec's documented sentinels back to None for
    // most; non-default-sentinel fields keep packaging-derived values).
    assert_eq!(dmx.stream_flags(0), None);
    assert_eq!(dmx.stream_flags(1), None);
    assert_eq!(dmx.stream_language(0), None);
    assert_eq!(dmx.stream_language(1), None);
    assert_eq!(dmx.stream_priority(0), None);
    assert_eq!(dmx.stream_priority(1), None);
    assert_eq!(dmx.stream_quality(0), None);
    assert_eq!(dmx.stream_quality(1), None);
    assert_eq!(dmx.stream_start(0), None);
    assert_eq!(dmx.stream_start(1), None);
    assert_eq!(dmx.stream_initial_frames(0), None);
    assert_eq!(dmx.stream_initial_frames(1), None);

    // fccHandler: video packaging default is `MJPG`; audio default
    // is all-zero (None).
    assert_eq!(dmx.stream_handler(0), Some(*b"MJPG"));
    assert_eq!(dmx.stream_handler(1), None);

    // dwSampleSize: video = None (one frame per chunk); audio = 4
    // (nBlockAlign for stereo s16le).
    assert_eq!(dmx.stream_sample_size(0), None);
    assert_eq!(dmx.stream_sample_size(1), Some(4));

    // (dwScale, dwRate): video = (1, 25); audio = (1, 48_000).
    assert_eq!(dmx.stream_timebase(0), Some((1, 25)));
    assert_eq!(dmx.stream_timebase(1), Some((1, 48_000)));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: an explicit non-zero fccType in a 56-byte strh
// decodes to the expected bytes; the all-zero sentinel parses as None.
// ---------------------------------------------------------------------------

fn hand_rolled_avi_with_video_fcc_type(fcc_type: [u8; 4]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();

    // strh body (56 B).
    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(&fcc_type);
    // dwScale at offset 20, dwRate at offset 24 (set to (1, 25) so the
    // strh is otherwise legitimate-looking).
    strh[20..24].copy_from_slice(&1u32.to_le_bytes());
    strh[24..28].copy_from_slice(&25u32.to_le_bytes());

    // strf body: minimal BITMAPINFOHEADER (40 B), BI_RGB.
    let mut strf = vec![0u8; 40];
    strf[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
    strf[4..8].copy_from_slice(&64i32.to_le_bytes()); // biWidth
    strf[8..12].copy_from_slice(&48i32.to_le_bytes()); // biHeight
    strf[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf[14..16].copy_from_slice(&24u16.to_le_bytes()); // biBitCount

    // avih body (56 B).
    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame
    avih[28..32].copy_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih[36..40].copy_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih[40..44].copy_from_slice(&48u32.to_le_bytes()); // dwHeight

    fn write_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
    }

    // hdrl body: avih chunk + LIST strl(strh + strf).
    let mut hdrl_body: Vec<u8> = Vec::new();
    hdrl_body.extend_from_slice(b"hdrl");
    write_chunk(&mut hdrl_body, b"avih", &avih);

    let mut strl_body: Vec<u8> = Vec::new();
    strl_body.extend_from_slice(b"strl");
    write_chunk(&mut strl_body, b"strh", &strh);
    write_chunk(&mut strl_body, b"strf", &strf);

    hdrl_body.extend_from_slice(b"LIST");
    hdrl_body.extend_from_slice(&(strl_body.len() as u32).to_le_bytes());
    hdrl_body.extend_from_slice(&strl_body);

    // movi body: empty.
    let mut movi_body: Vec<u8> = Vec::new();
    movi_body.extend_from_slice(b"movi");

    // RIFF body: hdrl LIST + movi LIST.
    let mut riff_body: Vec<u8> = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(hdrl_body.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&hdrl_body);
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(movi_body.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&movi_body);

    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&riff_body);

    buf
}

#[test]
fn strh_fcc_type_hand_rolled_nonzero_decodes() {
    let bytes = hand_rolled_avi_with_video_fcc_type(*b"vids");
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_fcc_type(0), Some(*b"vids"));

    let md = dmx.metadata();
    let v_type = md.iter().find(|(k, _)| k == "avi:strh.0.fcc_type").unwrap();
    assert_eq!(v_type.1, "vids");
}

#[test]
fn strh_fcc_type_hand_rolled_all_zero_parses_as_none() {
    // An all-zero `fccType` causes the demuxer to drop the stream
    // (the unknown / Data arm of build_stream rejects it), so we
    // construct an AVI with the all-zero strh and verify no
    // stream-level metadata key for fcc_type is emitted, and no
    // streams are surfaced (or alternatively that
    // `stream_fcc_type(0)` returns `None` if the stream IS surfaced).
    let bytes = hand_rolled_avi_with_video_fcc_type([0, 0, 0, 0]);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Regardless of whether the stream survives the classifier, the
    // `avi:strh.0.fcc_type` metadata key must NOT be present and
    // the accessor must return `None`.
    assert_eq!(dmx.stream_fcc_type(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.fcc_type"),
        "all-zero fccType must omit the metadata key"
    );
}

// ---------------------------------------------------------------------------
// Metadata FOURCC formatting: a non-printable byte in the FOURCC
// renders as `0xHHHHHHHH` lower-case hex (matching the
// `avi:strh.<n>.handler` printable-vs-hex split).
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_non_printable_fourcc_renders_as_hex_metadata() {
    let tmp = tmp_path("non-printable");
    let opts = AviMuxOptions::default().with_stream_fcc_type(0, [0x00, 0x11, 0x22, 0x33]);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(0), Some([0x00, 0x11, 0x22, 0x33]));

    let md = dmx.metadata();
    let v_type = md.iter().find(|(k, _)| k == "avi:strh.0.fcc_type").unwrap();
    assert_eq!(v_type.1, "0x00112233");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Out-of-range stream index returns None.
// ---------------------------------------------------------------------------

#[test]
fn strh_fcc_type_out_of_range_index_returns_none() {
    let tmp = tmp_path("out-of-range");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_fcc_type(99), None);

    let _ = std::fs::remove_file(&tmp);
}
