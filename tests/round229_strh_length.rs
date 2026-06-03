//! Round-229 per-stream `strh.dwLength` AVI tests.
//!
//! `dwLength` is the 32-bit DWORD at byte offset 32 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, `dwLength` row
//! line 244): *"Length of this stream. The units are defined by the
//! dwRate and dwScale members of the stream's header."*
//!
//! The pre-round-229 muxer always patched the auto-derived per-stream
//! packet / sample count into the strh at the end of `write_trailer`
//! (video: `packet_count`, audio PCM / CBR: running `sample_count`
//! from the muxer's `size / sample_size` formula). Round-229 adds:
//!
//! - the typed `AviDemuxer::stream_length(stream_index) -> Option<u32>`
//!   accessor mapping the `0` "no length declared" value back to
//!   `None` so an unspecified length reads the same as an absent one,
//!   mirroring the round-222 `dwSampleSize` / round-217
//!   `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
//!   `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
//!   round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
//!   `rcFrame` / round-80 `strn` / round-107 `IDIT` "default ==
//!   absent" convention,
//! - the `avi:strh.<n>.length` metadata key (omitted for the `0`
//!   value),
//! - the `AviMuxOptions::with_stream_length(stream_index, n)` builder
//!   writing the supplied 32-bit value verbatim at byte offset 32 of
//!   the strh, replacing the auto-derived per-stream packet / sample
//!   count.
//!
//! Exercises:
//!
//! - **Auto-derived baseline**: no override ⇒ the muxer's
//!   packaging-derived `packet_count` / `sample_count` defaults reach
//!   the demuxer (video: 1 packet ⇒ `Some(1)`; audio: 8-byte PCM
//!   payload / nBlockAlign=4 ⇒ `Some(2)`).
//! - **Video override round-trip**: an explicit override on the video
//!   stream round-trips via the typed accessor and the metadata key.
//! - **Audio override round-trip**: an explicit override on the PCM
//!   audio stream replaces the auto-derived `sample_count`.
//! - **Builder idempotency**: the last `with_stream_length(...)` for
//!   a given index wins.
//! - **Explicit `0`**: stamps the de-facto "no length declared" value;
//!   the demuxer maps it to `None`, and the metadata key is omitted.
//! - **Boundary values**: `1`, `u32::MAX`, and a typical
//!   long-form-capture frame count (= 90 000 frames at 25 fps =
//!   60 minutes) round-trip exactly.
//! - **Independence across streams**: an override on stream 0 doesn't
//!   perturb stream 1's accessor, and vice versa.
//! - **Independence from sibling DWORDs**: stamping `dwLength`
//!   doesn't perturb `dwSampleSize` / `dwSuggestedBufferSize` /
//!   `fccHandler` / `dwStart` / `wPriority` / `dwQuality` /
//!   `dwInitialFrames` / `wLanguage` readbacks.
//! - **Hand-rolled fixture**: an explicit non-zero `dwLength` in a
//!   56-byte strh decodes to the expected raw u32; an all-zero
//!   `dwLength` parses as `None`.
//! - **Out-of-range stream index**: the typed accessor returns
//!   `None`.
//! - **`StreamInfo::duration` agreement**: the `Demuxer::streams`
//!   duration tracks the raw stamp for any in-range value.

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
// packet (which under nBlockAlign=4 PCM stereo s16le sums to a
// sample_count of 2).
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
    std::env::temp_dir().join(format!("oxideav-avi-r229-{name}-{pid}-{nanos}.avi"))
}

// ---------------------------------------------------------------------------
// Auto-derived baseline: no override ⇒ the muxer's packaging-derived
// per-stream `packet_count` / `sample_count` defaults reach the
// demuxer.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_no_override_auto_derived_default() {
    let tmp = tmp_path("no-override");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Video stream: one packet ⇒ packet_count = 1.
    assert_eq!(dmx.stream_length(0), Some(1));
    // Audio stream: 8 bytes / nBlockAlign=4 ⇒ sample_count = 2.
    assert_eq!(dmx.stream_length(1), Some(2));

    let md = dmx.metadata();
    let m0 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.length")
        .expect("auto-derived video length must surface as metadata");
    assert_eq!(m0.1, "1");
    let m1 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.length")
        .expect("auto-derived audio length must surface as metadata");
    assert_eq!(m1.1, "2");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Round-trip: a non-default override on the VIDEO stream survives mux
// → demux via the typed accessor and the metadata key.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_video_override_roundtrip_accessor_and_metadata() {
    let tmp = tmp_path("video-override");
    // Pretend this is a fixed-budget streamer that stamps a playlist
    // boundary count well past the actual single packet emitted.
    let opts = AviMuxOptions::default().with_stream_length(0, 1_500);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), Some(1_500));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.length")
        .expect("expected `avi:strh.0.length` metadata key for the override");
    assert_eq!(entry.1, "1500");

    // The audio stream is untouched: auto-derived sample_count still
    // surfaces.
    assert_eq!(dmx.stream_length(1), Some(2));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Round-trip: an override on the AUDIO stream replaces the
// auto-derived sample_count.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_audio_override_roundtrip_replaces_auto_derived_sample_count() {
    let tmp = tmp_path("audio-override");
    let opts = AviMuxOptions::default().with_stream_length(1, 192_000);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Audio override wins over the auto-derived sample_count of 2.
    assert_eq!(dmx.stream_length(1), Some(192_000));
    // Video untouched.
    assert_eq!(dmx.stream_length(0), Some(1));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: last `with_stream_length` for a given index
// wins.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_builder_idempotency_last_call_wins() {
    let tmp = tmp_path("idempotency");
    let opts = AviMuxOptions::default()
        .with_stream_length(0, 10)
        .with_stream_length(0, 100)
        .with_stream_length(0, 1_000);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), Some(1_000));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Explicit `0` override: stamps the de-facto "no length declared"
// value; demuxer maps it to None; metadata key omitted.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_explicit_zero_roundtrips_as_none_and_omits_metadata() {
    let tmp = tmp_path("explicit-zero");
    let opts = AviMuxOptions::default().with_stream_length(0, 0);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.length"),
        "explicit `0` override must omit the metadata key (default == absent)"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Boundary values on the video stream.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_boundary_one_roundtrips() {
    let tmp = tmp_path("boundary-one");
    let opts = AviMuxOptions::default().with_stream_length(0, 1);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), Some(1));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_length_boundary_u32_max_roundtrips() {
    let tmp = tmp_path("boundary-u32-max");
    let opts = AviMuxOptions::default().with_stream_length(0, u32::MAX);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), Some(u32::MAX));

    let md = dmx.metadata();
    let entry = md.iter().find(|(k, _)| k == "avi:strh.0.length").unwrap();
    assert_eq!(entry.1, u32::MAX.to_string());

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_length_typical_long_form_capture_count_roundtrips() {
    // 90 000 frames at 25 fps = 3600 seconds = 60 minutes.
    let tmp = tmp_path("long-form-90k");
    let opts = AviMuxOptions::default().with_stream_length(0, 90_000);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), Some(90_000));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence across streams.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_independence_across_streams() {
    let tmp = tmp_path("independence");
    // Override stream 0 only.
    let opts = AviMuxOptions::default().with_stream_length(0, 4_096);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), Some(4_096));
    // Audio stream still carries the auto-derived sample_count.
    assert_eq!(dmx.stream_length(1), Some(2));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from sibling DWORDs: stamping dwLength does not perturb
// the other strh DWORDs the demuxer surfaces.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_does_not_perturb_sibling_dwords() {
    let tmp = tmp_path("siblings");
    let opts = AviMuxOptions::default()
        // Stamp every sibling so we can confirm round-trip after also
        // adding dwLength.
        .with_stream_sample_size(0, 2_048)
        .with_stream_suggested_buffer_size(0, 65_536)
        .with_stream_handler(0, *b"YV12")
        .with_stream_start(0, 7)
        .with_stream_priority(0, 9)
        .with_stream_quality(0, 8_000)
        .with_stream_initial_frames(0, 11)
        .with_stream_language(0, 0x0409)
        // …and the new dwLength override.
        .with_stream_length(0, 314_159);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), Some(314_159));
    assert_eq!(dmx.stream_sample_size(0), Some(2_048));
    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(65_536));
    assert_eq!(dmx.stream_handler(0), Some(*b"YV12"));
    assert_eq!(dmx.stream_start(0), Some(7));
    assert_eq!(dmx.stream_priority(0), Some(9));
    assert_eq!(dmx.stream_quality(0), Some(8_000));
    assert_eq!(dmx.stream_initial_frames(0), Some(11));
    assert_eq!(dmx.stream_language(0), Some(0x0409));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// `StreamInfo::duration` agreement: the framework-level duration the
// demuxer derives from this same DWORD tracks the raw stamp.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_streaminfo_duration_agrees_with_raw_stamp() {
    let tmp = tmp_path("duration-agree");
    let opts = AviMuxOptions::default().with_stream_length(0, 9_999);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(0), Some(9_999));
    let streams = dmx.streams();
    let s0 = streams.iter().find(|s| s.index == 0).unwrap();
    // The framework `duration` is the same u32 widened to i64.
    assert_eq!(s0.duration, Some(9_999_i64));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: explicit non-zero / all-zero dwLength in a
// 56-byte strh decodes correctly.
// ---------------------------------------------------------------------------

fn hand_rolled_avi_with_video_length(length: u32) -> Vec<u8> {
    // Minimal one-video-stream AVI with all-zero strh fields except
    // fccType=`vids`, dwScale/dwRate (25 fps), and dwLength at offset
    // 32. A minimal BI_RGB strf and an empty movi follow.
    let mut buf: Vec<u8> = Vec::new();

    // strh body (56 B).
    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"vids");
    // dwScale at 20, dwRate at 24.
    strh[20..24].copy_from_slice(&1u32.to_le_bytes());
    strh[24..28].copy_from_slice(&25u32.to_le_bytes());
    // dwLength at 32.
    strh[32..36].copy_from_slice(&length.to_le_bytes());

    // strf body: minimal BITMAPINFOHEADER (40 B), BI_RGB (compression=0).
    let mut strf = vec![0u8; 40];
    strf[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
    strf[4..8].copy_from_slice(&64i32.to_le_bytes()); // biWidth
    strf[8..12].copy_from_slice(&48i32.to_le_bytes()); // biHeight
    strf[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf[14..16].copy_from_slice(&24u16.to_le_bytes()); // biBitCount
                                                        // biCompression at 16: BI_RGB = 0 (already zero).

    // avih body (56 B).
    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame
    avih[16..20].copy_from_slice(&0u32.to_le_bytes()); // dwFlags
    avih[20..24].copy_from_slice(&0u32.to_le_bytes()); // dwTotalFrames
    avih[24..28].copy_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih[28..32].copy_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih[36..40].copy_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih[40..44].copy_from_slice(&48u32.to_le_bytes()); // dwHeight

    // Helper: write a chunk with FourCC + size + body (pad if odd).
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

    // LIST strl body
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
fn strh_length_hand_rolled_nonzero_decodes() {
    let bytes = hand_rolled_avi_with_video_length(42_424);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_length(0), Some(42_424));
    let md = dmx.metadata();
    let entry = md.iter().find(|(k, _)| k == "avi:strh.0.length").unwrap();
    assert_eq!(entry.1, "42424");
}

#[test]
fn strh_length_hand_rolled_zero_decodes_as_none() {
    let bytes = hand_rolled_avi_with_video_length(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_length(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.length"),
        "zero dwLength must not surface a metadata key"
    );
}

// ---------------------------------------------------------------------------
// Out-of-range stream index returns None on the typed accessor.
// ---------------------------------------------------------------------------

#[test]
fn strh_length_out_of_range_index_returns_none() {
    let tmp = tmp_path("out-of-range");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_length(99), None);

    let _ = std::fs::remove_file(&tmp);
}
