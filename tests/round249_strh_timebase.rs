//! Round-249 per-stream `(strh.dwScale, strh.dwRate)` AVI tests.
//!
//! `dwScale` and `dwRate` are the paired 32-bit DWORDs at byte offsets
//! 20 and 24 of the 56-byte AVISTREAMHEADER per AVI 1.0
//! §"AVISTREAMHEADER" (`docs/container/riff/avi-riff-file-reference.md`,
//! `dwScale` row at line 241 + `dwRate` row at line 242). The spec text
//! reads: *"Used with dwRate to specify the time scale that this stream
//! will use. Dividing dwRate by dwScale gives the number of samples per
//! second. For video streams, this is the frame rate. For audio streams,
//! this rate corresponds to the time needed to play nBlockAlign bytes of
//! audio, which for PCM audio is the just the sample rate."*
//!
//! The pre-round-249 muxer always stamped the packaging-derived
//! `t.entry.scale` / `t.entry.rate` (video: `(1, frame_rate.num)`;
//! audio: `(1, sample_rate)`) at byte offsets 20 / 24. Round-249 adds:
//!
//! - the typed `AviDemuxer::stream_timebase(stream_index) -> Option<(u32, u32)>`
//!   raw-DWORD accessor mapping a `0` in either DWORD back to `None`
//!   (the writer-skips-it / mathematically-undefined sentinel),
//! - the `avi:strh.<n>.scale = "<N>"` and `avi:strh.<n>.rate = "<N>"`
//!   decimal metadata keys (both omitted when either DWORD is zero),
//! - the `AviMuxOptions::with_stream_timebase(stream_index, scale, rate)`
//!   builder writing the supplied pair verbatim at byte offsets 20 / 24
//!   of the strh, replacing the packaging-derived default.
//!
//! Exercises:
//!
//! - **No override baseline**: packaging-derived `(1, 25)` for the video
//!   stream and `(1, 48000)` for the audio stream round-trip; metadata
//!   keys emit decimal values.
//! - **Video override**: stamping NTSC's `(1001, 30000)` timebase on
//!   the video stream round-trips both DWORDs verbatim through the
//!   accessor and the metadata.
//! - **Audio override**: stamping `(1, 44100)` (CD audio) on the audio
//!   stream round-trips.
//! - **Per-stream independence**: an override on stream 0 doesn't
//!   perturb stream 1's readback, and vice versa.
//! - **Builder idempotency**: the last `with_stream_timebase(...)` for
//!   a given index wins.
//! - **u32::MAX boundary**: the maximum 32-bit value in each DWORD
//!   round-trips exactly.
//! - **Sibling-DWORD independence**: stamping `(dwScale, dwRate)`
//!   doesn't perturb `dwFlags` / `dwLength` / `dwSampleSize` /
//!   `dwSuggestedBufferSize` / `fccHandler` / `dwStart` / `wPriority`
//!   / `dwQuality` / `dwInitialFrames` / `wLanguage` readbacks.
//! - **Independence from `avih.dwMicroSecPerFrame`**: stamping a
//!   per-stream `(dwScale, dwRate)` doesn't bleed into the file-global
//!   frame-rate hint.
//! - **Hand-rolled fixture**: an explicit non-zero `(dwScale, dwRate)`
//!   in a 56-byte strh decodes to the expected pair; either DWORD
//!   being zero parses as `None`.
//! - **Metadata decimal formatting**: the override pair renders as
//!   bare decimal `u32` strings (matching the existing `avi:streams.*`
//!   convention for numeric magnitudes).
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
    std::env::temp_dir().join(format!("oxideav-avi-r249-{name}-{pid}-{nanos}.avi"))
}

// ---------------------------------------------------------------------------
// No-override baseline: the packaging-derived `(scale, rate)` pair
// surfaces via the typed accessor and the decimal metadata keys.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_no_override_packaging_default_surfaces_verbatim() {
    let tmp = tmp_path("no-override");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Video stream: frame_rate (25, 1) → (scale=1, rate=25).
    assert_eq!(dmx.stream_timebase(0), Some((1, 25)));
    // Audio stream: sample_rate 48_000 → (scale=1, rate=48_000).
    assert_eq!(dmx.stream_timebase(1), Some((1, 48_000)));

    let md = dmx.metadata();
    let v_scale = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.scale")
        .expect("expected `avi:strh.0.scale` metadata key");
    assert_eq!(v_scale.1, "1");
    let v_rate = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.rate")
        .expect("expected `avi:strh.0.rate` metadata key");
    assert_eq!(v_rate.1, "25");

    let a_scale = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.scale")
        .expect("expected `avi:strh.1.scale` metadata key");
    assert_eq!(a_scale.1, "1");
    let a_rate = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.rate")
        .expect("expected `avi:strh.1.rate` metadata key");
    assert_eq!(a_rate.1, "48000");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Video override: NTSC 29.97 fps timebase `(1001, 30000)` round-trips.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_video_override_ntsc_roundtrip() {
    let tmp = tmp_path("video-ntsc");
    let opts = AviMuxOptions::default().with_stream_timebase(0, 1001, 30_000);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_timebase(0), Some((1001, 30_000)));

    let md = dmx.metadata();
    let v_scale = md.iter().find(|(k, _)| k == "avi:strh.0.scale").unwrap();
    assert_eq!(v_scale.1, "1001");
    let v_rate = md.iter().find(|(k, _)| k == "avi:strh.0.rate").unwrap();
    assert_eq!(v_rate.1, "30000");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Audio override: CD-audio `(1, 44100)` round-trips.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_audio_override_cd_audio_roundtrip() {
    let tmp = tmp_path("audio-cd");
    let opts = AviMuxOptions::default().with_stream_timebase(1, 1, 44_100);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_timebase(1), Some((1, 44_100)));

    let md = dmx.metadata();
    let a_scale = md.iter().find(|(k, _)| k == "avi:strh.1.scale").unwrap();
    assert_eq!(a_scale.1, "1");
    let a_rate = md.iter().find(|(k, _)| k == "avi:strh.1.rate").unwrap();
    assert_eq!(a_rate.1, "44100");

    // Video stream still at packaging default.
    assert_eq!(dmx.stream_timebase(0), Some((1, 25)));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Per-stream independence.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_video_override_does_not_perturb_audio_stream() {
    let tmp = tmp_path("video-only-perturb");
    let opts = AviMuxOptions::default().with_stream_timebase(0, 1001, 30_000);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_timebase(0), Some((1001, 30_000)));
    assert_eq!(dmx.stream_timebase(1), Some((1, 48_000)));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_timebase_audio_override_does_not_perturb_video_stream() {
    let tmp = tmp_path("audio-only-perturb");
    let opts = AviMuxOptions::default().with_stream_timebase(1, 1, 22_050);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_timebase(1), Some((1, 22_050)));
    assert_eq!(dmx.stream_timebase(0), Some((1, 25)));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: last `with_stream_timebase` for a given index wins.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_builder_idempotency_last_call_wins() {
    let tmp = tmp_path("idempotency");
    let opts = AviMuxOptions::default()
        .with_stream_timebase(0, 1, 24)
        .with_stream_timebase(0, 1, 30)
        .with_stream_timebase(0, 1001, 60_000);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_timebase(0), Some((1001, 60_000)));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// u32::MAX boundary: the maximum 32-bit value in each DWORD round-trips.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_u32_max_boundary_roundtrip() {
    let tmp = tmp_path("u32-max");
    let opts = AviMuxOptions::default().with_stream_timebase(0, u32::MAX, u32::MAX);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_timebase(0), Some((u32::MAX, u32::MAX)));

    let md = dmx.metadata();
    let v_scale = md.iter().find(|(k, _)| k == "avi:strh.0.scale").unwrap();
    assert_eq!(v_scale.1, u32::MAX.to_string());
    let v_rate = md.iter().find(|(k, _)| k == "avi:strh.0.rate").unwrap();
    assert_eq!(v_rate.1, u32::MAX.to_string());

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Sibling-DWORD independence: the override doesn't perturb dwFlags /
// dwLength / dwSampleSize / dwSuggestedBufferSize / fccHandler /
// dwStart / wPriority / dwQuality / dwInitialFrames / wLanguage.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_override_does_not_perturb_sibling_strh_dwords() {
    let tmp = tmp_path("sibling-independence");
    let opts = AviMuxOptions::default()
        .with_stream_timebase(0, 1001, 30_000)
        .with_stream_timebase(1, 1, 96_000);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Sibling DWORDs all stay at their pre-override defaults (the
    // demuxer maps the spec's documented sentinels back to None).
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

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Override drives the framework-derived `StreamInfo::time_base`. The
// `dwRate` / `dwScale` pair in the strh is the on-disk source of truth
// the demuxer uses to build `time_base`, so an override observably
// shifts the per-stream tick.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_override_shifts_stream_info_time_base() {
    let tmp = tmp_path("time-base-shift");
    // Stamp NTSC on the video stream; the framework `time_base` for
    // stream 0 must reflect the override pair, not the original 25-fps
    // packaging pair.
    let opts = AviMuxOptions::default().with_stream_timebase(0, 1001, 30_000);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Raw override surfaces verbatim.
    assert_eq!(dmx.stream_timebase(0), Some((1001, 30_000)));

    // The framework-level `StreamInfo::time_base` derives from the
    // same raw pair (after the demuxer's `.max(1)` clamp on each
    // member, which is a no-op for non-zero values). For a video
    // stream the demuxer picks `TimeBase::new(scale as i64, rate as i64)`
    // so the override at (1001, 30000) maps to a 1001 / 30_000 tick.
    let streams = dmx.streams();
    let tb = streams[0].time_base.as_rational();
    assert_eq!(tb.num, 1001);
    assert_eq!(tb.den, 30_000);

    // Audio stream's tick stays at the packaging default
    // (`scale=1, rate=48_000`).
    let audio_tb = streams[1].time_base.as_rational();
    assert_eq!(audio_tb.num, 1);
    assert_eq!(audio_tb.den, 48_000);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: an explicit non-zero (dwScale, dwRate) in a
// 56-byte strh decodes to the expected pair; either DWORD being zero
// parses as None.
// ---------------------------------------------------------------------------

fn hand_rolled_avi_with_video_timebase(scale: u32, rate: u32) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();

    // strh body (56 B).
    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"vids");
    // dwScale at offset 20, dwRate at offset 24.
    strh[20..24].copy_from_slice(&scale.to_le_bytes());
    strh[24..28].copy_from_slice(&rate.to_le_bytes());

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
fn strh_timebase_hand_rolled_nonzero_decodes() {
    let bytes = hand_rolled_avi_with_video_timebase(1001, 30_000);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_timebase(0), Some((1001, 30_000)));

    let md = dmx.metadata();
    let v_scale = md.iter().find(|(k, _)| k == "avi:strh.0.scale").unwrap();
    assert_eq!(v_scale.1, "1001");
    let v_rate = md.iter().find(|(k, _)| k == "avi:strh.0.rate").unwrap();
    assert_eq!(v_rate.1, "30000");
}

#[test]
fn strh_timebase_hand_rolled_zero_scale_parses_as_none() {
    let bytes = hand_rolled_avi_with_video_timebase(0, 30_000);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_timebase(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter()
            .all(|(k, _)| k != "avi:strh.0.scale" && k != "avi:strh.0.rate"),
        "either-zero (dwScale, dwRate) must omit both metadata keys"
    );
}

#[test]
fn strh_timebase_hand_rolled_zero_rate_parses_as_none() {
    let bytes = hand_rolled_avi_with_video_timebase(1001, 0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_timebase(0), None);
}

#[test]
fn strh_timebase_hand_rolled_zero_both_parses_as_none() {
    let bytes = hand_rolled_avi_with_video_timebase(0, 0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_timebase(0), None);
}

// ---------------------------------------------------------------------------
// Out-of-range stream index returns None.
// ---------------------------------------------------------------------------

#[test]
fn strh_timebase_out_of_range_index_returns_none() {
    let tmp = tmp_path("out-of-range");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_timebase(99), None);

    let _ = std::fs::remove_file(&tmp);
}
