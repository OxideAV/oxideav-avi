//! Round-3 OpenDML 2.0 feature-completeness tests.
//!
//! Covers:
//! - `LIST odml dmlh` cross-segment total-frame count (P1)
//! - `vprp` Video Properties Header emission + parse (P3)
//! - `LIST rec ` cluster grouping in `movi` (P4)
//! - 2-field interlaced `ix##` parse (P2 — decoder side; encoder still
//!   emits progressive)

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, MediaType, Packet, Rational,
    ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{
    open_with_kind, open_with_options, AviKind, AviMuxOptions, RiffSegmentLimit,
};

/// Synthetic registry entry for the FOURCC ↔ codec_id mapping the
/// tests below need. Avoids a producer-crate dev-dep — real
/// MagicYUV decode coverage lives in `crates/oxideav-tests`.
fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv"))
        .tag(CodecTag::fourcc(b"M8RG"))
        .tag(CodecTag::fourcc(b"M8YA"));
    reg.register(info);
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

fn synthesize_payload(seed: u32, base_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(base_len);
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..base_len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// P1: LIST odml dmlh — cross-segment total-frame count.
// ---------------------------------------------------------------------------

#[test]
fn opendml_emits_list_odml_dmlh_in_hdrl() {
    // The OpenDML envelope always emits `LIST odml dmlh` so the
    // dmlh.dwTotalFrames count covers all segments. Verify the FourCCs
    // appear inside the hdrl LIST and that the dmlh body length is 4.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..6).map(|i| synthesize_payload(i + 700, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r3-dmlh-emit.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_kind(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
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
    let bytes = std::fs::read(&tmp).unwrap();
    // odml LIST appears (LIST + size + "odml" form-type).
    let odml_count = bytes.windows(4).filter(|w| *w == b"odml").count();
    assert!(odml_count >= 1, "expected `odml` form-type FourCC");
    // dmlh chunk appears with size = 4.
    let mut found = false;
    for i in 0..bytes.len().saturating_sub(8) {
        if &bytes[i..i + 4] == b"dmlh" {
            let sz = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]);
            assert_eq!(sz, 4, "dmlh body length must be 4 (single DWORD)");
            // Body is dwTotalFrames matching the actual frame count.
            let total =
                u32::from_le_bytes([bytes[i + 8], bytes[i + 9], bytes[i + 10], bytes[i + 11]]);
            assert_eq!(
                total as usize,
                frames.len(),
                "dmlh.dwTotalFrames must match real frame count"
            );
            found = true;
            break;
        }
    }
    assert!(found, "dmlh chunk not found");
}

#[test]
fn opendml_dmlh_total_frames_aggregates_across_segments() {
    // Force 2+ AVIX segments and verify dmlh.dwTotalFrames captures
    // the cross-segment total (not just the primary segment).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..12).map(|i| synthesize_payload(i + 800, 512)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r3-dmlh-cross.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_kind(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::Bytes(4 * 1024)),
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
    let bytes = std::fs::read(&tmp).unwrap();

    // Confirm we have multiple RIFF segments.
    let mut riff_count = 0;
    let mut cur = 0usize;
    while cur + 12 <= bytes.len() {
        if &bytes[cur..cur + 4] != b"RIFF" {
            break;
        }
        riff_count += 1;
        let sz = u32::from_le_bytes([
            bytes[cur + 4],
            bytes[cur + 5],
            bytes[cur + 6],
            bytes[cur + 7],
        ]) as usize;
        cur += 8 + sz + (sz & 1);
    }
    assert!(
        riff_count >= 2,
        "fixture must produce ≥ 2 RIFFs to validate the cross-segment claim"
    );

    // Demuxer surfaces dmlh value via metadata.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let md = dmx.metadata();
    let total_all = md
        .iter()
        .find(|(k, _)| k == "avi:total_frames_all_segments")
        .map(|(_, v)| v.clone())
        .expect("avi:total_frames_all_segments key must surface");
    assert_eq!(total_all, frames.len().to_string());
}

// ---------------------------------------------------------------------------
// P3: vprp Video Properties Header.
// ---------------------------------------------------------------------------

#[test]
fn opendml_emits_vprp_for_video_streams() {
    // OpenDML mode emits `vprp` chunks inside each video stream's
    // `strl`. Default values: nbFieldPerFrame = 1, frame_aspect_ratio
    // = 4:3, dimensions match the bitmap, refresh rate = fps.
    let stream = magicyuv_stream(128, 96);
    let payload = synthesize_payload(13, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r3-vprp-emit.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_kind(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let mut found = false;
    for i in 0..bytes.len().saturating_sub(8 + 36) {
        if &bytes[i..i + 4] == b"vprp" {
            let sz = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]])
                as usize;
            // We emit 9 fixed DWORDs (36 B) + 1 VIDEO_FIELD_DESC (32 B) = 68 B.
            assert_eq!(
                sz, 68,
                "vprp body length must be 68 (9 DWORDs + 1 field rect)"
            );
            let body = &bytes[i + 8..i + 8 + sz];
            // VideoFormatToken / VideoStandard = 0 (FORMAT_UNKNOWN /
            // STANDARD_UNKNOWN).
            assert_eq!(&body[0..4], &0u32.to_le_bytes());
            assert_eq!(&body[4..8], &0u32.to_le_bytes());
            // Vertical refresh rate = fps = 25.
            assert_eq!(&body[8..12], &25u32.to_le_bytes());
            // dwFrameAspectRatio = (4 << 16) | 3.
            let aspect = u32::from_le_bytes([body[20], body[21], body[22], body[23]]);
            assert_eq!(aspect, (4u32 << 16) | 3);
            // dwFrameWidthInPixels = 128.
            assert_eq!(&body[24..28], &128u32.to_le_bytes());
            // dwFrameHeightInLines = 96.
            assert_eq!(&body[28..32], &96u32.to_le_bytes());
            // nbFieldPerFrame = 1.
            assert_eq!(&body[32..36], &1u32.to_le_bytes());
            found = true;
            break;
        }
    }
    assert!(found, "vprp chunk not found in OpenDML output");
}

#[test]
fn opendml_vprp_round_trips_via_demuxer_metadata() {
    // After emit, the demuxer surfaces the vprp fields under
    // `avi:vprp.<index>.*`.
    let stream = magicyuv_stream(640, 480);
    let payload = synthesize_payload(31, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r3-vprp-roundtrip.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_kind(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
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
    let dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:vprp.0.frame_width_in_pixels").as_deref(),
        Some("640")
    );
    assert_eq!(
        get("avi:vprp.0.frame_height_in_lines").as_deref(),
        Some("480")
    );
    assert_eq!(get("avi:vprp.0.frame_aspect_ratio").as_deref(), Some("4:3"));
    assert_eq!(get("avi:vprp.0.nb_field_per_frame").as_deref(), Some("1"));
    assert_eq!(
        get("avi:vprp.0.vertical_refresh_rate").as_deref(),
        Some("25")
    );
}

#[test]
fn avi10_does_not_emit_vprp_or_dmlh() {
    // Legacy AVI 1.0 mode must NOT emit vprp / odml / dmlh — those are
    // OpenDML 2.0-only extensions and stamping them on a plain AVI 1.0
    // file would confuse round-trip-aware tools.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(2, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r3-no-vprp-on-avi10.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_kind(ws, std::slice::from_ref(&stream), AviKind::Avi10).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    assert!(
        !bytes.windows(4).any(|w| w == b"vprp"),
        "AVI 1.0 mode must not emit vprp"
    );
    assert!(
        !bytes.windows(4).any(|w| w == b"odml"),
        "AVI 1.0 mode must not emit LIST odml"
    );
    assert!(
        !bytes.windows(4).any(|w| w == b"dmlh"),
        "AVI 1.0 mode must not emit dmlh"
    );
}

// ---------------------------------------------------------------------------
// P4: LIST rec cluster emission.
// ---------------------------------------------------------------------------

#[test]
fn rec_cluster_groups_packets_inside_movi() {
    // Opt-in clustering: 8 packets, cap = 3 → 3 clusters of 3, 3, 2
    // packets. We expect at least 3 `LIST rec ` cluster signatures
    // and full byte round-trip on the demux side (the demuxer
    // already enters LIST rec automatically).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..8).map(|i| synthesize_payload(i + 900, 128)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r3-rec-cluster.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::new().with_rec_cluster_packets(3),
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
    let bytes = std::fs::read(&tmp).unwrap();
    // Count `LIST rec ` signatures: an 8-byte LIST header + "rec "
    // form-type at offset+8. We approximate by counting "rec "
    // FourCCs that immediately follow a "LIST" + 4-byte size.
    let mut rec_clusters = 0;
    for i in 0..bytes.len().saturating_sub(12) {
        if &bytes[i..i + 4] == b"LIST" && &bytes[i + 8..i + 12] == b"rec " {
            rec_clusters += 1;
        }
    }
    assert!(
        rec_clusters >= 3,
        "expected ≥ 3 `LIST rec ` clusters for 8 frames at cap=3, got {rec_clusters}"
    );

    // Round-trip: demuxer recovers all 8 frames byte-equal even with
    // LIST rec clustering in the way.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), frames.len());
    for (i, (g, s)) in got.iter().zip(frames.iter()).enumerate() {
        assert_eq!(g, s, "frame {i} byte mismatch after rec-cluster round-trip");
    }
}

#[test]
fn rec_cluster_disabled_by_default() {
    // No options → no `LIST rec ` clusters at all.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..6).map(|i| synthesize_payload(i + 950, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r3-rec-default-off.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_kind(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
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
    let bytes = std::fs::read(&tmp).unwrap();
    let rec_clusters = (0..bytes.len().saturating_sub(12))
        .filter(|&i| &bytes[i..i + 4] == b"LIST" && &bytes[i + 8..i + 12] == b"rec ")
        .count();
    assert_eq!(rec_clusters, 0, "rec clustering must be off by default");
}
