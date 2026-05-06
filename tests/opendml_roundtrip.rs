//! OpenDML 2.0 + MagicYUV-FourCC integration tests for oxideav-avi.
//!
//! These exercise the AVIX continuation walker on the demuxer side
//! and the [`AviKind::OpenDml`] / [`RiffSegmentLimit`] emit path on
//! the muxer side. The test fixtures synthesise opaque payloads (no
//! actual codec is involved) and round-trip the chunk bytes through
//! the AVI envelope.
//!
//! Single-stream MagicYUV is the natural exemplar — its native
//! FourCC family lives in `codec_map.rs` — but the underlying AVI
//! logic is codec-agnostic; the same envelope works for any video
//! codec the muxer can package.

use oxideav_core::{
    CodecId, CodecParameters, MediaType, Packet, Rational, ReadSeek, StreamInfo, TimeBase,
    WriteSeek,
};

use oxideav_avi::muxer::{open_with_kind, AviKind, RiffSegmentLimit};

/// One stream of single-stream MagicYUV at 25 fps with the M8RG
/// FourCC defaulted via the codec_map's "magicyuv" path.
fn magicyuv_stream(width: u32, height: u32) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("magicyuv"));
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

/// Build a deterministic synthetic packet payload: 200..600 bytes of
/// pseudo-random content with `seed` mixed in. Even-length so no
/// padding is needed in the AVI movi LIST.
fn synthesize_payload(seed: u32, base_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(base_len);
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..base_len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

#[test]
fn single_riff_avi_with_m8rg_fourcc_roundtrips_5_frames() {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..5).map(|i| synthesize_payload(i, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-magicyuv-single.avi");
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

    // Demuxer surfaces the magicyuv codec_id via the FourCC map.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.format_name(), "avi");
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(
        dmx.streams()[0].params.codec_id.as_str(),
        "magicyuv",
        "FourCC map should resolve M8RG → magicyuv"
    );

    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), frames.len(), "frame count");
    for (i, (g, s)) in got.iter().zip(frames.iter()).enumerate() {
        assert_eq!(g, s, "frame {i} byte mismatch");
    }
}

#[test]
fn opendml_two_riff_segments_recover_8_frames_in_order() {
    // 8 frames of ~512 bytes each. With limit=2 KiB we force ≥ 2
    // RIFFs.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..8).map(|i| synthesize_payload(i + 100, 512)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-opendml-two-riffs.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_kind(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::Bytes(2 * 1024)),
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
    let riff_count = bytes.windows(4).filter(|w| *w == b"RIFF").count();
    assert!(
        riff_count >= 2,
        "expected ≥ 2 RIFF chunks (multi-segment OpenDML), got {riff_count}"
    );
    let avix_count = bytes.windows(4).filter(|w| *w == b"AVIX").count();
    assert!(avix_count >= 1, "expected ≥ 1 AVIX form, got {avix_count}");
    let indx_count = bytes.windows(4).filter(|w| *w == b"indx").count();
    assert_eq!(indx_count, 1, "exactly one indx super-index");

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "magicyuv");

    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), frames.len(), "frame count across all RIFFs");
    for (i, (g, s)) in got.iter().zip(frames.iter()).enumerate() {
        assert_eq!(g, s, "frame {i} byte mismatch");
    }
}

#[test]
fn opendml_indx_super_index_entries_match_riff_offsets() {
    // 16 frames at 512 B each, limit 4 KiB → multiple RIFFs. The
    // indx super-index must reflect each RIFF's qwOffset / dwSize /
    // dwDuration.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..16).map(|i| synthesize_payload(i + 200, 512)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-opendml-indx.avi");
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

    // Walk RIFF chunks at the top level.
    let mut riff_offsets_and_sizes: Vec<(u64, u64)> = Vec::new();
    let mut cursor = 0usize;
    while cursor + 12 <= bytes.len() {
        if &bytes[cursor..cursor + 4] != b"RIFF" {
            break;
        }
        let sz = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as u64;
        let total = 8 + sz + (sz & 1);
        riff_offsets_and_sizes.push((cursor as u64, total));
        cursor += total as usize;
    }
    assert!(riff_offsets_and_sizes.len() >= 2, "expected ≥ 2 RIFFs");

    // Locate `indx` chunk inside the first RIFF only.
    let first_riff_end = (riff_offsets_and_sizes[0].0 + riff_offsets_and_sizes[0].1) as usize;
    let mut indx_off: Option<usize> = None;
    let mut j = riff_offsets_and_sizes[0].0 as usize + 12;
    while j + 8 <= first_riff_end {
        if &bytes[j..j + 4] == b"indx" {
            indx_off = Some(j);
            break;
        }
        j += 4;
    }
    let indx_off = indx_off.expect("indx chunk in first RIFF");
    let payload_off = indx_off + 8;
    // Preamble: 2 + 1 + 1 + 4 + 4 + 12 = 24 B.
    let n_entries = u32::from_le_bytes([
        bytes[payload_off + 4],
        bytes[payload_off + 5],
        bytes[payload_off + 6],
        bytes[payload_off + 7],
    ]) as usize;
    assert_eq!(
        n_entries,
        riff_offsets_and_sizes.len(),
        "nEntriesInUse should match actual segment count"
    );
    let chunk_id = &bytes[payload_off + 8..payload_off + 12];
    assert_eq!(chunk_id, b"00dc", "indx dwChunkId should be '00dc'");

    let entries_start = payload_off + 24;
    let mut frames_seen: u64 = 0;
    for (i, &(expected_off, expected_size)) in riff_offsets_and_sizes.iter().enumerate() {
        let base = entries_start + 16 * i;
        let qw_off = u64::from_le_bytes([
            bytes[base],
            bytes[base + 1],
            bytes[base + 2],
            bytes[base + 3],
            bytes[base + 4],
            bytes[base + 5],
            bytes[base + 6],
            bytes[base + 7],
        ]);
        let dw_size = u32::from_le_bytes([
            bytes[base + 8],
            bytes[base + 9],
            bytes[base + 10],
            bytes[base + 11],
        ]) as u64;
        let dw_duration = u32::from_le_bytes([
            bytes[base + 12],
            bytes[base + 13],
            bytes[base + 14],
            bytes[base + 15],
        ]) as u64;
        assert_eq!(qw_off, expected_off, "indx[{i}].qwOffset");
        assert_eq!(dw_size, expected_size, "indx[{i}].dwSize");
        frames_seen += dw_duration;
    }
    assert_eq!(
        frames_seen,
        frames.len() as u64,
        "sum of dwDuration must equal total frame count"
    );

    // Demuxer round-trip: all 16 frames recovered byte-equal.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), 16, "all 16 frames recovered");
    for (i, (g, s)) in got.iter().zip(frames.iter()).enumerate() {
        assert_eq!(g, s, "frame {i} byte mismatch after OpenDML round-trip");
    }
}

#[test]
fn opendml_single_segment_when_limit_is_large() {
    // Generous limit → only one segment. Output should still parse,
    // and the indx super-index should declare exactly one entry.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 300, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-opendml-onesegment.avi");
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
    // Top-level walk for RIFF count (substring would over-count).
    let mut top_level_riffs = 0;
    let mut cursor = 0usize;
    while cursor + 12 <= bytes.len() {
        if &bytes[cursor..cursor + 4] != b"RIFF" {
            break;
        }
        top_level_riffs += 1;
        let sz = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let total = 8 + sz + (sz & 1);
        cursor += total;
    }
    assert_eq!(top_level_riffs, 1, "OpenDML with generous limit → 1 RIFF");

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let mut count = 0;
    while let Ok(_p) = dmx.next_packet() {
        count += 1;
    }
    assert_eq!(count, 3);
}
