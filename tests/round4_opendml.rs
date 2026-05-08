//! Round-4 OpenDML 2.0 feature-completeness tests.
//!
//! Covers:
//! - **P1** 2-field encoder: muxer emits `AVI_INDEX_2FIELD` super
//!   index + 12-byte std-index entries with `dwOffsetField2`
//!   per OpenDML 2.0 §3.0 "AVI Field Index Chunk".
//! - **P2** vprp populator API: per-stream NTSC/PAL/SECAM token +
//!   non-default aspect ratio + interlaced field framing.
//! - **P3** dwOffsetField2 surfaced via `Demuxer::metadata()` so
//!   2-field-aware downstream consumers can detect interlaced
//!   carriage and read per-frame field-2 offsets.
//! - **P4** LIST rec cluster threshold by byte budget.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, MediaType, Muxer, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{
    open_avi, open_with_options, AviKind, AviMuxOptions, RiffSegmentLimit, VprpConfig,
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
// P1 / P3: AVI_INDEX_2FIELD encoder + dwOffsetField2 round-trip via demuxer
// ---------------------------------------------------------------------------

#[test]
fn opendml_field2_index_emits_12_byte_entries() {
    // OpenDML 2.0 §3.0 "AVI Field Index Chunk": when the std-index
    // is a 2-field index, `wLongsPerEntry == 3`, `bIndexSubType ==
    // AVI_INDEX_2FIELD = 0x01`, and each entry is 12 B carrying
    // (dwOffset, dwSize, dwOffsetField2).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 4000, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-2field-emit.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_field2_stream(0);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            // Mark the second field as starting at half-payload so
            // dwOffsetField2 has a meaningful value.
            mux.set_field2_offset((payload.len() / 2) as u32);
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();

    // Find the ix00 chunk and check its preamble.
    let mut found = false;
    for i in 0..bytes.len().saturating_sub(32) {
        if &bytes[i..i + 4] == b"ix00" {
            let sz = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]])
                as usize;
            let body = &bytes[i + 8..i + 8 + sz];
            // wLongsPerEntry = 3.
            assert_eq!(
                u16::from_le_bytes([body[0], body[1]]),
                3,
                "wLongsPerEntry must be 3 for 2-field std-index"
            );
            // bIndexSubType = 0x01 (AVI_INDEX_2FIELD).
            assert_eq!(body[2], 0x01, "bIndexSubType must be AVI_INDEX_2FIELD");
            // bIndexType = 0x01 (AVI_INDEX_OF_CHUNKS).
            assert_eq!(body[3], 0x01, "bIndexType must be AVI_INDEX_OF_CHUNKS");
            let n_entries = u32::from_le_bytes([body[4], body[5], body[6], body[7]]) as usize;
            assert_eq!(n_entries, frames.len());
            // Each entry is 12 B (dwOffset, dwSize, dwOffsetField2).
            for k in 0..n_entries {
                let base = 24 + k * 12;
                let dw_off2 = u32::from_le_bytes([
                    body[base + 8],
                    body[base + 9],
                    body[base + 10],
                    body[base + 11],
                ]);
                // dwOffsetField2 is qwBaseOffset-relative; should be
                // non-zero because we set a payload-relative offset.
                assert!(dw_off2 > 0, "dwOffsetField2 entry {k} should be non-zero");
            }
            found = true;
            break;
        }
    }
    assert!(found, "ix00 chunk not found in 2-field OpenDML output");
}

#[test]
fn opendml_field2_super_index_inherits_subtype() {
    // OpenDML 2.0 §3.0 "Super Index Chunk": when the pointed-to
    // standard indexes are field indexes, the super index also
    // carries `bIndexSubType = AVI_INDEX_2FIELD`.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(1234, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-2field-superidx.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_field2_stream(0);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        mux.set_field2_offset(32);
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();

    // Find the indx chunk and verify bIndexSubType = 0x01.
    let mut found = false;
    for i in 0..bytes.len().saturating_sub(32) {
        if &bytes[i..i + 4] == b"indx" {
            let body_off = i + 8;
            // body[2] = bIndexSubType.
            assert_eq!(
                bytes[body_off + 2],
                0x01,
                "indx super-index must carry AVI_INDEX_2FIELD subtype"
            );
            found = true;
            break;
        }
    }
    assert!(found, "indx super-index chunk not found");
}

#[test]
fn opendml_field2_offsets_round_trip_via_demuxer_metadata() {
    // The demuxer surfaces `avi:ix.<index>.is_2field = true` and
    // `avi:ix.<index>.field2_offsets = "<comma-separated>"` for
    // 2-field interlaced streams (round-4 P3).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synthesize_payload(i + 5000, 200)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-2field-meta.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_field2_stream(0);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            mux.set_field2_offset(100); // half-way through each 200-byte payload
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    assert_eq!(get("avi:ix.0.is_2field").as_deref(), Some("true"));
    let offsets =
        get("avi:ix.0.field2_offsets").expect("field2_offsets must surface on 2-field stream");
    let parsed: Vec<u32> = offsets
        .split(',')
        .map(|s| s.parse::<u32>().unwrap())
        .collect();
    assert_eq!(parsed.len(), frames.len(), "one field2 offset per frame");
    // Each offset must be > 100 because the muxer converted the
    // payload-relative 100 to qwBaseOffset-relative form (=
    // d_chunk_data + 100 where d_chunk_data is the data offset
    // inside movi).
    for off in &parsed {
        assert!(
            *off > 100,
            "qwBaseOffset-relative offset must be > payload-relative input"
        );
    }
}

#[test]
fn non_field2_streams_emit_8_byte_entries() {
    // Without `.with_field2_stream(0)`, the std-index keeps the
    // round-3 8-byte entry layout (`wLongsPerEntry == 2`) — i.e.
    // 2-field encoder is opt-in.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(1, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-nofield2.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::new(),
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

    // ix00 must use the default 8-byte entry layout.
    let mut found = false;
    for i in 0..bytes.len().saturating_sub(12) {
        if &bytes[i..i + 4] == b"ix00" {
            let body = &bytes[i + 8..];
            assert_eq!(u16::from_le_bytes([body[0], body[1]]), 2);
            assert_eq!(body[2], 0); // bIndexSubType = 0 (default)
            found = true;
            break;
        }
    }
    assert!(found, "ix00 chunk not found");
}

// ---------------------------------------------------------------------------
// P2: vprp populator API for NTSC/PAL/SECAM and custom aspect/framing.
// ---------------------------------------------------------------------------

#[test]
fn vprp_ntsc_preset_overrides_defaults() {
    // VprpConfig::ntsc() should stamp NTSC_CCIR_601 + STANDARD_NTSC
    // + 60 Hz refresh + nbFieldPerFrame=2 onto the vprp body.
    let stream = magicyuv_stream(720, 480);
    let payload = synthesize_payload(7, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-vprp-ntsc.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::ntsc());
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
    let bytes = std::fs::read(&tmp).unwrap();

    let mut found = false;
    for i in 0..bytes.len().saturating_sub(8 + 36) {
        if &bytes[i..i + 4] == b"vprp" {
            let sz = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]])
                as usize;
            // Round-9 candidate 1: muxer now emits one VIDEO_FIELD_DESC
            // record per field instead of always emitting a single
            // record. NTSC preset (nbFieldPerFrame=2) → 36 + 2*32 = 100.
            assert_eq!(sz, 100);
            let body = &bytes[i + 8..i + 8 + sz];
            // VideoFormatToken = NTSC_CCIR_601 = 4.
            assert_eq!(u32::from_le_bytes([body[0], body[1], body[2], body[3]]), 4);
            // VideoStandard = STANDARD_NTSC = 2.
            assert_eq!(u32::from_le_bytes([body[4], body[5], body[6], body[7]]), 2);
            // dwVerticalRefreshRate = 60.
            assert_eq!(
                u32::from_le_bytes([body[8], body[9], body[10], body[11]]),
                60
            );
            // nbFieldPerFrame = 2.
            assert_eq!(
                u32::from_le_bytes([body[32], body[33], body[34], body[35]]),
                2
            );
            found = true;
            break;
        }
    }
    assert!(found, "vprp chunk not found in NTSC OpenDML output");
}

#[test]
fn vprp_pal_preset_round_trips_via_demuxer_metadata() {
    // VprpConfig::pal() → STANDARD_PAL=1, refresh 50, nbField=2.
    let stream = magicyuv_stream(720, 576);
    let payload = synthesize_payload(13, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-vprp-pal.avi");
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
    let dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    assert_eq!(
        get("avi:vprp.0.video_format_token").as_deref(),
        Some("2") // PAL_CCIR_601
    );
    assert_eq!(get("avi:vprp.0.video_standard").as_deref(), Some("1")); // STANDARD_PAL
    assert_eq!(
        get("avi:vprp.0.vertical_refresh_rate").as_deref(),
        Some("50")
    );
    assert_eq!(get("avi:vprp.0.nb_field_per_frame").as_deref(), Some("2"));
}

#[test]
fn vprp_custom_aspect_ratio_round_trips() {
    // 16:9 explicit aspect via VprpConfig::with_aspect.
    let stream = magicyuv_stream(1920, 1080);
    let payload = synthesize_payload(99, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-vprp-16x9.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let cfg = VprpConfig::default().with_aspect(16, 9);
        let opts = AviMuxOptions::new().with_vprp(0, cfg);
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
    let dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    assert_eq!(
        get("avi:vprp.0.frame_aspect_ratio").as_deref(),
        Some("16:9")
    );
}

// ---------------------------------------------------------------------------
// P4: LIST rec cluster threshold by byte budget.
// ---------------------------------------------------------------------------

#[test]
fn rec_cluster_byte_budget_closes_clusters_predictably() {
    // 8 packets at 256 bytes each = 264 bytes per cluster entry
    // (8-byte chunk header + 256 payload). Budget = 600 bytes
    // means 2 packets per cluster (264 + 264 = 528 stays in;
    // adding another would push to 792 > 600 → close+reopen).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..8)
        .map(|i| synthesize_payload(i + 10_000, 256))
        .collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-rec-bytes.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_rec_cluster_bytes(600);
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
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
    let bytes = std::fs::read(&tmp).unwrap();
    // Count `LIST rec ` clusters.
    let rec_clusters = (0..bytes.len().saturating_sub(12))
        .filter(|&i| &bytes[i..i + 4] == b"LIST" && &bytes[i + 8..i + 12] == b"rec ")
        .count();
    // 8 packets / 2 per cluster = 4 clusters.
    assert!(
        rec_clusters >= 3,
        "byte-budget clustering should produce ≥ 3 clusters for 8x256 frames at 600 B budget; got {rec_clusters}"
    );
}

#[test]
fn rec_cluster_byte_budget_round_trips_demuxer() {
    // VBR-style stream (varying packet sizes); byte budget keeps
    // each cluster ≤ ~512 bytes. Demuxer must still recover all
    // frames byte-equal.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..6)
        .map(|i| {
            let len = 64 + (i as usize) * 80;
            synthesize_payload(i + 11_000, len)
        })
        .collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-rec-bytes-rt.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_rec_cluster_bytes(512);
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
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
        assert_eq!(g, s, "frame {i} byte mismatch after byte-budget round-trip");
    }
}

#[test]
fn rec_cluster_bytes_below_min_treated_as_off() {
    // n < 256 → no clustering at all (matches packet-cap n < 2 rule).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synthesize_payload(i + 12_000, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r4-rec-bytes-off.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_rec_cluster_bytes(100); // < 256
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
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
    let bytes = std::fs::read(&tmp).unwrap();
    let rec_clusters = (0..bytes.len().saturating_sub(12))
        .filter(|&i| &bytes[i..i + 4] == b"LIST" && &bytes[i + 8..i + 12] == b"rec ")
        .count();
    assert_eq!(
        rec_clusters, 0,
        "byte budget < 256 must be treated as no clustering"
    );
}
