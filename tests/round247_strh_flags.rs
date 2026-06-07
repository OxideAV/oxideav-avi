//! Round-247 per-stream `strh.dwFlags` AVI tests.
//!
//! `dwFlags` is the 32-bit DWORD at byte offset 8 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, `dwFlags` row at
//! line 237) + the *dwFlags values* table at lines 252–255 carrying
//! the two spec-documented bits:
//!
//! - `AVISF_DISABLED` (`0x0000_0001`): *"Indicates this stream should
//!   not be enabled by default."*
//! - `AVISF_VIDEO_PALCHANGES` (`0x0001_0000`): *"Indicates this video
//!   stream contains palette changes. This flag warns the playback
//!   software that it will need to animate the palette."*
//!
//! The pre-round-247 muxer always stamped `0` at strh body offset 8.
//! Round-247 adds:
//!
//! - the typed `AviDemuxer::stream_flags(stream_index) -> Option<u32>`
//!   raw accessor + the typed `stream_flags_typed -> Option<StrhFlags>`
//!   decoded accessor mapping the `0` "no flags set" default back to
//!   `None` so an unspecified flag DWORD reads the same as an absent
//!   one (mirroring the round-229 `dwLength` / round-222
//!   `dwSampleSize` / round-217 `dwSuggestedBufferSize` / round-210
//!   `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
//!   round-176 `dwQuality` / round-153 `dwInitialFrames` / round-119
//!   `wLanguage` / round-115 `rcFrame` "default == absent"
//!   convention),
//! - the `avi:strh.<n>.flags = "0xXXXXXXXX"` upper-case-hex metadata
//!   key (omitted for the `0` value),
//! - the `AviMuxOptions::with_stream_flags(stream_index, flags)`
//!   builder writing the supplied 32-bit value verbatim at byte
//!   offset 8 of the strh.
//!
//! Exercises:
//!
//! - **No override baseline**: legacy `0` writer default surfaces as
//!   `None` on both video and audio streams; metadata keys omitted.
//! - **`AVISF_DISABLED` round-trip**: stamping the documented
//!   "disabled by default" bit on the audio stream round-trips via
//!   the typed accessor, the typed-flags decode, and the metadata
//!   key.
//! - **`AVISF_VIDEO_PALCHANGES` round-trip**: stamping the documented
//!   palette-animation bit on the video stream round-trips and the
//!   typed decode exposes the `video_palchanges = true` field.
//! - **Combined bits**: both documented bits OR'd together
//!   round-trip exactly and the typed decode flags both `disabled`
//!   and `video_palchanges`.
//! - **Vendor / driver-private upper-half bits**: an undocumented bit
//!   in the upper half-DWORD round-trips verbatim via the raw
//!   accessor and is preserved in `StrhFlags::bits` even though
//!   neither documented field is set.
//! - **Builder idempotency**: the last `with_stream_flags(...)` for
//!   a given index wins.
//! - **Explicit `0`**: stamps the legacy "no flags set" value; the
//!   demuxer maps it back to `None`, and the metadata key is omitted.
//! - **`u32::MAX` boundary**: every bit set round-trips exactly.
//! - **Independence across streams**: an override on stream 0
//!   doesn't perturb stream 1's accessor, and vice versa.
//! - **Independence from sibling strh DWORDs**: stamping `dwFlags`
//!   doesn't perturb `dwLength` / `dwSampleSize` /
//!   `dwSuggestedBufferSize` / `fccHandler` / `dwStart` /
//!   `wPriority` / `dwQuality` / `dwInitialFrames` / `wLanguage`
//!   readbacks.
//! - **Independence from `avih.dwFlags`**: stamping a per-stream
//!   `strh.dwFlags` doesn't perturb the file-global `avih_flags()`
//!   typed-decode.
//! - **Hand-rolled fixture**: an explicit non-zero `dwFlags` in a
//!   56-byte strh decodes to the expected raw u32 + typed decode; an
//!   all-zero `dwFlags` parses as `None`.
//! - **Metadata hex formatting**: an `AVISF_VIDEO_PALCHANGES` stamp
//!   renders as `"0x00010000"` upper-case, matching the file-global
//!   `avi:flags` key formatting.
//! - **Out-of-range stream index**: the raw and typed accessors
//!   both return `None`.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer, Packet, Rational,
    ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{open_avi as demuxer_open_avi, AVISF_DISABLED, AVISF_VIDEO_PALCHANGES};
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

// One video packet (`video_payload_len` bytes) and one 8-byte audio
// packet so every test has a complete `movi` and `idx1` to walk.
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
    std::env::temp_dir().join(format!("oxideav-avi-r247-{name}-{pid}-{nanos}.avi"))
}

// ---------------------------------------------------------------------------
// No override baseline: legacy `0` "no flags set" default surfaces as
// `None` on both streams; metadata keys omitted.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_no_override_zero_default_surfaces_as_none() {
    let tmp = tmp_path("no-override");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(0), None);
    assert_eq!(dmx.stream_flags(1), None);
    assert!(dmx.stream_flags_typed(0).is_none());
    assert!(dmx.stream_flags_typed(1).is_none());

    let md = dmx.metadata();
    assert!(
        md.iter()
            .all(|(k, _)| !k.starts_with("avi:strh.") || !k.ends_with(".flags")),
        "the `0` default must omit `avi:strh.<n>.flags` metadata keys"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// AVISF_DISABLED round-trip on the audio stream.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_avisf_disabled_audio_roundtrip() {
    let tmp = tmp_path("disabled-audio");
    let opts = AviMuxOptions::default().with_stream_flags(1, AVISF_DISABLED);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(1), Some(AVISF_DISABLED));
    let typed = dmx.stream_flags_typed(1).expect("typed flags decode");
    assert!(typed.disabled);
    assert!(!typed.video_palchanges);
    assert_eq!(typed.bits, AVISF_DISABLED);

    // Video stream untouched.
    assert_eq!(dmx.stream_flags(0), None);

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.flags")
        .expect("expected `avi:strh.1.flags` metadata key");
    assert_eq!(entry.1, "0x00000001");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// AVISF_VIDEO_PALCHANGES round-trip on the video stream.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_avisf_video_palchanges_video_roundtrip() {
    let tmp = tmp_path("palchanges-video");
    let opts = AviMuxOptions::default().with_stream_flags(0, AVISF_VIDEO_PALCHANGES);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(0), Some(AVISF_VIDEO_PALCHANGES));
    let typed = dmx.stream_flags_typed(0).expect("typed flags decode");
    assert!(!typed.disabled);
    assert!(typed.video_palchanges);
    assert_eq!(typed.bits, AVISF_VIDEO_PALCHANGES);

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.flags")
        .expect("expected `avi:strh.0.flags` metadata key");
    assert_eq!(entry.1, "0x00010000");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Both documented bits OR'd together round-trip.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_combined_bits_roundtrip() {
    let tmp = tmp_path("combined");
    let combined = AVISF_DISABLED | AVISF_VIDEO_PALCHANGES;
    let opts = AviMuxOptions::default().with_stream_flags(0, combined);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(0), Some(combined));
    let typed = dmx.stream_flags_typed(0).expect("typed flags decode");
    assert!(typed.disabled);
    assert!(typed.video_palchanges);
    assert_eq!(typed.bits, combined);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Vendor / driver-private upper-half bits round-trip raw + via
// StrhFlags::bits while leaving the documented fields false.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_undocumented_upper_half_bits_roundtrip_verbatim() {
    let tmp = tmp_path("undocumented");
    // 0x4000_0000 sits in the upper half-DWORD outside both documented
    // AVISF_* constants. Some legacy capture filters use bits here to
    // tag driver-private state; the demuxer must round-trip them
    // verbatim and not mistakenly decode them as AVISF_DISABLED or
    // AVISF_VIDEO_PALCHANGES.
    let vendor_bit: u32 = 0x4000_0000;
    let opts = AviMuxOptions::default().with_stream_flags(0, vendor_bit);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(0), Some(vendor_bit));
    let typed = dmx.stream_flags_typed(0).expect("typed flags decode");
    assert!(!typed.disabled, "vendor bit must not flag AVISF_DISABLED");
    assert!(
        !typed.video_palchanges,
        "vendor bit must not flag AVISF_VIDEO_PALCHANGES"
    );
    assert_eq!(typed.bits, vendor_bit);

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.flags")
        .expect("expected `avi:strh.0.flags` metadata key");
    assert_eq!(entry.1, "0x40000000");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: last `with_stream_flags` for a given index wins.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_builder_idempotency_last_call_wins() {
    let tmp = tmp_path("idempotency");
    let opts = AviMuxOptions::default()
        .with_stream_flags(0, AVISF_DISABLED)
        .with_stream_flags(0, AVISF_VIDEO_PALCHANGES)
        .with_stream_flags(0, AVISF_DISABLED | AVISF_VIDEO_PALCHANGES);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_flags(0),
        Some(AVISF_DISABLED | AVISF_VIDEO_PALCHANGES)
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Explicit `0` override: stamps the legacy "no flags set" value;
// demuxer maps it back to None; metadata key omitted.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_explicit_zero_roundtrips_as_none_and_omits_metadata() {
    let tmp = tmp_path("explicit-zero");
    let opts = AviMuxOptions::default().with_stream_flags(0, 0);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(0), None);
    assert!(dmx.stream_flags_typed(0).is_none());

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.flags"),
        "explicit `0` override must omit the metadata key (default == absent)"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// u32::MAX boundary: every bit set round-trips exactly.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_u32_max_boundary_roundtrip() {
    let tmp = tmp_path("u32-max");
    let opts = AviMuxOptions::default().with_stream_flags(0, u32::MAX);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(0), Some(u32::MAX));
    let typed = dmx.stream_flags_typed(0).expect("typed flags decode");
    assert!(typed.disabled);
    assert!(typed.video_palchanges);
    assert_eq!(typed.bits, u32::MAX);

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.flags")
        .expect("expected `avi:strh.0.flags` metadata key");
    assert_eq!(entry.1, "0xFFFFFFFF");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence across streams.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_video_override_does_not_perturb_audio_stream() {
    let tmp = tmp_path("video-only");
    let opts = AviMuxOptions::default().with_stream_flags(0, AVISF_VIDEO_PALCHANGES);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(0), Some(AVISF_VIDEO_PALCHANGES));
    assert_eq!(dmx.stream_flags(1), None);

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_flags_audio_override_does_not_perturb_video_stream() {
    let tmp = tmp_path("audio-only");
    let opts = AviMuxOptions::default().with_stream_flags(1, AVISF_DISABLED);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(1), Some(AVISF_DISABLED));
    assert_eq!(dmx.stream_flags(0), None);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from sibling strh DWORDs.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_override_does_not_perturb_sibling_strh_dwords() {
    let tmp = tmp_path("sibling-independence");
    let opts = AviMuxOptions::default()
        .with_stream_flags(0, AVISF_VIDEO_PALCHANGES)
        .with_stream_flags(1, AVISF_DISABLED);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Sibling DWORDs all stay at their pre-override defaults (which the
    // demuxer maps to None for the spec's documented sentinels).
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

    // fccHandler: video stream's packaging default is `MJPG`; audio
    // packaging default is all-zero (None).
    assert_eq!(dmx.stream_handler(0), Some(*b"MJPG"));
    assert_eq!(dmx.stream_handler(1), None);

    // dwSampleSize: video = None (one frame per chunk), audio = 4
    // (nBlockAlign for stereo s16le).
    assert_eq!(dmx.stream_sample_size(0), None);
    assert_eq!(dmx.stream_sample_size(1), Some(4));

    // dwLength still auto-derived to the actual packet/sample counts.
    assert_eq!(dmx.stream_length(0), Some(1));
    assert_eq!(dmx.stream_length(1), Some(2));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from avih.dwFlags.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_override_does_not_perturb_avih_flags() {
    let tmp = tmp_path("avih-independence");
    let opts = AviMuxOptions::default().with_stream_flags(0, AVISF_VIDEO_PALCHANGES);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // The file-global avih.dwFlags should remain the muxer's default
    // (AVIF_HASINDEX | AVIF_TRUSTCKTYPE) regardless of any
    // strh-level flag override.
    let avih = dmx.avih_flags();
    assert!(
        avih.has_index,
        "AVIF_HASINDEX must remain set in the default avih.dwFlags"
    );
    assert!(
        avih.trust_ck_type,
        "AVIF_TRUSTCKTYPE must remain set in the default avih.dwFlags"
    );
    // The two strh-level bits live at different positions and must
    // not bleed into the file-global flags.
    assert!(
        !avih.is_interleaved,
        "AVIF_ISINTERLEAVED must not be inferred from per-stream dwFlags"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: an explicit non-zero dwFlags in a 56-byte strh
// decodes to the expected raw u32 + typed decode; an all-zero
// dwFlags parses as None.
// ---------------------------------------------------------------------------

fn hand_rolled_avi_with_video_flags(flags: u32) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();

    // strh body (56 B).
    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"vids");
    // dwFlags at offset 8.
    strh[8..12].copy_from_slice(&flags.to_le_bytes());
    // dwScale at 20, dwRate at 24.
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
fn strh_flags_hand_rolled_nonzero_decodes() {
    let bytes = hand_rolled_avi_with_video_flags(AVISF_VIDEO_PALCHANGES);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_flags(0), Some(AVISF_VIDEO_PALCHANGES));
    let typed = dmx.stream_flags_typed(0).expect("typed flags decode");
    assert!(typed.video_palchanges);
    assert!(!typed.disabled);

    let md = dmx.metadata();
    let entry = md.iter().find(|(k, _)| k == "avi:strh.0.flags").unwrap();
    assert_eq!(entry.1, "0x00010000");
}

#[test]
fn strh_flags_hand_rolled_zero_decodes_as_none() {
    let bytes = hand_rolled_avi_with_video_flags(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_flags(0), None);
    assert!(dmx.stream_flags_typed(0).is_none());

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.flags"),
        "zero dwFlags must not surface a metadata key"
    );
}

// ---------------------------------------------------------------------------
// Out-of-range stream index returns None on both accessors.
// ---------------------------------------------------------------------------

#[test]
fn strh_flags_out_of_range_index_returns_none() {
    let tmp = tmp_path("out-of-range");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_flags(99), None);
    assert!(dmx.stream_flags_typed(99).is_none());

    let _ = std::fs::remove_file(&tmp);
}
