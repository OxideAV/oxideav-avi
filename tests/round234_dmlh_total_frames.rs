//! Round-234 OpenDML `dmlh.dwTotalFrames` muxer-side override tests.
//!
//! `dmlh.dwTotalFrames` is the single DWORD at the start of the
//! `dmlh` chunk body inside `LIST odml` (a sibling of `avih` / `strl`
//! inside `LIST hdrl`) per OpenDML 2.0 §5.0 "Extended AVI Header"
//! (`docs/container/riff/opendml-avi-2.0.pdf`): the "real total frame
//! count across every `RIFF AVIX` segment". Pre-round-234 the muxer
//! always patched the auto-derived primary-video-stream
//! `packet_count` into this DWORD at the end of `write_trailer`
//! (`TrackState::packet_count` is not reset across segments, so the
//! count folds every AVIX continuation packet). Round-234 adds the
//! `AviMuxOptions::with_dmlh_total_frames(n)` builder writing the
//! supplied 32-bit value verbatim at that same patch site, replacing
//! the auto-derived default.
//!
//! Exercises:
//!
//! - **Auto-derived baseline**: no override ⇒ the demuxer reads back
//!   the primary video stream's packet count, agreeing with
//!   `super_index_duration_violations` (no entries).
//! - **Override round-trip**: an explicit override stamps the supplied
//!   value verbatim, the typed `dmlh_total_frames` accessor reads it
//!   back, and the `avi:total_frames_all_segments` metadata key
//!   carries the same string.
//! - **Builder idempotency**: the last `with_dmlh_total_frames(...)`
//!   wins.
//! - **Explicit `0`**: stamps a structurally-present `dmlh` chunk
//!   with a zero body; the typed accessor returns `Some(0)` and the
//!   metadata key surfaces as `"0"`.
//! - **Boundary values**: `1`, `u32::MAX`, and a typical long-form
//!   capture frame count (90 000 frames = 60 minutes @ 25 fps)
//!   round-trip exactly.
//! - **Mismatch surfaces via violations**: stamping a `dmlh` value
//!   that disagrees with the per-segment `dwDuration` sum trips the
//!   demuxer's `super_index_duration_violations` cross-check (the
//!   sum carries the actual per-segment frame totals; the stamped
//!   `dmlh` value is what the violation compares against).
//! - **`avih.dwTotalFrames` independence**: the override does not
//!   perturb `avih.dwTotalFrames` (still the primary-segment video
//!   packet count).
//! - **AVI 1.0 no-op**: the override is ignored in `AviKind::Avi10`
//!   mode (no `LIST odml dmlh` is emitted at all), so the typed
//!   accessor returns `None`.
//! - **Hand-rolled fixture**: an explicit non-zero `dmlh` DWORD in a
//!   minimal hand-built RIFF reads back via the typed accessor.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, MediaType, Muxer,
    Packet, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("mjpeg")).tag(CodecTag::fourcc(b"MJPG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    reg
}

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
    let mut params =
        CodecParameters::audio(CodecId::new("pcm_s16le")).with_tag(CodecTag::wave_format(0x0001));
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

/// Mux `frames` video-only packets into a single-segment OpenDML file
/// at `path` with the given options. Returns the actual frame count
/// written (== `frames`).
fn mux_video_opendml(path: &std::path::Path, frames: usize, options: AviMuxOptions) -> u32 {
    let vid = video_stream(0);
    let streams = vec![vid.clone()];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    // 64 KiB ceiling keeps everything in the primary segment for the
    // common-case tests; mismatch test below uses a smaller ceiling.
    let mut mux = open_avi(
        ws,
        &streams,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(65_536)),
        options,
    )
    .unwrap();
    mux.write_header().unwrap();
    for i in 0..frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 256]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();
    }
    mux.write_trailer().unwrap();
    frames as u32
}

/// Same as `mux_video_opendml` but in `AviKind::Avi10` mode (no
/// `LIST odml` is emitted at all).
fn mux_video_avi10(path: &std::path::Path, frames: usize, options: AviMuxOptions) -> u32 {
    let vid = video_stream(0);
    let streams = vec![vid.clone()];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();
    for i in 0..frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 256]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();
    }
    mux.write_trailer().unwrap();
    frames as u32
}

/// Multi-segment OpenDML driver matching the round-101 shape: a
/// 4 KiB segment ceiling plus bulky video packets rolls multiple
/// `RIFF AVIX` segments. Returns the file path and frame count.
fn mux_video_multi_segment(
    name: &str,
    frames: usize,
    options: AviMuxOptions,
) -> (std::path::PathBuf, u32) {
    let vid = video_stream(0);
    let aud = audio_stream(1);
    let streams = vec![vid.clone(), aud.clone()];
    let tmp = std::env::temp_dir().join(name);
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(
        ws,
        &streams,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(4096)),
        options,
    )
    .unwrap();
    mux.write_header().unwrap();
    for i in 0..frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 1500]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();
        let mut apkt = Packet::new(1, aud.time_base, vec![0u8; 64]);
        apkt.pts = Some(i as i64);
        apkt.flags.keyframe = true;
        mux.write_packet(&apkt).unwrap();
    }
    mux.write_trailer().unwrap();
    (tmp, frames as u32)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn auto_derived_baseline_round_trips() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-baseline.avi");
    let frames = mux_video_opendml(&tmp, 5, AviMuxOptions::default());

    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    assert_eq!(
        dem.dmlh_total_frames(),
        Some(frames as u64),
        "no override ⇒ auto-derived primary-video packet_count reaches the dmlh DWORD"
    );

    // Metadata key carries the same value.
    let md = dem.metadata();
    let total = md
        .iter()
        .find(|(k, _)| k == "avi:total_frames_all_segments")
        .map(|(_, v)| v.as_str());
    assert_eq!(total, Some("5"));

    // No violations: the auto-derived dmlh equals the per-segment
    // dwDuration sum trivially (single-segment file in this test).
    assert!(dem.super_index_duration_violations().is_empty());
}

#[test]
fn override_round_trips_via_accessor_and_metadata() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-override.avi");
    let _frames = mux_video_opendml(
        &tmp,
        5,
        AviMuxOptions::default().with_dmlh_total_frames(424242),
    );

    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    assert_eq!(
        dem.dmlh_total_frames(),
        Some(424242),
        "override stamps the supplied value verbatim into the dmlh DWORD"
    );

    let md = dem.metadata();
    let total = md
        .iter()
        .find(|(k, _)| k == "avi:total_frames_all_segments")
        .map(|(_, v)| v.as_str());
    assert_eq!(total, Some("424242"));
}

#[test]
fn builder_idempotency_last_call_wins() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-builder-idem.avi");
    let opts = AviMuxOptions::default()
        .with_dmlh_total_frames(1)
        .with_dmlh_total_frames(2)
        .with_dmlh_total_frames(99);
    let _frames = mux_video_opendml(&tmp, 3, opts);

    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    assert_eq!(dem.dmlh_total_frames(), Some(99));
}

#[test]
fn explicit_zero_round_trips_as_some_zero() {
    // Round-234: passing 0 stamps a structurally-present dmlh chunk
    // with a zero body. The typed accessor returns Some(0) (the
    // absence distinction is the chunk's presence, not its value)
    // and the metadata key carries "0".
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-zero.avi");
    let _frames = mux_video_opendml(&tmp, 4, AviMuxOptions::default().with_dmlh_total_frames(0));

    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    assert_eq!(
        dem.dmlh_total_frames(),
        Some(0),
        "explicit 0 override stamps a zero DWORD; the chunk is present so the accessor returns Some(0)"
    );

    let md = dem.metadata();
    let total = md
        .iter()
        .find(|(k, _)| k == "avi:total_frames_all_segments")
        .map(|(_, v)| v.as_str());
    assert_eq!(total, Some("0"));
}

#[test]
fn boundary_value_one_round_trips() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-one.avi");
    let _frames = mux_video_opendml(&tmp, 3, AviMuxOptions::default().with_dmlh_total_frames(1));
    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    assert_eq!(dem.dmlh_total_frames(), Some(1));
}

#[test]
fn boundary_value_u32_max_round_trips() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-umax.avi");
    let _frames = mux_video_opendml(
        &tmp,
        3,
        AviMuxOptions::default().with_dmlh_total_frames(u32::MAX),
    );
    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    assert_eq!(dem.dmlh_total_frames(), Some(u32::MAX as u64));
}

#[test]
fn boundary_value_long_form_capture_round_trips() {
    // 90 000 frames @ 25 fps = 3600 s = 60 minutes (a long-form
    // capture writer rounding its dmlh stamp to a known target before
    // packet emission). Verifies a typical-shape DWORD survives the
    // round-trip exactly.
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-longform.avi");
    let _frames = mux_video_opendml(
        &tmp,
        3,
        AviMuxOptions::default().with_dmlh_total_frames(90_000),
    );
    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    assert_eq!(dem.dmlh_total_frames(), Some(90_000));
}

#[test]
fn mismatch_with_per_segment_durations_surfaces_violation() {
    // Round-234: an override that disagrees with the actual per-segment
    // dwDuration sum trips the demuxer's super_index_duration_violations
    // cross-check. We force a multi-segment file (4 KiB ceiling) and
    // stamp a deliberately wrong dmlh value; the violation entry
    // carries the stamped dmlh_total_frames and the actual super-index
    // duration total derived from the per-segment frame counts.
    let frames = 6usize;
    let stamped = 999u32; // intentionally disagrees with the real frame count
    let (path, real_frames) = mux_video_multi_segment(
        "oxideav-avi-r234-mismatch.avi",
        frames,
        AviMuxOptions::default().with_dmlh_total_frames(stamped),
    );
    assert_eq!(real_frames as usize, frames);

    let f = std::fs::File::open(&path).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    // The typed accessor reads back the stamped value.
    assert_eq!(dem.dmlh_total_frames(), Some(stamped as u64));

    // The per-segment dwDuration values still sum to the real frame
    // count — the override only changed the dmlh DWORD, not any
    // super-index entry.
    let durations = dem.super_index_segment_durations(0);
    let sum: u64 = durations.iter().map(|&d| d as u64).sum();
    assert_eq!(
        sum, real_frames as u64,
        "per-segment dwDuration sum carries the actual frame count, not the stamped dmlh value"
    );

    // Violation fires for the video stream with the stamped dmlh.
    let violations = dem.super_index_duration_violations();
    let video = violations
        .iter()
        .find(|v| v.stream_index == 0)
        .expect("expected a violation entry for the video stream");
    assert_eq!(video.super_index_duration_total, real_frames as u64);
    assert_eq!(video.dmlh_total_frames, stamped as u64);
}

#[test]
fn override_does_not_perturb_avih_total_frames() {
    // The override only touches the dmlh DWORD; avih.dwTotalFrames is
    // still the primary-segment video packet count.
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-avih-indep.avi");
    let frames = 5;
    let _ = mux_video_opendml(
        &tmp,
        frames,
        AviMuxOptions::default().with_dmlh_total_frames(99_999),
    );
    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    // dmlh carries the stamped value.
    assert_eq!(dem.dmlh_total_frames(), Some(99_999));

    // avih.dwTotalFrames (== total_frames) still the primary-segment
    // video packet count: the demuxer derives `duration_micros` as
    // `avih.total_frames * avih.micro_sec_per_frame`. For 5 frames at
    // 25 fps (40_000 µs/frame) that's 200_000 µs — independent of the
    // 99_999 dmlh override.
    let dur = dem
        .duration_micros()
        .expect("duration_micros must be present");
    assert_eq!(
        dur, 200_000,
        "override is dmlh-only; avih.dwTotalFrames * dwMicroSecPerFrame stays at the primary-segment packet-count-derived duration"
    );
}

#[test]
fn override_is_noop_in_avi10_mode() {
    // AVI 1.0 mode doesn't emit `LIST odml` at all, so the override
    // has nothing to stamp. The typed accessor returns None.
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-avi10-noop.avi");
    let _frames = mux_video_avi10(&tmp, 4, AviMuxOptions::default().with_dmlh_total_frames(42));
    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    assert_eq!(
        dem.dmlh_total_frames(),
        None,
        "AviKind::Avi10 emits no LIST odml; the override is silently a no-op"
    );

    // The avi:total_frames_all_segments metadata key is omitted too.
    let md = dem.metadata();
    assert!(md.iter().all(|(k, _)| k != "avi:total_frames_all_segments"));
}

#[test]
fn override_does_not_perturb_idx1_entries() {
    // The override is dmlh-only; the idx1 entry count (== per-packet
    // entries) is unaffected. We use a small AVI 1.0 file too as a
    // pure regression check on the OpenDML-mode override.
    let tmp = std::env::temp_dir().join("oxideav-avi-r234-idx1-indep.avi");
    let frames = 4;
    let _ = mux_video_opendml(
        &tmp,
        frames,
        AviMuxOptions::default().with_dmlh_total_frames(12_345),
    );
    let f = std::fs::File::open(&tmp).unwrap();
    let mut dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    // Drain packets to confirm we get exactly `frames` of them — the
    // override didn't bend the actual stream count.
    let mut got = 0usize;
    loop {
        match dem.next_packet() {
            Ok(_pkt) => got += 1,
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e:?}"),
        }
    }
    assert_eq!(got, frames, "the override does not alter packet emission");
}

#[test]
fn hand_rolled_fixture_with_explicit_dmlh() {
    // Hand-build a minimal OpenDML RIFF whose `LIST odml dmlh` carries
    // an explicit non-zero DWORD. Verifies the demuxer reads the same
    // byte sequence the muxer emits.
    //
    // Layout:
    //   "RIFF"(4) + size(4) + "AVI "(4)
    //     "LIST"(4) + size(4) + "hdrl"(4)
    //       "avih"(4) + size(4=56) + 56 bytes of AVIMAINHEADER
    //       "LIST"(4) + size(4) + "strl"(4)
    //         "strh"(4) + size(4=56) + 56 bytes of AVISTREAMHEADER
    //         "strf"(4) + size(4=40) + 40 bytes of BITMAPINFOHEADER
    //       "LIST"(4) + size(4) + "odml"(4)
    //         "dmlh"(4) + size(4=4) + 4 bytes of dwTotalFrames (LE)
    //     "LIST"(4) + size(4) + "movi"(4)
    //       (no chunks — empty movi is legal for this parse path)

    use std::io::Cursor;

    let mut buf: Vec<u8> = Vec::new();

    // --- helpers ----------------------------------------------------------
    fn push_4cc(b: &mut Vec<u8>, cc: &[u8; 4]) {
        b.extend_from_slice(cc);
    }
    fn push_u32(b: &mut Vec<u8>, v: u32) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    fn push_u16(b: &mut Vec<u8>, v: u16) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    fn push_i32(b: &mut Vec<u8>, v: i32) {
        b.extend_from_slice(&v.to_le_bytes());
    }

    // RIFF preamble (size patched at end).
    push_4cc(&mut buf, b"RIFF");
    let riff_size_at = buf.len();
    push_u32(&mut buf, 0);
    push_4cc(&mut buf, b"AVI ");

    // LIST hdrl (size patched at end).
    push_4cc(&mut buf, b"LIST");
    let hdrl_size_at = buf.len();
    push_u32(&mut buf, 0);
    push_4cc(&mut buf, b"hdrl");

    // avih (56 bytes).
    push_4cc(&mut buf, b"avih");
    push_u32(&mut buf, 56);
    push_u32(&mut buf, 40_000); // dwMicroSecPerFrame (25 fps)
    push_u32(&mut buf, 0); // dwMaxBytesPerSec
    push_u32(&mut buf, 0); // dwPaddingGranularity
    push_u32(&mut buf, 0x10); // dwFlags = AVIF_HASINDEX
    push_u32(&mut buf, 0); // dwTotalFrames (primary segment count — not what we're testing)
    push_u32(&mut buf, 0); // dwInitialFrames
    push_u32(&mut buf, 1); // dwStreams
    push_u32(&mut buf, 0); // dwSuggestedBufferSize
    push_u32(&mut buf, 64); // dwWidth
    push_u32(&mut buf, 48); // dwHeight
    push_u32(&mut buf, 0); // dwReserved[0]
    push_u32(&mut buf, 0); // dwReserved[1]
    push_u32(&mut buf, 0); // dwReserved[2]
    push_u32(&mut buf, 0); // dwReserved[3]

    // LIST strl.
    push_4cc(&mut buf, b"LIST");
    let strl_size_at = buf.len();
    push_u32(&mut buf, 0);
    push_4cc(&mut buf, b"strl");

    // strh (56 bytes). Field order: fccType, fccHandler, dwFlags,
    // wPriority, wLanguage, dwInitialFrames, dwScale, dwRate, dwStart,
    // dwLength, dwSuggestedBufferSize, dwQuality, dwSampleSize, rcFrame.
    push_4cc(&mut buf, b"strh");
    push_u32(&mut buf, 56);
    push_4cc(&mut buf, b"vids");
    push_4cc(&mut buf, b"MJPG");
    push_u32(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u16(&mut buf, 0);
    push_u32(&mut buf, 0);
    push_u32(&mut buf, 1);
    push_u32(&mut buf, 25);
    push_u32(&mut buf, 0);
    push_u32(&mut buf, 0);
    push_u32(&mut buf, 0);
    push_u32(&mut buf, 0xFFFF_FFFF);
    push_u32(&mut buf, 0);
    let rc_frame: [i16; 4] = [0, 0, 64, 48];
    for v in rc_frame {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    // strf (40 bytes BITMAPINFOHEADER).
    push_4cc(&mut buf, b"strf");
    push_u32(&mut buf, 40);
    push_u32(&mut buf, 40); // biSize
    push_i32(&mut buf, 64); // biWidth
    push_i32(&mut buf, 48); // biHeight
    push_u16(&mut buf, 1); // biPlanes
    push_u16(&mut buf, 24); // biBitCount
    push_4cc(&mut buf, b"MJPG"); // biCompression
    push_u32(&mut buf, 64 * 48 * 3); // biSizeImage
    push_i32(&mut buf, 0); // biXPelsPerMeter
    push_i32(&mut buf, 0); // biYPelsPerMeter
    push_u32(&mut buf, 0); // biClrUsed
    push_u32(&mut buf, 0); // biClrImportant

    // strl LIST size: bytes since (strl_size_at + 4).
    let strl_body_len = (buf.len() - strl_size_at - 4) as u32;
    buf[strl_size_at..strl_size_at + 4].copy_from_slice(&strl_body_len.to_le_bytes());

    // LIST odml dmlh — the chunk under test.
    push_4cc(&mut buf, b"LIST");
    let odml_size_at = buf.len();
    push_u32(&mut buf, 0);
    push_4cc(&mut buf, b"odml");
    push_4cc(&mut buf, b"dmlh");
    push_u32(&mut buf, 4); // dmlh body length
    let dmlh_value: u32 = 7_777_777;
    push_u32(&mut buf, dmlh_value);

    let odml_body_len = (buf.len() - odml_size_at - 4) as u32;
    buf[odml_size_at..odml_size_at + 4].copy_from_slice(&odml_body_len.to_le_bytes());

    // hdrl LIST size: bytes since (hdrl_size_at + 4).
    let hdrl_body_len = (buf.len() - hdrl_size_at - 4) as u32;
    buf[hdrl_size_at..hdrl_size_at + 4].copy_from_slice(&hdrl_body_len.to_le_bytes());

    // Empty LIST movi.
    push_4cc(&mut buf, b"LIST");
    let movi_size_at = buf.len();
    push_u32(&mut buf, 0);
    push_4cc(&mut buf, b"movi");
    let movi_body_len = (buf.len() - movi_size_at - 4) as u32;
    buf[movi_size_at..movi_size_at + 4].copy_from_slice(&movi_body_len.to_le_bytes());

    // Patch outer RIFF size.
    let riff_body_len = (buf.len() - riff_size_at - 4) as u32;
    buf[riff_size_at..riff_size_at + 4].copy_from_slice(&riff_body_len.to_le_bytes());

    let cur: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let dem = demuxer_open_avi(cur, &registry()).unwrap();

    assert_eq!(
        dem.dmlh_total_frames(),
        Some(dmlh_value as u64),
        "hand-rolled dmlh DWORD decodes verbatim via the typed accessor"
    );

    let md = dem.metadata();
    let total = md
        .iter()
        .find(|(k, _)| k == "avi:total_frames_all_segments")
        .map(|(_, v)| v.as_str());
    assert_eq!(total, Some(dmlh_value.to_string().as_str()));
}
