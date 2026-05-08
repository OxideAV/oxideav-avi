//! Round-9 AVI feature tests.
//!
//! Covers:
//! - **C1** vprp per-field rect array round-trip — round-8 stopped at
//!   the 9 fixed DWORDs and discarded the trailing
//!   `VIDEO_FIELD_DESC[]` array. Round-9 reads + surfaces the
//!   per-field rects via `Demuxer::metadata()` and the typed
//!   `AviDemuxer::vprp_field_descs(stream)` accessor; the muxer is
//!   also fixed to emit one record per field instead of always
//!   writing a single record.
//! - **C3** typed `dmlh.dwTotalFrames` accessor — `AviDemuxer::dmlh_total_frames()`
//!   exposes the OpenDML 2.0 §5.0 cross-segment frame count without
//!   forcing callers to walk the metadata Vec.
//! - **C4** backward-walking strict keyframe seek —
//!   `seek_to_keyframe_strict` returns a [`KeyframeSeekResult`]
//!   exposing the gap between target_pts and the keyframe the
//!   demuxer landed on, so callers can plan a decode-and-discard
//!   loop or fail the seek if the GOP gap is too large.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, PixelFormat, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
// `Muxer` is referenced via its trait methods write_header / write_packet /
// write_trailer; rustc doesn't always pick that up as an explicit import
// until the call site forces resolution. Mark it used so `cargo clippy
// -D warnings` doesn't gate on `unused_imports`.
#[allow(dead_code)]
fn _muxer_trait_in_scope<M: Muxer>() {}

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_with_options, AviKind, AviMuxOptions, RiffSegmentLimit, VprpConfig};

// ---------------------------------------------------------------------------
// Test fixtures shared across round-9 cases.
// ---------------------------------------------------------------------------

fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg
}

fn registry_with_mjpeg() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("mjpeg")).tag(CodecTag::fourcc(b"MJPG")));
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

fn mjpeg_stream(width: u32, height: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn synth_payload(seed: u32, base_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(base_len);
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..base_len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// C1: vprp per-field rect array.
// ---------------------------------------------------------------------------

#[test]
fn vprp_pal_two_field_rects_round_trip_via_metadata() {
    // Round-9 C1: VprpConfig::pal() declares nbFieldPerFrame=2. The
    // muxer now emits one VIDEO_FIELD_DESC per field (compressed
    // half-height + alternating VideoYValidStartLine) instead of a
    // single full-frame record. The demuxer must surface the
    // per-field rects via `avi:vprp.<i>.field<j>.<key>` metadata.
    let stream = magicyuv_stream(720, 576);
    let payload = synth_payload(31, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r9-vprp-pal-2field.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::pal());
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    // nbFieldPerFrame = 2 from the PAL preset.
    assert_eq!(get("avi:vprp.0.nb_field_per_frame").as_deref(), Some("2"));
    // Field 0: half-height = 288, full width = 720, top-line start = 23.
    assert_eq!(
        get("avi:vprp.0.field0.compressed_bm_height").as_deref(),
        Some("288")
    );
    assert_eq!(
        get("avi:vprp.0.field0.compressed_bm_width").as_deref(),
        Some("720")
    );
    assert_eq!(
        get("avi:vprp.0.field0.video_y_valid_start_line").as_deref(),
        Some("23")
    );
    // Field 1: same dims, bottom field starts at half_height + 23 = 311.
    assert_eq!(
        get("avi:vprp.0.field1.compressed_bm_height").as_deref(),
        Some("288")
    );
    assert_eq!(
        get("avi:vprp.0.field1.video_y_valid_start_line").as_deref(),
        Some("311")
    );
}

#[test]
fn vprp_field_descs_typed_accessor_returns_two_records() {
    // Round-9 C1: typed accessor avoids re-parsing the metadata Vec.
    let stream = magicyuv_stream(720, 576);
    let payload = synth_payload(41, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r9-vprp-typed-accessor.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::pal());
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let descs = dmx.vprp_field_descs(0);
    assert_eq!(descs.len(), 2, "PAL nbFieldPerFrame=2 → two rect records");
    // Top field at line 23, bottom at line 311 (288 + 23).
    assert_eq!(descs[0].video_y_valid_start_line, 23);
    assert_eq!(descs[1].video_y_valid_start_line, 311);
    assert_eq!(descs[0].compressed_bm_height, 288);
    assert_eq!(descs[1].compressed_bm_height, 288);
    assert_eq!(descs[0].valid_bm_width, 720);
    // Out-of-range stream → empty slice.
    assert!(dmx.vprp_field_descs(99).is_empty());
}

#[test]
fn vprp_progressive_default_emits_single_field_rect() {
    // Round-9 C1: progressive (nbFieldPerFrame=1, the default) keeps
    // the single-record body the round-3 muxer always wrote.
    let stream = magicyuv_stream(640, 480);
    let payload = synth_payload(53, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r9-vprp-progressive.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let descs = dmx.vprp_field_descs(0);
    assert_eq!(
        descs.len(),
        1,
        "progressive default → single full-frame rect"
    );
    assert_eq!(descs[0].compressed_bm_height, 480);
    assert_eq!(descs[0].compressed_bm_width, 640);
    assert_eq!(descs[0].video_y_valid_start_line, 0);
}

// ---------------------------------------------------------------------------
// C3: typed dmlh.dwTotalFrames accessor.
// ---------------------------------------------------------------------------

#[test]
fn dmlh_total_frames_accessor_returns_some_for_opendml() {
    // Round-9 C3: a multi-segment OpenDML file forces the muxer to
    // emit `LIST odml dmlh` carrying the cross-segment total frame
    // count. The typed accessor returns Some(total) reflecting it.
    //
    // Use a tiny segment limit so >= 2 RIFFs are written.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..6).map(|i| synth_payload(i + 8000, 32_000)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r9-dmlh-total-frames.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            // Force at least 2 RIFF segments by capping at ~64 KiB.
            AviKind::OpenDml(RiffSegmentLimit::Bytes(64 * 1024)),
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
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let total = dmx.dmlh_total_frames();
    assert_eq!(
        total,
        Some(6u64),
        "dmlh.dwTotalFrames must equal the muxed frame count"
    );

    // Cross-check against the existing metadata key — the typed
    // accessor must not diverge from the string-based representation
    // historic callers depend on.
    let md = dmx.metadata();
    let md_total = md
        .iter()
        .find(|(k, _)| k == "avi:total_frames_all_segments")
        .map(|(_, v)| v.clone());
    assert_eq!(md_total.as_deref(), Some("6"));
}

#[test]
fn dmlh_total_frames_accessor_returns_none_for_avi_1_0() {
    // Round-9 C3: AVI 1.0 (no LIST odml dmlh) → None.
    let stream = mjpeg_stream(64, 64);
    let payload = synth_payload(67, 64);
    let reg = registry_with_mjpeg();

    let tmp = std::env::temp_dir().join("oxideav-avi-r9-dmlh-total-frames-none.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert!(
        dmx.dmlh_total_frames().is_none(),
        "AVI 1.0 has no dmlh chunk → accessor returns None"
    );
}

// ---------------------------------------------------------------------------
// C4: backward-walking strict keyframe seek.
// ---------------------------------------------------------------------------

#[test]
fn seek_to_keyframe_strict_reports_zero_gap_at_keyframe() {
    // Round-9 C4: every MJPEG frame the muxer wrote was flagged as a
    // keyframe, so a request at pts=5 lands exactly on pts=5 with
    // gop_distance=0.
    let stream = mjpeg_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..10u32)
        .map(|i| {
            let mut v = vec![0xFFu8, 0xD8];
            v.extend_from_slice(&i.to_le_bytes());
            v.extend_from_slice(&[0u8; 14]);
            v
        })
        .collect();
    let reg = registry_with_mjpeg();

    let tmp = std::env::temp_dir().join("oxideav-avi-r9-seek-strict-kf.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();

    let res = dmx
        .seek_to_keyframe_strict(0, 5)
        .expect("seek must succeed");
    assert_eq!(res.target_pts, 5);
    assert_eq!(res.landed_pts, 5);
    assert_eq!(
        res.gop_distance, 0,
        "every frame is a keyframe → gop gap is always 0"
    );
}

#[test]
fn seek_to_keyframe_strict_reports_non_zero_gap_inside_gop() {
    // Round-9 C4: simulate a sparse-keyframe stream by raw-writing
    // an idx1 where only a couple frames have AVIIF_KEYFRAME set.
    // Build a normal AVI then patch the idx1 entries' flag bytes so
    // only entries 0 and 5 are keyframes; ask for pts=8 → should
    // land on pts=5, gop_distance=3.
    let stream = mjpeg_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..10u32)
        .map(|i| {
            let mut v = vec![0xFFu8, 0xD8];
            v.extend_from_slice(&i.to_le_bytes());
            v.extend_from_slice(&[0u8; 14]);
            v
        })
        .collect();
    let reg = registry_with_mjpeg();

    let tmp = std::env::temp_dir().join("oxideav-avi-r9-seek-strict-gop.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            // Real GOP: keyframes at 0 and 5 only (other writes
            // pretend they're keyframes via flags.keyframe = true,
            // but we patch idx1 below so the demuxer sees them as
            // delta frames).
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    // Patch idx1 entries' flag bytes: clear AVIIF_KEYFRAME (0x10)
    // for everything except entries 0 and 5. Each idx1 entry is 16 B
    // laid out as ckid(4) | flags(4) | offset(4) | size(4); the
    // flags DWORD's low byte holds the AVIIF_KEYFRAME bit.
    let mut bytes = std::fs::read(&tmp).unwrap();
    let idx1_pos = {
        let mut found = None;
        for (i, w) in bytes.windows(4).enumerate() {
            if w == b"idx1" {
                found = Some(i);
                break;
            }
        }
        found.expect("idx1 must be present")
    };
    // Skip 4-byte FourCC + 4-byte size = 8 bytes to reach first entry.
    let entries_start = idx1_pos + 8;
    let n_entries = frames.len();
    for i in 0..n_entries {
        let off = entries_start + i * 16;
        if i == 0 || i == 5 {
            // Force AVIIF_KEYFRAME = 0x10 on (the muxer should
            // already have set it; this is a no-op safety belt).
            bytes[off + 4] |= 0x10;
        } else {
            // Clear AVIIF_KEYFRAME so the demuxer sees a delta frame.
            bytes[off + 4] &= !0x10;
        }
    }
    let tmp2 = std::env::temp_dir().join("oxideav-avi-r9-seek-strict-gop-patched.avi");
    std::fs::write(&tmp2, &bytes).unwrap();

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp2).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();

    // pts=8 lands inside the (5..10) GOP → strict result lands on 5.
    let res = dmx
        .seek_to_keyframe_strict(0, 8)
        .expect("seek must succeed");
    assert_eq!(res.target_pts, 8);
    assert_eq!(
        res.landed_pts, 5,
        "request for pts=8 must back off to keyframe at pts=5"
    );
    assert_eq!(
        res.gop_distance, 3,
        "decode-and-discard distance is target - landed = 3"
    );

    // Exact keyframe → zero gap.
    let res2 = dmx.seek_to_keyframe_strict(0, 5).unwrap();
    assert_eq!(res2.gop_distance, 0);

    // Mid-GOP between 0 and 5: pts=3 → lands at 0, gap=3.
    let res3 = dmx.seek_to_keyframe_strict(0, 3).unwrap();
    assert_eq!(res3.landed_pts, 0);
    assert_eq!(res3.gop_distance, 3);
}

#[test]
fn seek_to_keyframe_strict_negative_target_lands_on_first_keyframe() {
    // Round-9 C4: a request for a pts < 0 falls back to the first
    // keyframe in the file (gop_distance is clamped to 0 even though
    // arithmetically `target - landed < 0`).
    let stream = mjpeg_stream(64, 64);
    let payload = vec![0xFFu8, 0xD8, 1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let reg = registry_with_mjpeg();

    let tmp = std::env::temp_dir().join("oxideav-avi-r9-seek-strict-neg.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();

    let res = dmx.seek_to_keyframe_strict(0, -7).unwrap();
    assert_eq!(res.landed_pts, 0);
    assert_eq!(
        res.gop_distance, 0,
        "clamped at 0 even when target < landed"
    );
}
