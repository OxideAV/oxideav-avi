//! Round-234 OpenDML `LIST odml dmlh.dwTotalFrames` muxer-side override.
//!
//! Per OpenDML 2.0 §5.0 "Extended AVI Header (dmlh)" / "Total Frames"
//! (`docs/container/riff/opendml-avi-2.0.pdf` page 16):
//! *"The dwTotalFrames field indicates the real size of the AVI file.
//! Since the same field in the Main AVI Header 'avih' indicates the size
//! within the first RIFF 'AVI' chunk."* — i.e. the cross-segment frame
//! count, distinct from `avih.dwTotalFrames` (which counts the primary
//! RIFF only).
//!
//! Pre-round-234 the muxer always patched the auto-derived
//! `total_video_frames` (the primary video stream's cross-segment
//! `packet_count`) into the 4-byte `dmlh` body at `write_trailer` time.
//! Round-234 adds the `AviMuxOptions::with_dmlh_total_frames(n)` builder
//! that stamps `n` verbatim into the `ODMLExtendedAVIHeader.dwTotalFrames`
//! DWORD instead, replacing the auto-derived value.
//!
//! The override only changes the 4-byte stamp inside `LIST odml dmlh`;
//! it does NOT touch `avih.dwTotalFrames` (the primary-RIFF count stays
//! auto-derived), nor any downstream `idx1` / `ix##` / per-stream
//! `strh.dwLength` derivation — a caller that stamps a value
//! incompatible with the file's actual cross-segment chunk count is
//! creating an internally-inconsistent file on purpose (half-written
//! capture dumps, fixed-budget streamers that pre-declare a known frame
//! budget, fuzz / regression fixtures).
//!
//! Pairs with the round-101 demuxer accessor
//! [`AviDemuxer::dmlh_total_frames`] (and the
//! `avi:total_frames_all_segments` metadata key) for a builder→writer
//! →demuxer round-trip of any non-zero value.
//!
//! Exercises:
//!
//! - **Auto-derived baseline**: no override ⇒ the pre-round-234
//!   `total_video_frames` reaches the demuxer's
//!   `dmlh_total_frames()` accessor and the
//!   `avi:total_frames_all_segments` metadata key.
//! - **Override round-trip**: a non-default override survives mux →
//!   demux via both surfaces.
//! - **Builder idempotency**: the last `with_dmlh_total_frames(...)`
//!   call wins.
//! - **Override does not perturb `avih.dwTotalFrames`**: the
//!   primary-RIFF count keeps tracking the muxer's auto-derived
//!   `packet_count`.
//! - **Override does not perturb per-stream `strh.dwLength`**: the
//!   per-stream stamp keeps tracking the auto-derived packet / sample
//!   count.
//! - **Boundary values**: `1`, `u32::MAX`, a typical
//!   long-form-capture count, and the explicit `0` value all
//!   round-trip exactly via the typed accessor.
//! - **Avi10 mode ignores the override**: the muxer never emits a
//!   `LIST odml dmlh` for `AviKind::Avi10`, so the demuxer surfaces
//!   `None` regardless.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
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

/// Mux an OpenDML file with `n_frames` video packets plus one audio
/// packet per video frame, into a single-segment envelope (no rolling
/// `RIFF AVIX` continuations). Returns the path.
fn mux_opendml(name: &str, n_frames: u32, options: AviMuxOptions) -> std::path::PathBuf {
    let vid = video_stream(0);
    let aud = audio_stream(1);
    let streams = vec![vid.clone(), aud.clone()];
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r234-{name}-{pid}-{nanos}.avi"));
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    // 1 GiB segment limit: well above what any test writes, so we get
    // a single-RIFF OpenDML envelope (one `RIFF AVI ` with `LIST odml
    // dmlh`, no `RIFF AVIX` continuations). The override semantics are
    // identical whether or not we roll segments — the dmlh patch site
    // runs once at `write_trailer` time regardless.
    let mut mux = open_avi(
        ws,
        &streams,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(1 << 30)),
        options,
    )
    .unwrap();
    mux.write_header().unwrap();
    for i in 0..n_frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 64]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();

        let mut apkt = Packet::new(1, aud.time_base, vec![0u8; 8]);
        apkt.pts = Some(i as i64);
        apkt.flags.keyframe = true;
        mux.write_packet(&apkt).unwrap();
    }
    mux.write_trailer().unwrap();
    tmp
}

/// Mux a single-RIFF `AviKind::Avi10` file (no OpenDML envelope, so no
/// `LIST odml dmlh` is written).
fn mux_avi10(name: &str, n_frames: u32, options: AviMuxOptions) -> std::path::PathBuf {
    let vid = video_stream(0);
    let aud = audio_stream(1);
    let streams = vec![vid.clone(), aud.clone()];
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r234-{name}-{pid}-{nanos}.avi"));
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();
    for i in 0..n_frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 64]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();

        let mut apkt = Packet::new(1, aud.time_base, vec![0u8; 8]);
        apkt.pts = Some(i as i64);
        apkt.flags.keyframe = true;
        mux.write_packet(&apkt).unwrap();
    }
    mux.write_trailer().unwrap();
    tmp
}

// ---------------------------------------------------------------------------
// Auto-derived baseline: no override ⇒ the muxer's
// total_video_frames default reaches the demuxer through both the
// typed accessor and the `avi:total_frames_all_segments` metadata key.
// ---------------------------------------------------------------------------

#[test]
fn dmlh_total_frames_no_override_auto_derived_default() {
    let tmp = mux_opendml("no-override", 3, AviMuxOptions::new());
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // 3 video packets ⇒ auto-derived dmlh.dwTotalFrames = 3.
    assert_eq!(dmx.dmlh_total_frames(), Some(3));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:total_frames_all_segments")
        .expect("auto-derived dmlh must surface as metadata");
    assert_eq!(entry.1, "3");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Override round-trip: a non-default override survives mux → demux via
// both the typed accessor and the metadata key.
// ---------------------------------------------------------------------------

#[test]
fn dmlh_total_frames_override_roundtrip_accessor_and_metadata() {
    let tmp = mux_opendml(
        "override",
        3,
        AviMuxOptions::new().with_dmlh_total_frames(5_000),
    );
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.dmlh_total_frames(), Some(5_000));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:total_frames_all_segments")
        .expect("override dmlh must surface as metadata");
    assert_eq!(entry.1, "5000");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: the last `with_dmlh_total_frames(...)` call
// wins.
// ---------------------------------------------------------------------------

#[test]
fn dmlh_total_frames_builder_idempotency_last_call_wins() {
    let opts = AviMuxOptions::new()
        .with_dmlh_total_frames(10)
        .with_dmlh_total_frames(100)
        .with_dmlh_total_frames(1_000);
    let tmp = mux_opendml("idempotency", 3, opts);
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.dmlh_total_frames(), Some(1_000));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from `avih.dwTotalFrames`: the override only touches the
// `dmlh` stamp; the primary-RIFF avih count stays auto-derived.
// ---------------------------------------------------------------------------

#[test]
fn dmlh_total_frames_override_does_not_perturb_avih_total_frames() {
    let opts = AviMuxOptions::new().with_dmlh_total_frames(9_999);
    let tmp = mux_opendml("indep-avih", 4, opts);
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // dmlh carries the override.
    assert_eq!(dmx.dmlh_total_frames(), Some(9_999));

    // avih.dwTotalFrames keeps the auto-derived primary-RIFF count.
    // The demuxer feeds file-level duration from `avih.total_frames *
    // micro_sec_per_frame`: 4 packets at 25 fps ⇒ 4 × 40 000 µs =
    // 160 000 µs. If the override had also been stamped into avih
    // the file duration would balloon to 9 999 × 40 000 µs ≈ 399.96 s.
    let dur = oxideav_core::Demuxer::duration_micros(&dmx);
    assert_eq!(dur, Some(160_000));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from per-stream `strh.dwLength`: the dmlh override
// does not change the per-stream auto-derived packet / sample stamp.
// ---------------------------------------------------------------------------

#[test]
fn dmlh_total_frames_override_does_not_perturb_strh_length() {
    let opts = AviMuxOptions::new().with_dmlh_total_frames(7_000);
    let tmp = mux_opendml("indep-strh", 3, opts);
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // dmlh carries the override.
    assert_eq!(dmx.dmlh_total_frames(), Some(7_000));

    // Video strh.dwLength keeps the auto-derived per-stream packet
    // count (= 3).
    assert_eq!(dmx.stream_length(0), Some(3));
    // Audio strh.dwLength keeps the auto-derived sample_count: 3
    // audio packets × (8 B body / nBlockAlign=4) = 6.
    assert_eq!(dmx.stream_length(1), Some(6));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Boundary values via the typed accessor.
// ---------------------------------------------------------------------------

#[test]
fn dmlh_total_frames_boundary_one_roundtrips() {
    let tmp = mux_opendml(
        "boundary-one",
        2,
        AviMuxOptions::new().with_dmlh_total_frames(1),
    );
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.dmlh_total_frames(), Some(1));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn dmlh_total_frames_boundary_u32_max_roundtrips() {
    let tmp = mux_opendml(
        "boundary-u32-max",
        2,
        AviMuxOptions::new().with_dmlh_total_frames(u32::MAX),
    );
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    // u32::MAX widens to u64 on the accessor; both sides match.
    assert_eq!(dmx.dmlh_total_frames(), Some(u32::MAX as u64));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn dmlh_total_frames_boundary_long_form_capture_roundtrips() {
    // Typical 60-minute capture at 25 fps = 90 000 frames.
    let tmp = mux_opendml(
        "boundary-long",
        2,
        AviMuxOptions::new().with_dmlh_total_frames(90_000),
    );
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.dmlh_total_frames(), Some(90_000));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn dmlh_total_frames_explicit_zero_roundtrips_as_some_zero() {
    // OpenDML 2.0 §5.0 defines `dmlh.dwTotalFrames` as the cross-
    // segment frame count with no "no length declared" sentinel —
    // `0` literally means zero frames (i.e. the chunk reserves the
    // size but no frames have been declared as final). The demuxer
    // surfaces `Some(0)` for an explicitly-stamped `0` (distinct
    // from `None` which means no `LIST odml dmlh` was present).
    let tmp = mux_opendml(
        "explicit-zero",
        2,
        AviMuxOptions::new().with_dmlh_total_frames(0),
    );
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.dmlh_total_frames(), Some(0));
    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// AviKind::Avi10: the override is a no-op because no `LIST odml dmlh`
// chunk is written.
// ---------------------------------------------------------------------------

#[test]
fn dmlh_total_frames_override_ignored_in_avi10_mode() {
    let opts = AviMuxOptions::new().with_dmlh_total_frames(12_345);
    let tmp = mux_avi10("avi10-ignored", 3, opts);
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // No `LIST odml dmlh` chunk in `AviKind::Avi10` ⇒ accessor returns
    // `None` regardless of the option.
    assert_eq!(dmx.dmlh_total_frames(), None);

    // And the `avi:total_frames_all_segments` metadata key is omitted
    // entirely.
    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:total_frames_all_segments"),
        "Avi10 mode must not surface `avi:total_frames_all_segments`"
    );

    let _ = std::fs::remove_file(&tmp);
}
