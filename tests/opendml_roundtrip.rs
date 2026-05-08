//! OpenDML 2.0 + MagicYUV-FourCC integration tests for oxideav-avi.
//!
//! These exercise the AVIX continuation walker on the demuxer side
//! and the [`AviKind::OpenDml`] / [`RiffSegmentLimit`] emit path on
//! the muxer side. The test fixtures synthesise opaque payloads (no
//! actual codec is involved) and round-trip the chunk bytes through
//! the AVI envelope.
//!
//! Single-stream MagicYUV is the natural exemplar — its native
//! FourCC family lives in the codec crate's `register_codecs` — but
//! the underlying AVI logic is codec-agnostic; the same envelope
//! works for any video codec the muxer can package.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, MediaType, Packet, Rational, ReadSeek,
    StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{open_with_kind, AviKind, RiffSegmentLimit};

/// Build a CodecRegistry pre-populated with `oxideav-magicyuv`'s tag
/// claims (the 17 native v7 FourCCs) so the demuxer's forward
/// `resolve_tag(M8RG → "magicyuv")` direction works. The muxer no
/// longer asks the registry the inverse question — wire FourCCs
/// flow through `CodecParameters::tag` set by the caller (or by
/// the encoder's `output_params()`).
fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    oxideav_magicyuv::register_codecs(&mut reg);
    reg
}

/// One stream of single-stream MagicYUV at 25 fps with the M8RG
/// FourCC stamped on `params.tag` (caller-side equivalent of what
/// either the demuxer or the magicyuv encoder would set).
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
fn magicyuv_fourcc_round_trips_via_params_tag() {
    // Exercises the new wire-tag plumbing: the producer (here a
    // synthetic stream with `params.tag = CodecTag::fourcc(M8RG)`)
    // tells the muxer which FourCC to write, and the demuxer's
    // forward `CodecResolver::resolve_tag(M8RG)` recovers the
    // codec_id AND stamps the same `M8RG` back onto
    // `params.tag` so a second mux preserves the FourCC.
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();

    let payload = synthesize_payload(42, 256);
    let tmp = std::env::temp_dir().join("oxideav-avi-magicyuv-via-params-tag.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    // The on-wire BITMAPINFOHEADER must carry the FourCC the producer
    // asked for via `params.tag`.
    let bytes = std::fs::read(&tmp).unwrap();
    assert!(
        bytes.windows(4).any(|w| w == b"M8RG"),
        "expected M8RG FourCC somewhere in the muxer output",
    );

    // Demuxer round-trip: codec_id surfaces via the forward
    // `resolve_tag` direction; `params.tag` round-trips byte-for-byte.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "magicyuv");
    assert_eq!(
        dmx.streams()[0].params.tag,
        Some(CodecTag::fourcc(b"M8RG")),
        "params.tag must round-trip byte-for-byte through the AVI envelope",
    );
    let got = dmx.next_packet().unwrap();
    assert_eq!(got.data, payload);
}

#[test]
fn params_tag_picks_among_multiple_native_fourccs() {
    // When the producer sets `params.tag = CodecTag::fourcc(b"M8YA")`,
    // the muxer writes M8YA instead of M8RG. Lets a magicyuv encoder
    // (or any caller) pick which of the 17 native v7 variants to emit.
    let mut stream = magicyuv_stream(64, 64);
    stream.params.tag = Some(CodecTag::fourcc(b"M8YA"));
    let reg = registry_with_magicyuv();

    let payload = synthesize_payload(99, 128);
    let tmp = std::env::temp_dir().join("oxideav-avi-magicyuv-tag-override.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    assert!(bytes.windows(4).any(|w| w == b"M8YA"));
    // The demuxer should still resolve M8YA to "magicyuv" AND stamp
    // M8YA back onto `params.tag` (round-trip preservation).
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "magicyuv");
    assert_eq!(dmx.streams()[0].params.tag, Some(CodecTag::fourcc(b"M8YA")),);
}

#[test]
fn single_riff_avi_with_m8rg_fourcc_roundtrips_5_frames() {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..5).map(|i| synthesize_payload(i, 256)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-magicyuv-single.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_kind(ws, std::slice::from_ref(&stream), AviKind::Avi10).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Demuxer surfaces the magicyuv codec_id via the registry-based
    // CodecResolver path (no in-crate codec_map).
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    assert_eq!(dmx.format_name(), "avi");
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(
        dmx.streams()[0].params.codec_id.as_str(),
        "magicyuv",
        "CodecResolver should resolve M8RG → magicyuv via registry",
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
    let reg = registry_with_magicyuv();

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
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
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
    let reg = registry_with_magicyuv();

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
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
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
fn opendml_emits_ix_chunks_per_segment() {
    // OpenDML 2.0 §"Index Locations": each `RIFF AVIX` continuation
    // (and the primary RIFF) is expected to carry per-stream `ix##`
    // AVISTDINDEX chunks at the tail of its `movi` LIST. With 12
    // frames at 512 B and limit=4 KiB we land on multiple segments;
    // every segment that has at least one packet should have an
    // `ix00` chunk written after the packet data.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..12).map(|i| synthesize_payload(i + 400, 512)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-opendml-ix-chunks.avi");
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
    // Count distinct top-level RIFF segments.
    let mut riff_count = 0usize;
    let mut cursor = 0usize;
    while cursor + 12 <= bytes.len() {
        if &bytes[cursor..cursor + 4] != b"RIFF" {
            break;
        }
        riff_count += 1;
        let sz = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        cursor += 8 + sz + (sz & 1);
    }
    assert!(riff_count >= 2, "expected ≥ 2 RIFFs for ix## test");

    // Count `ix00` FourCCs in the file. Every segment with packets
    // emits one. Search by looking for the 8-byte chunk header
    // `ix00 <size>` and verifying the size > 32 (payload contains at
    // least the 32-byte preamble).
    let ix00_chunks: usize = bytes
        .windows(8)
        .filter(|w| {
            &w[0..4] == b"ix00" && {
                let sz = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
                sz >= 32
            }
        })
        .count();
    assert_eq!(
        ix00_chunks, riff_count,
        "expected one ix00 per segment, got {ix00_chunks} for {riff_count} RIFFs"
    );
}

#[test]
fn opendml_seek_via_std_index_when_idx1_missing() {
    // Synthesise an OpenDML AVI, then strip the `idx1` chunk so the
    // demuxer must fall back to the OpenDML `ix##` standard-index
    // path. Seek halfway through and verify the next packet's PTS
    // matches the requested seek target.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..16).map(|i| synthesize_payload(i + 500, 512)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-opendml-seek-ix.avi");
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

    // Replace `idx1` FourCC with `JUNK` so the demuxer treats it as
    // padding. The size field stays valid; `JUNK` chunks are
    // unconditionally skipped by the walker.
    let mut backing = std::fs::read(&tmp).unwrap();
    let mut found = None;
    for (i, w) in backing.windows(4).enumerate() {
        if w == b"idx1" {
            found = Some(i);
            break;
        }
    }
    let pos = found.expect("muxer always emits idx1");
    backing[pos..pos + 4].copy_from_slice(b"JUNK");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(backing));
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    // Now `seek_to` must use the std-index. Target frame 8 (mid-file).
    let landed = dmx
        .seek_to(0, 8)
        .expect("std-index-backed seek must succeed without idx1");
    // All frames are keyframes, so the landed pts matches the target.
    assert_eq!(landed, 8, "expected exact landing on frame-8");
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.stream_index, 0);
    let pts = pkt.pts.expect("pts set");
    assert!(
        pts >= landed,
        "post-seek pts {pts} should be ≥ landed {landed}"
    );
    // Payload should match frame 8 byte-for-byte.
    assert_eq!(
        pkt.data, frames[8],
        "post-seek packet must equal source frame 8 (byte-equal)"
    );
}

#[test]
fn opendml_metadata_surfaces_avih_fields() {
    // The demuxer's `metadata()` should expose key AVIMAINHEADER fields
    // under the `avi:*` namespace so consumers can introspect dimensions
    // / flags without re-parsing the file. Verify against a small
    // OpenDML round-trip.
    let stream = magicyuv_stream(128, 96);
    let payload = synthesize_payload(7, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-opendml-metadata.avi");
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
    assert_eq!(get("avi:width").as_deref(), Some("128"));
    assert_eq!(get("avi:height").as_deref(), Some("96"));
    assert_eq!(get("avi:streams").as_deref(), Some("1"));
    assert_eq!(get("avi:flags").as_deref(), Some("0x00000810"));
    // Truncated flag should NOT be set on a clean round-trip.
    assert_eq!(get("avi:truncated"), None);
}

#[test]
fn opendml_single_segment_when_limit_is_large() {
    // Generous limit → only one segment. Output should still parse,
    // and the indx super-index should declare exactly one entry.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 300, 256)).collect();
    let reg = registry_with_magicyuv();

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
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let mut count = 0;
    while let Ok(_p) = dmx.next_packet() {
        count += 1;
    }
    assert_eq!(count, 3);
}
