//! Round-217 per-stream `strh.dwSuggestedBufferSize` AVI tests.
//!
//! `dwSuggestedBufferSize` is the 32-bit DWORD at byte offset 36 of the
//! 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, `dwSuggestedBufferSize`
//! row line 245): *"How large a buffer should be used to read this stream.
//! Typically, this contains a value corresponding to the largest chunk
//! present in the stream. Using the correct buffer size makes playback
//! more efficient. Use zero if you do not know the correct buffer size."*
//!
//! The field is the per-stream counterpart of the file-global
//! `avih.dwSuggestedBufferSize` already exposed by [`AviDemuxer::avih_suggested_buffer_size`]:
//! the avih flavour is meant to cover the largest chunk across every
//! stream, while the strh flavour is a per-stream upper bound (the spec
//! recommends keeping it equal to the largest chunk in that one stream).
//! The two are spec-independent and the demuxer surfaces each verbatim
//! with no validation against the actual largest chunk seen in `movi`.
//!
//! The pre-round-217 muxer always auto-derived this field from
//! `t.max_chunk_size` (the largest body observed on that stream during
//! `write_packet`) and patched it into the strh in `write_trailer`. The
//! round-217 `AviMuxOptions::with_stream_suggested_buffer_size` builder
//! lets a caller stamp a different hint — including the documented
//! `0` "do not know the correct buffer size" sentinel that the demuxer
//! maps back to `None`.
//!
//! `0` is the spec-documented "do not know" sentinel and maps to `None`
//! so an unspecified hint reads the same as an absent one, mirroring
//! the round-210 `fccHandler` / round-203 `dwStart` / round-182
//! `wPriority` / round-176 `dwQuality` / round-153 `dwInitialFrames`
//! / round-119 `wLanguage` / round-115 `rcFrame` / round-80 `strn` /
//! round-107 `IDIT` "default == absent" convention.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip** of a non-default per-stream
//!   `dwSuggestedBufferSize` via the typed accessor and the metadata
//!   key.
//! - **No-override baseline**: with no override, the muxer keeps its
//!   long-standing auto-derived default (`t.max_chunk_size`), which
//!   surfaces on a stream that wrote at least one non-empty packet.
//! - **Builder idempotency**: the last
//!   `with_stream_suggested_buffer_size(...)` wins per stream index.
//! - **Explicit `0` override**: stamps the spec-documented "do not
//!   know" sentinel, the demuxer maps it to `None`, and the metadata
//!   key is omitted.
//! - **Boundary values**: `1`, `u32::MAX`, and a typical 64 KiB
//!   read-ahead hint round-trip exactly.
//! - **Over-declaration**: a hint larger than the actual largest chunk
//!   in `movi` round-trips verbatim — the spec calls out the field as
//!   an *upper bound* and the demuxer does not validate.
//! - **Under-declaration**: a hint smaller than the actual largest
//!   chunk also round-trips verbatim — the spec does not pin a
//!   normative validation, and some legacy capture tools stamp a
//!   fixed value smaller than their occasional peak.
//! - **Independence across streams**: an override on stream 1 doesn't
//!   perturb stream 0's accessor, and vice versa.
//! - **Independence from sibling DWORDs**: stamping
//!   `dwSuggestedBufferSize` doesn't perturb `fccHandler` / `dwStart` /
//!   `wPriority` / `dwQuality` / `dwInitialFrames` / `wLanguage`
//!   readbacks.
//! - **Independence from the file-global `avih.dwSuggestedBufferSize`**:
//!   the per-stream strh value and the file-global avih value are
//!   spec-independent and round-trip without bleeding into each other.
//! - **Hand-rolled fixtures**: an explicit non-zero `dwSuggestedBufferSize`
//!   in a 56-byte strh decodes to the expected raw u32; an all-zero
//!   `dwSuggestedBufferSize` parses as `None`.

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

// One video packet of `video_payload_len` bytes and one 8-byte audio
// packet. The video payload size lets us assert the auto-derived
// default in the no-override baseline.
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
    std::env::temp_dir().join(format!("oxideav-avi-r217-{name}-{pid}-{nanos}.avi"))
}

// ---------------------------------------------------------------------------
// Round-trip: a non-default per-stream hint survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_override_roundtrip_accessor_and_metadata() {
    let tmp = tmp_path("override-roundtrip");
    // 64 KiB — a typical read-ahead hint a capture tool might stamp.
    let opts = AviMuxOptions::default().with_stream_suggested_buffer_size(0, 65_536);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(65_536));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.suggested_buffer_size")
        .expect("expected `avi:strh.0.suggested_buffer_size` metadata key for the override");
    assert_eq!(entry.1, "65536");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Baseline: no override ⇒ auto-derived `t.max_chunk_size` patched in
// `write_trailer`. With one 64-byte video packet, the strh sbs reads
// back as 64.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_no_override_auto_derived_from_max_chunk_size() {
    let tmp = tmp_path("no-override");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Video stream wrote one 64-byte packet → max_chunk_size = 64.
    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(64));
    // Audio stream wrote one 8-byte packet → max_chunk_size = 8.
    assert_eq!(dmx.stream_suggested_buffer_size(1), Some(8));

    let md = dmx.metadata();
    let m0 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.suggested_buffer_size")
        .expect("auto-derived video-stream hint must surface as metadata");
    assert_eq!(m0.1, "64");
    let m1 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.suggested_buffer_size")
        .expect("auto-derived audio-stream hint must surface as metadata");
    assert_eq!(m1.1, "8");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: last `with_stream_suggested_buffer_size` for a
// given index wins.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_builder_idempotency_last_call_wins() {
    let tmp = tmp_path("builder-idempotency");
    let opts = AviMuxOptions::default()
        .with_stream_suggested_buffer_size(0, 1024)
        .with_stream_suggested_buffer_size(0, 4096)
        .with_stream_suggested_buffer_size(0, 16_384);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(16_384));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Explicit `0` override stamps the spec-documented "do not know" sentinel;
// the demuxer maps it to `None` and the metadata key is omitted.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_explicit_zero_stamps_unknown_sentinel() {
    let tmp = tmp_path("explicit-zero");
    let opts = AviMuxOptions::default().with_stream_suggested_buffer_size(0, 0);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Override won: video stream's hint now 0 ⇒ None.
    assert_eq!(dmx.stream_suggested_buffer_size(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter()
            .all(|(k, _)| k != "avi:strh.0.suggested_buffer_size"),
        "explicit `0` override must omit the metadata key (default == absent)"
    );

    // Audio stream is untouched: auto-derived value still surfaces.
    assert_eq!(dmx.stream_suggested_buffer_size(1), Some(8));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Boundary values: `1` and `u32::MAX` round-trip exactly.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_boundary_one_roundtrips() {
    let tmp = tmp_path("boundary-one");
    let opts = AviMuxOptions::default().with_stream_suggested_buffer_size(0, 1);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(1));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_sbs_boundary_u32_max_roundtrips() {
    let tmp = tmp_path("boundary-u32-max");
    let opts = AviMuxOptions::default().with_stream_suggested_buffer_size(0, u32::MAX);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(u32::MAX));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.suggested_buffer_size")
        .unwrap();
    assert_eq!(entry.1, u32::MAX.to_string());

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Over-declaration: a hint larger than the actual largest chunk
// round-trips verbatim (spec calls the field an upper bound; the demuxer
// does not validate).
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_over_declaration_roundtrips_verbatim() {
    let tmp = tmp_path("over-declaration");
    // Actual largest chunk = 64; declared hint = 1 MiB.
    let opts = AviMuxOptions::default().with_stream_suggested_buffer_size(0, 1_048_576);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(1_048_576));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Under-declaration: a hint smaller than the actual largest chunk also
// round-trips verbatim (the demuxer doesn't second-guess the writer).
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_under_declaration_roundtrips_verbatim() {
    let tmp = tmp_path("under-declaration");
    // Actual largest chunk = 64; declared hint = 16.
    let opts = AviMuxOptions::default().with_stream_suggested_buffer_size(0, 16);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(16));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence across streams.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_per_stream_independence() {
    let tmp = tmp_path("per-stream-independence");
    let opts = AviMuxOptions::default()
        .with_stream_suggested_buffer_size(0, 32_768)
        .with_stream_suggested_buffer_size(1, 4_096);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(32_768));
    assert_eq!(dmx.stream_suggested_buffer_size(1), Some(4_096));

    let md = dmx.metadata();
    let m0 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.suggested_buffer_size")
        .unwrap();
    let m1 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.suggested_buffer_size")
        .unwrap();
    assert_eq!(m0.1, "32768");
    assert_eq!(m1.1, "4096");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Out-of-range stream index returns `None`.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_out_of_range_stream_index_is_none() {
    let tmp = tmp_path("out-of-range");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Only two streams exist (indices 0, 1).
    assert_eq!(dmx.stream_suggested_buffer_size(2), None);
    assert_eq!(dmx.stream_suggested_buffer_size(99), None);
    assert_eq!(dmx.stream_suggested_buffer_size(u32::MAX), None);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from sibling DWORDs.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_independent_of_sibling_dwords() {
    let tmp = tmp_path("sibling-independence");
    let opts = AviMuxOptions::default().with_stream_suggested_buffer_size(1, 8_192);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Hint got stamped.
    assert_eq!(dmx.stream_suggested_buffer_size(1), Some(8_192));
    // Every sibling per-stream DWORD must still read as its own default.
    assert_eq!(dmx.stream_start(1), None);
    assert_eq!(dmx.stream_priority(1), None);
    assert_eq!(dmx.stream_quality(1), None);
    assert_eq!(dmx.stream_initial_frames(1), None);
    assert_eq!(dmx.stream_language(1), None);
    // Audio stream's fccHandler default is all-zero ⇒ None.
    assert_eq!(dmx.stream_handler(1), None);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from the file-global `avih.dwSuggestedBufferSize`.
// ---------------------------------------------------------------------------

#[test]
fn strh_sbs_independent_of_avih_global_hint() {
    let tmp = tmp_path("vs-avih-global");
    // avih.dwSuggestedBufferSize = 999_999 (file-global override),
    // strh.dwSuggestedBufferSize for stream 0 = 12345 (per-stream).
    let opts = AviMuxOptions::default()
        .with_suggested_buffer_size(999_999)
        .with_stream_suggested_buffer_size(0, 12_345);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // File-global avih value reads back via `avih_suggested_buffer_size`.
    assert_eq!(dmx.avih_suggested_buffer_size(), 999_999);
    // Per-stream strh value reads back via `stream_suggested_buffer_size`.
    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(12_345));
    // Stream 1 keeps its auto-derived default (8 = max audio chunk size).
    assert_eq!(dmx.stream_suggested_buffer_size(1), Some(8));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: an AVI with a chosen `dwSuggestedBufferSize` at
// byte offset 36 of the strh decodes to the expected raw u32.
// ---------------------------------------------------------------------------

fn build_hand_rolled_avi(sbs_raw: u32) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();

    // strh body (56 B):
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids"); // fccType
    strh.extend_from_slice(&[0u8; 4]); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwLength
    strh.extend_from_slice(&sbs_raw.to_le_bytes()); // dwSuggestedBufferSize (offset 36)
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwQuality (-1)
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
                                                 // rcFrame (left, top, right, bottom)
    strh.extend_from_slice(&0i16.to_le_bytes());
    strh.extend_from_slice(&0i16.to_le_bytes());
    strh.extend_from_slice(&64i16.to_le_bytes());
    strh.extend_from_slice(&48i16.to_le_bytes());
    assert_eq!(strh.len(), 56);

    // strf body (40 B BITMAPINFOHEADER) for MJPG:
    let mut strf = Vec::with_capacity(40);
    strf.extend_from_slice(&40u32.to_le_bytes()); // biSize
    strf.extend_from_slice(&64i32.to_le_bytes()); // biWidth
    strf.extend_from_slice(&48i32.to_le_bytes()); // biHeight
    strf.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    strf.extend_from_slice(b"MJPG"); // biCompression
    strf.extend_from_slice(&(64u32 * 48 * 3).to_le_bytes()); // biSizeImage
    strf.extend_from_slice(&0i32.to_le_bytes());
    strf.extend_from_slice(&0i32.to_le_bytes());
    strf.extend_from_slice(&0u32.to_le_bytes());
    strf.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(strf.len(), 40);

    // strl LIST = "strl" + strh chunk + strf chunk.
    let mut strl: Vec<u8> = Vec::new();
    strl.extend_from_slice(b"strl");
    strl.extend_from_slice(b"strh");
    strl.extend_from_slice(&(strh.len() as u32).to_le_bytes());
    strl.extend_from_slice(&strh);
    strl.extend_from_slice(b"strf");
    strl.extend_from_slice(&(strf.len() as u32).to_le_bytes());
    strl.extend_from_slice(&strf);

    // avih body (56 B):
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&0x10u32.to_le_bytes()); // dwFlags (HASINDEX)
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwTotalFrames
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    avih.extend_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih.extend_from_slice(&48u32.to_le_bytes()); // dwHeight
    for _ in 0..4 {
        avih.extend_from_slice(&0u32.to_le_bytes()); // reserved
    }
    assert_eq!(avih.len(), 56);

    // hdrl LIST body.
    let mut hdrl: Vec<u8> = Vec::new();
    hdrl.extend_from_slice(b"hdrl");
    hdrl.extend_from_slice(b"avih");
    hdrl.extend_from_slice(&(avih.len() as u32).to_le_bytes());
    hdrl.extend_from_slice(&avih);
    hdrl.extend_from_slice(b"LIST");
    hdrl.extend_from_slice(&(strl.len() as u32).to_le_bytes());
    hdrl.extend_from_slice(&strl);

    // movi LIST body.
    let frame = vec![0xFFu8; 4];
    let mut movi: Vec<u8> = Vec::new();
    movi.extend_from_slice(b"movi");
    movi.extend_from_slice(b"00dc");
    movi.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    movi.extend_from_slice(&frame);

    // idx1.
    let mut idx1_body: Vec<u8> = Vec::new();
    idx1_body.extend_from_slice(b"00dc");
    idx1_body.extend_from_slice(&0x10u32.to_le_bytes()); // AVIIF_KEYFRAME
    idx1_body.extend_from_slice(&4u32.to_le_bytes()); // offset
    idx1_body.extend_from_slice(&(frame.len() as u32).to_le_bytes());

    let mut riff_body: Vec<u8> = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(hdrl.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&hdrl);
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(movi.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&movi);
    riff_body.extend_from_slice(b"idx1");
    riff_body.extend_from_slice(&(idx1_body.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&idx1_body);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);

    out
}

#[test]
fn hand_rolled_fixture_non_zero_sbs_decodes_verbatim() {
    let bytes = build_hand_rolled_avi(0xDEAD_BEEF);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(0xDEAD_BEEF));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.suggested_buffer_size")
        .unwrap();
    assert_eq!(entry.1, 0xDEAD_BEEFu32.to_string());
}

#[test]
fn hand_rolled_fixture_zero_sbs_parses_as_none() {
    let bytes = build_hand_rolled_avi(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_suggested_buffer_size(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter()
            .all(|(k, _)| k != "avi:strh.0.suggested_buffer_size"),
        "fixture's `0` dwSuggestedBufferSize must omit the metadata key (default == absent)"
    );
}
