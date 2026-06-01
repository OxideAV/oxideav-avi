//! Round-203 per-stream `strh.dwStart` AVI tests.
//!
//! `dwStart` is the 32-bit DWORD at byte offset 28 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, `dwStart` row,
//! line 243): *"Starting time for this stream. The units are defined
//! by the dwRate and dwScale members in the main file header. Usually,
//! this is zero, but it can specify a delay time for a stream that
//! does not start concurrently with the file."*
//!
//! The spec phrases the field as a stream-local delay relative to the
//! file's logical start; the unit is the stream's own
//! `(dwRate / dwScale)` tick (frames for video, samples-or-blocks for
//! audio). The demuxer surfaces the raw 32-bit DWORD verbatim and the
//! muxer writes whatever 32-bit value the caller supplies — applications
//! that use the field for delayed-stream tagging round-trip exactly.
//!
//! `0` is the spec-documented "starts concurrently with the file"
//! default (also the muxer's own default since round-3) and maps to
//! `None` so an unspecified start reads the same as an absent one,
//! mirroring the round-182 `wPriority` / round-176 `dwQuality` /
//! round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
//! `rcFrame` / round-80 `strn` / round-107 `IDIT` "default == absent"
//! convention.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip** of a non-default per-stream start via
//!   the typed accessor and the metadata key.
//! - **No-override baseline**: with no override, the muxer keeps the
//!   `dwStart = 0` default which the demuxer maps to `None` and the
//!   metadata-key loop omits.
//! - **Builder idempotency**: the last `with_stream_start(...)` wins
//!   per stream index.
//! - **Explicit `0` override** reads back as `None` (default == absent).
//! - **Boundary values** (`1`, `u32::MAX`) round-trip verbatim — the
//!   spec does not pin a range so neither extreme is special-cased.
//! - **Independence across streams**: a start offset on stream 1
//!   doesn't perturb stream 0's `None`, and vice versa.
//! - **Independence from sibling DWORDs**: stamping `dwStart` doesn't
//!   perturb `wPriority` / `dwQuality` / `dwInitialFrames` /
//!   `wLanguage` readbacks.
//! - **Hand-rolled fixtures**: an explicit non-zero `dwStart` in a
//!   56-byte strh decodes to the expected raw u32; an all-zeros
//!   `dwStart` parses as `None`.

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

fn tmp_path(name: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("oxideav-avi-r203-{name}-{pid}-{nanos}.avi"))
}

// ---------------------------------------------------------------------------
// Round-trip: a non-default per-stream start survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn strh_start_override_roundtrip_accessor_and_metadata() {
    // Audio is delayed 19 ticks (in its own dwScale=1/dwRate=48000
    // sample units, so ~0.4 ms of pre-roll silence). A small non-zero
    // pick the spec's "specify a delay time for a stream that does not
    // start concurrently with the file" illustrates.
    let tmp = tmp_path("override-roundtrip");
    let opts = AviMuxOptions::default().with_stream_start(1, 19);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_start(1), Some(19));
    // Stream 0 keeps the legacy zero ⇒ reads as None.
    assert_eq!(dmx.stream_start(0), None);

    let md = dmx.metadata();
    let key = "avi:strh.1.start";
    let entry = md.iter().find(|(k, _)| k == key);
    assert!(
        entry.is_some(),
        "expected `{key}` metadata key for the non-default override"
    );
    assert_eq!(entry.unwrap().1, "19");
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.start"),
        "stream 0's `0` default must NOT surface as a metadata key (default == absent)"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Baseline: no override ⇒ legacy `0` ⇒ `None` accessor + omitted key.
// ---------------------------------------------------------------------------

#[test]
fn strh_start_no_override_reads_as_none_and_metadata_omits_key() {
    let tmp = tmp_path("no-override");
    write_minimal(&tmp, AviMuxOptions::default());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_start(0), None);
    assert_eq!(dmx.stream_start(1), None);

    let md = dmx.metadata();
    assert!(
        md.iter()
            .all(|(k, _)| !k.starts_with("avi:strh.") || !k.ends_with(".start")),
        "no `avi:strh.<n>.start` key may surface when every stream carries the `0` default"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: last `with_stream_start` for a given index wins.
// ---------------------------------------------------------------------------

#[test]
fn strh_start_builder_idempotency_last_call_wins() {
    let tmp = tmp_path("builder-idempotency");
    let opts = AviMuxOptions::default()
        .with_stream_start(1, 100)
        .with_stream_start(1, 200)
        .with_stream_start(1, 7);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_start(1), Some(7));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Explicit `0` override reads back as `None` (default == absent).
// ---------------------------------------------------------------------------

#[test]
fn strh_start_explicit_zero_maps_back_to_none() {
    let tmp = tmp_path("explicit-zero");
    let opts = AviMuxOptions::default().with_stream_start(1, 0);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_start(1), None);

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.1.start"),
        "explicit `0` override must omit the metadata key just like no-override"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Boundary values: `1` and `u32::MAX` round-trip verbatim (no range pin).
// ---------------------------------------------------------------------------

#[test]
fn strh_start_boundary_value_one() {
    let tmp = tmp_path("boundary-one");
    let opts = AviMuxOptions::default().with_stream_start(0, 1);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_start(0), Some(1));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_start_boundary_value_u32_max() {
    let tmp = tmp_path("boundary-u32max");
    let opts = AviMuxOptions::default().with_stream_start(0, u32::MAX);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // The spec does not pin a range; u32::MAX (= 0xFFFF_FFFF) is the
    // documented "use default" sentinel for `dwQuality` but is NOT
    // special-cased for `dwStart` — the spec text gives no sentinel for
    // dwStart so the value round-trips verbatim.
    assert_eq!(dmx.stream_start(0), Some(u32::MAX));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence across streams: start on stream 1 doesn't perturb stream 0,
// and vice versa.
// ---------------------------------------------------------------------------

#[test]
fn strh_start_per_stream_independence() {
    let tmp = tmp_path("per-stream-independence");
    let opts = AviMuxOptions::default()
        .with_stream_start(0, 3)
        .with_stream_start(1, 42);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_start(0), Some(3));
    assert_eq!(dmx.stream_start(1), Some(42));

    let md = dmx.metadata();
    let m0 = md.iter().find(|(k, _)| k == "avi:strh.0.start").unwrap();
    let m1 = md.iter().find(|(k, _)| k == "avi:strh.1.start").unwrap();
    assert_eq!(m0.1, "3");
    assert_eq!(m1.1, "42");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Out-of-range stream-index accessor returns `None`.
// ---------------------------------------------------------------------------

#[test]
fn strh_start_out_of_range_stream_index_is_none() {
    let tmp = tmp_path("out-of-range");
    write_minimal(&tmp, AviMuxOptions::default());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Only two streams exist (indices 0, 1).
    assert_eq!(dmx.stream_start(2), None);
    assert_eq!(dmx.stream_start(99), None);
    assert_eq!(dmx.stream_start(u32::MAX), None);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from sibling DWORDs: stamping dwStart leaves wPriority,
// dwQuality, dwInitialFrames, wLanguage at their own defaults.
// ---------------------------------------------------------------------------

#[test]
fn strh_start_independent_of_sibling_dwords() {
    let tmp = tmp_path("sibling-independence");
    let opts = AviMuxOptions::default().with_stream_start(1, 1234);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // dwStart got stamped.
    assert_eq!(dmx.stream_start(1), Some(1234));
    // Every sibling per-stream DWORD must still read as its own default.
    assert_eq!(dmx.stream_priority(1), None);
    assert_eq!(dmx.stream_quality(1), None);
    assert_eq!(dmx.stream_initial_frames(1), None);
    assert_eq!(dmx.stream_language(1), None);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: an AVI with a non-zero `dwStart` at byte offset 28
// of the strh decodes to the expected raw u32.
// ---------------------------------------------------------------------------

fn build_hand_rolled_avi(start_raw: u32) -> Vec<u8> {
    // Builds the minimal AVI envelope:
    //   RIFF 'AVI '
    //     LIST 'hdrl'
    //       'avih' (56 B)
    //       LIST 'strl'
    //         'strh' (56 B) — vids
    //         'strf' (40 B) — BITMAPINFOHEADER, MJPG
    //     LIST 'movi' (1 frame)
    //     'idx1'
    let mut out: Vec<u8> = Vec::new();

    // strh body (56 B):
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids");
    strh.extend_from_slice(b"MJPG"); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    strh.extend_from_slice(&start_raw.to_le_bytes()); // dwStart (offset 28)
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwLength = 1 frame
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwQuality (-1)
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
                                                 // rcFrame (left, top, right, bottom) = (0,0,64,48)
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
    strf.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    strf.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    assert_eq!(strf.len(), 40);

    // strl LIST = "strl" + strh chunk + strf chunk.
    let mut strl: Vec<u8> = Vec::new();
    strl.extend_from_slice(b"strl");
    // strh chunk
    strl.extend_from_slice(b"strh");
    strl.extend_from_slice(&(strh.len() as u32).to_le_bytes());
    strl.extend_from_slice(&strh);
    // strf chunk
    strl.extend_from_slice(b"strf");
    strl.extend_from_slice(&(strf.len() as u32).to_le_bytes());
    strl.extend_from_slice(&strf);

    // avih body (56 B):
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame (25fps)
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

    // hdrl LIST body = "hdrl" + avih chunk + strl LIST.
    let mut hdrl: Vec<u8> = Vec::new();
    hdrl.extend_from_slice(b"hdrl");
    hdrl.extend_from_slice(b"avih");
    hdrl.extend_from_slice(&(avih.len() as u32).to_le_bytes());
    hdrl.extend_from_slice(&avih);
    hdrl.extend_from_slice(b"LIST");
    hdrl.extend_from_slice(&(strl.len() as u32).to_le_bytes());
    hdrl.extend_from_slice(&strl);

    // movi LIST body = "movi" + one tiny '00dc' frame.
    let frame = vec![0xFFu8; 4];
    let mut movi: Vec<u8> = Vec::new();
    movi.extend_from_slice(b"movi");
    movi.extend_from_slice(b"00dc");
    movi.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    movi.extend_from_slice(&frame);

    // idx1 chunk: one entry pointing at the frame.
    let mut idx1_body: Vec<u8> = Vec::new();
    idx1_body.extend_from_slice(b"00dc");
    idx1_body.extend_from_slice(&0x10u32.to_le_bytes()); // AVIIF_KEYFRAME
    idx1_body.extend_from_slice(&4u32.to_le_bytes()); // movi-relative offset to '00dc'
    idx1_body.extend_from_slice(&(frame.len() as u32).to_le_bytes());

    // riff body = "AVI " + LIST hdrl + LIST movi + idx1 chunk
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

    // RIFF envelope.
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);

    out
}

#[test]
fn hand_rolled_fixture_non_zero_start_decodes_verbatim() {
    let bytes = build_hand_rolled_avi(0xDEAD_BEEFu32);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_start(0), Some(0xDEAD_BEEFu32));

    let md = dmx.metadata();
    let entry = md.iter().find(|(k, _)| k == "avi:strh.0.start").unwrap();
    assert_eq!(entry.1, format!("{}", 0xDEAD_BEEFu32));
}

#[test]
fn hand_rolled_fixture_zero_start_parses_as_none() {
    let bytes = build_hand_rolled_avi(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_start(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.start"),
        "fixture's `0` dwStart must omit the metadata key (default == absent)"
    );
}
