//! Round-18 AVI feature tests.
//!
//! Covers:
//! - **C3** Strict cross-validator: `open_avi_strict` promotes the
//!   round-17 `avi:idx1.<n>.divergent_offsets` lenient metadata into
//!   a hard `Error::InvalidData` so callers wanting fail-fast on a
//!   stale `idx1` abort instead of inspecting metadata.
//! - **C4** `Idx1Flags`-aware first-video-keyframe-after seek that
//!   skips `AVIIF_NO_TIME`-tagged entries (palette / text / data
//!   side-band chunks the muxer flagged but that don't increment
//!   the per-stream presentation clock).
//! - **C1** Per-stream `dwMaxBytesPerSec` cap + strict-mode promotion.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{
    open_avi as demuxer_open_avi, open_avi_strict, AVIIF_KEYFRAME, AVIIF_NO_TIME,
};
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

// ---------------------------------------------------------------------------
// Test fixtures.
// ---------------------------------------------------------------------------

fn registry_with_video() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg
}

fn magicyuv_stream(width: u32, height: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn synth_payload(seed: u32, len: usize) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

/// Find every top-level chunk header `tag` in `bytes` and return
/// the byte offset of its 4-byte FourCC. Used to locate idx1 + ix##
/// chunk bodies for in-place mutation.
fn find_chunk_offsets(bytes: &[u8], tag: &[u8; 4]) -> Vec<usize> {
    let mut out = Vec::new();
    for i in 0..bytes.len().saturating_sub(4) {
        if &bytes[i..i + 4] == tag {
            out.push(i);
        }
    }
    out
}

/// Build a multi-segment OpenDML file the demuxer's `want_ix_scan`
/// will trigger for. Mirrors round-17's helper.
fn write_multi_segment_opendml_file(
    path: &std::path::Path,
    stream: &StreamInfo,
    frames: &[Vec<u8>],
) {
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let opts = AviMuxOptions::new().synthesise_idx1_from_ix(true);
    let mut mux = open_avi(
        ws,
        std::slice::from_ref(stream),
        AviKind::OpenDml(RiffSegmentLimit::Bytes(8 * 1024)),
        opts,
    )
    .unwrap();
    mux.write_header().unwrap();
    for (i, payload) in frames.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
}

// ---------------------------------------------------------------------------
// C3: Strict cross-validator.
// ---------------------------------------------------------------------------

#[test]
fn open_avi_strict_passes_when_idx1_and_ix_agree() {
    // Round-18 C3: a freshly muxed multi-segment OpenDML file whose
    // idx1 + ix## are byte-equal opens cleanly under strict mode.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..16).map(|i| synth_payload(i + 18100, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r18-strict-agree.avi");
    write_multi_segment_opendml_file(&tmp, &stream, &frames);

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = open_avi_strict(rs, &reg).expect("strict open should succeed on agreeing indexes");
    // Lenient view also surfaces no metadata key — sanity check.
    for (k, _) in dmx.metadata() {
        assert!(
            !k.contains("divergent_offsets"),
            "no divergent_offsets metadata expected when indexes agree (saw {k})"
        );
    }
}

#[test]
fn open_avi_strict_fails_on_idx1_offset_corruption() {
    // Round-18 C3: same byte-mutation as round-17 C4's
    // `cross_validator_surfaces_mismatch_when_idx1_offset_corrupted`,
    // but opened via `open_avi_strict` — must return
    // `Error::InvalidData` carrying the divergent seq + both
    // candidate offsets, NOT a metadata key on a successful open.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..16).map(|i| synth_payload(i + 18200, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r18-strict-mismatch.avi");
    write_multi_segment_opendml_file(&tmp, &stream, &frames);

    let mut bytes = std::fs::read(&tmp).unwrap();
    let idx1_offsets = find_chunk_offsets(&bytes, b"idx1");
    assert_eq!(idx1_offsets.len(), 1, "expected exactly one idx1");
    let body_start = idx1_offsets[0] + 8;
    // Mutate first idx1 entry's `dwOffset` DWORD: layout is
    // ckid(4) + flags(4) + offset(4) + size(4) = 16 B.
    let off_dword_pos = body_start + 8;
    bytes[off_dword_pos] = 0xEF;
    bytes[off_dword_pos + 1] = 0xBE;
    bytes[off_dword_pos + 2] = 0xAD;
    bytes[off_dword_pos + 3] = 0xDE;
    let mutated = std::env::temp_dir().join("oxideav-avi-r18-strict-mismatch-mut.avi");
    std::fs::write(&mutated, &bytes).unwrap();

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&mutated).unwrap());
    let err = match open_avi_strict(rs, &reg) {
        Ok(_) => panic!("strict open must fail on divergence"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("idx1") && msg.contains("ix##") && msg.contains("seq=0"),
        "error must mention idx1, ix##, and seq=0: {msg}"
    );
    assert!(
        matches!(err, Error::InvalidData(_)),
        "must be Error::InvalidData, got {err:?}"
    );

    // Lenient open on the SAME mutated file still works (round-17
    // C4 behaviour) — surfaces metadata instead of erroring.
    let rs2: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&mutated).unwrap());
    let dmx = demuxer_open_avi(rs2, &reg).expect("lenient open still succeeds on mutated idx1");
    let div_keys: Vec<_> = dmx
        .metadata()
        .iter()
        .filter(|(k, _)| k.contains("divergent_offsets"))
        .collect();
    assert_eq!(
        div_keys.len(),
        1,
        "lenient open still surfaces metadata on the same file"
    );
}

#[test]
fn open_avi_strict_passes_when_only_idx1_present() {
    // Round-18 C3: AVI 1.0 (idx1-only) files have no ix## to compare
    // against so the cross-validator has nothing to disagree on —
    // strict mode opens cleanly.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synth_payload(i + 18300, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r18-strict-avi10.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    open_avi_strict(rs, &reg).expect("strict open should pass on AVI 1.0 (no ix## to compare)");
}

// ---------------------------------------------------------------------------
// C4: Idx1Flags-aware seek-to-first-non-NO_TIME keyframe.
// ---------------------------------------------------------------------------

#[test]
fn seek_first_video_keyframe_after_skips_no_time_entries() {
    // Round-18 C4: build an idx1-bearing AVI 1.0 file with 8
    // keyframes, then in-place stamp `AVIIF_NO_TIME` onto entries
    // 1, 2 and 4 to simulate a stream whose idx1 interleaves
    // palette/text side-band chunks with real video keyframes.
    // `seek_to_first_video_keyframe_after(0, 0)` must skip the
    // NO_TIME entries and land on entry 0 (a non-NO_TIME keyframe);
    // `seek_to_first_video_keyframe_after(0, 1)` must skip 1+2 and
    // land on 3; `(0, 4)` must skip 4 and land on 5.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..8).map(|i| synth_payload(i + 18400, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r18-no-time-seek.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // In-place mutation: each idx1 entry is 16 B, body starts 8 B
    // after the `idx1` FourCC. Flags DWORD is at body_start + i*16
    // + 4. OR in AVIIF_NO_TIME (0x0100) on entries 1, 2, 4.
    let mut bytes = std::fs::read(&tmp).unwrap();
    let idx1_offsets = find_chunk_offsets(&bytes, b"idx1");
    assert_eq!(idx1_offsets.len(), 1);
    let body_start = idx1_offsets[0] + 8;
    for &entry_i in &[1usize, 2, 4] {
        let flags_pos = body_start + entry_i * 16 + 4;
        let mut f = u32::from_le_bytes([
            bytes[flags_pos],
            bytes[flags_pos + 1],
            bytes[flags_pos + 2],
            bytes[flags_pos + 3],
        ]);
        // Sanity check: the muxer set AVIIF_KEYFRAME on every
        // packet-flags-keyframe == true write_packet call.
        assert_eq!(
            f & AVIIF_KEYFRAME,
            AVIIF_KEYFRAME,
            "entry {entry_i} must already be a keyframe before mutation"
        );
        f |= AVIIF_NO_TIME;
        bytes[flags_pos..flags_pos + 4].copy_from_slice(&f.to_le_bytes());
    }
    let mutated = std::env::temp_dir().join("oxideav-avi-r18-no-time-seek-mut.avi");
    std::fs::write(&mutated, &bytes).unwrap();

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&mutated).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Verify the mutation took: entry 1 is now NO_TIME-flagged via
    // the typed accessor.
    let f1 = dmx.idx1_typed_flags_for_packet(0, 1).unwrap();
    assert!(f1.is_no_time && f1.is_keyframe);

    // Target = 0: first non-NO_TIME keyframe at-or-after pts 0 is
    // entry 0 itself (which we did NOT mutate).
    let r = dmx.seek_to_first_video_keyframe_after(0, 0).unwrap();
    assert_eq!(r.target_pts, 0);
    assert_eq!(r.landed_pts, 0);
    assert_eq!(r.gop_distance, 0);

    // Target = 1: entries 1+2 are NO_TIME; first non-NO_TIME
    // keyframe at-or-after pts 1 is entry 3.
    let r = dmx.seek_to_first_video_keyframe_after(0, 1).unwrap();
    assert_eq!(r.target_pts, 1);
    assert_eq!(r.landed_pts, 3);
    assert_eq!(r.gop_distance, 2);

    // Target = 4: entry 4 is NO_TIME; first non-NO_TIME keyframe
    // at-or-after pts 4 is entry 5.
    let r = dmx.seek_to_first_video_keyframe_after(0, 4).unwrap();
    assert_eq!(r.target_pts, 4);
    assert_eq!(r.landed_pts, 5);
    assert_eq!(r.gop_distance, 1);

    // Target = 7: entry 7 is non-NO_TIME, lands cleanly.
    let r = dmx.seek_to_first_video_keyframe_after(0, 7).unwrap();
    assert_eq!(r.landed_pts, 7);
    assert_eq!(r.gop_distance, 0);

    // Past EOF: fall back to last non-NO_TIME keyframe (entry 7).
    let r = dmx.seek_to_first_video_keyframe_after(0, 999).unwrap();
    assert_eq!(r.landed_pts, 7);
    assert_eq!(r.gop_distance, 0); // saturating_sub clamps to 0.

    // Out-of-range stream returns Err.
    assert!(dmx.seek_to_first_video_keyframe_after(99, 0).is_err());
}

#[test]
fn seek_first_video_keyframe_after_errors_when_no_idx1() {
    // Round-18 C4: helper requires idx1 (matches its docstring).
    // Build an idx1-bearing file but mangle the `idx1` FourCC so
    // the demuxer doesn't pick it up; then verify the helper returns
    // `Error::Unsupported`.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synth_payload(i + 18500, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r18-no-idx1.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let mut bytes = std::fs::read(&tmp).unwrap();
    let idx1_offsets = find_chunk_offsets(&bytes, b"idx1");
    assert_eq!(idx1_offsets.len(), 1);
    // Rename `idx1` -> `IDX1` so the demuxer's case-sensitive parser
    // skips it but the file still walks cleanly.
    let off = idx1_offsets[0];
    bytes[off..off + 4].copy_from_slice(b"IDX1");
    let mutated = std::env::temp_dir().join("oxideav-avi-r18-no-idx1-mut.avi");
    std::fs::write(&mutated, &bytes).unwrap();

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&mutated).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    let err = dmx
        .seek_to_first_video_keyframe_after(0, 0)
        .expect_err("must error when no idx1 present");
    assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// C1: Per-stream `dwMaxBytesPerSec` cap helper.
// ---------------------------------------------------------------------------

#[test]
fn per_stream_max_bytes_per_sec_clean_when_under_cap() {
    // Round-18 C1: a small video stream's observed bytes/sec stays
    // well under a generous 1 MB/s cap, so `over_budget_streams`
    // returns empty after `write_trailer`.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth_payload(i + 18600, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r18-budget-clean.avi");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let opts = AviMuxOptions::new().with_per_stream_max_bytes_per_sec(0, 1_000_000);
    let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
    mux.write_header().unwrap();
    for (i, payload) in frames.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    assert_eq!(
        mux.over_budget_streams(),
        &[][..],
        "no breaches expected when cap >> observed"
    );
}

#[test]
fn per_stream_max_bytes_per_sec_surfaces_breach() {
    // Round-18 C1: large frames + a tiny 100 B/s cap forces the
    // observed bytes/sec to exceed it; the breach lands in
    // `over_budget_streams` as `(stream_idx, observed_bps, cap_bps)`.
    let stream = magicyuv_stream(64, 64);
    // 4 frames of 4 KiB each at 25 fps = 4 KiB * 25 = ~100 KiB/s.
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth_payload(i + 18700, 4096)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r18-budget-breach.avi");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let opts = AviMuxOptions::new().with_per_stream_max_bytes_per_sec(0, 100);
    let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
    mux.write_header().unwrap();
    for (i, payload) in frames.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    let breaches = mux.over_budget_streams();
    assert_eq!(
        breaches.len(),
        1,
        "expected exactly one breach: {breaches:?}"
    );
    let (idx, observed, cap) = breaches[0];
    assert_eq!(idx, 0);
    assert_eq!(cap, 100);
    assert!(
        observed > 100,
        "observed must exceed cap; got observed={observed} cap={cap}"
    );
}

#[test]
fn per_stream_max_bytes_per_sec_strict_mode_errors_trailer() {
    // Round-18 C1: with `with_strict_per_stream_budget(true)` the
    // first breach makes `write_trailer` fail with
    // `Error::InvalidData` instead of populating
    // `over_budget_streams` silently.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth_payload(i + 18800, 4096)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r18-budget-strict.avi");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let opts = AviMuxOptions::new()
        .with_per_stream_max_bytes_per_sec(0, 100)
        .with_strict_per_stream_budget(true);
    let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
    mux.write_header().unwrap();
    for (i, payload) in frames.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    let err = mux
        .write_trailer()
        .expect_err("strict mode must fail trailer on breach");
    assert!(matches!(err, Error::InvalidData(_)), "got {err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("stream 0") && msg.contains("cap=100"),
        "error must name the offending stream + cap: {msg}"
    );
    // The breaches buffer is still populated for the caller to
    // inspect post-error.
    let breaches = mux.over_budget_streams();
    assert_eq!(breaches.len(), 1);
    assert_eq!(breaches[0].0, 0);
    assert_eq!(breaches[0].2, 100);
}

#[test]
fn per_stream_max_bytes_per_sec_replaces_prior_cap() {
    // Round-18 C1: builder semantics — a second
    // `with_per_stream_max_bytes_per_sec` call for the same stream
    // index replaces the prior cap.
    let opts = AviMuxOptions::new()
        .with_per_stream_max_bytes_per_sec(0, 100)
        .with_per_stream_max_bytes_per_sec(0, 200);
    assert_eq!(opts.per_stream_max_bytes_per_sec, vec![(0, 200)]);

    // bytes_per_sec == 0 removes the cap.
    let opts = opts.with_per_stream_max_bytes_per_sec(0, 0);
    assert!(opts.per_stream_max_bytes_per_sec.is_empty());
}
