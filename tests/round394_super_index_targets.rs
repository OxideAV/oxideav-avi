//! Round 394 — OpenDML 2.0 super-index entries must point at the
//! `ix##` standard-index chunks themselves.
//!
//! Clean-room source: `docs/container/riff/opendml-avi-2.0.pdf`
//! §"AVI Super Index Chunk" — each `_avisuperindex_entry` carries the
//! *"absolute file offset"* of an AVISTDINDEX chunk in `qwOffset`
//! (the spec's inline comment marks offset `0` as an unused entry),
//! the *"size of index chunk at this offset"* in `dwSize`, and the
//! *"time span in stream ticks"* in `dwDuration` — plus §"Index
//! Locations in RIFF File" ("New 'ix##' chunks can be added to grow
//! the file", which is why a single segment may contribute several
//! entries under mid-`movi` flushing).
//!
//! Covers the muxer fix (entries point at `ix00` chunk headers, not
//! at the enclosing RIFF segments), the new typed
//! `super_index_entries` accessor, the `super_index_target_violations`
//! cross-check + `avi:indx.<n>.stale_targets` divergence key, and the
//! mid-`movi` multi-`ix##`-per-segment shape.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer as _, MediaType, Packet,
    Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};

fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG"));
    reg.register(info);
    reg
}

fn video_stream() -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(64);
    params.height = Some(64);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn audio_stream() -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn payload(seed: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed.wrapping_mul(0x9E37_79B9).wrapping_add(1);
    for _ in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 24) as u8);
    }
    out
}

/// Mux `n` video frames of `frame_len` bytes with the given options,
/// returning the file bytes (staged through a temp file — the muxer
/// consumes its writer, so the bytes are read back from disk).
fn mux_video(
    tag: &str,
    n: usize,
    frame_len: usize,
    limit: RiffSegmentLimit,
    opts: AviMuxOptions,
) -> Vec<u8> {
    let stream = video_stream();
    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r394-{tag}.avi"));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(limit),
            opts,
        )
        .unwrap();
        use oxideav_core::Muxer as _;
        mux.write_header().unwrap();
        for i in 0..n {
            let mut pkt = Packet::new(0, stream.time_base, payload(i as u32, frame_len));
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Byte-scan every `ix00` chunk header: returns `(offset, 8 + cb)`.
fn find_ix00_chunks(bytes: &[u8]) -> Vec<(u64, u32)> {
    let mut out = Vec::new();
    let mut k = 0usize;
    while k + 8 <= bytes.len() {
        if &bytes[k..k + 4] == b"ix00" {
            let cb =
                u32::from_le_bytes([bytes[k + 4], bytes[k + 5], bytes[k + 6], bytes[k + 7]]) as u64;
            if cb >= 24 && (k as u64 + 8 + cb) <= bytes.len() as u64 {
                out.push((k as u64, (8 + cb) as u32));
                k += (8 + cb) as usize;
                continue;
            }
        }
        k += 1;
    }
    out
}

fn open_demuxer(bytes: &[u8]) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.to_vec()));
    oxideav_avi::demuxer::open_avi(rs, &reg).unwrap()
}

#[test]
fn super_index_entries_target_ix_chunks_multi_segment() {
    // 4 KiB ceiling + 512 B frames → several RIFF segments, one ix00
    // at each movi tail.
    let bytes = mux_video(
        "targets",
        16,
        512,
        RiffSegmentLimit::Bytes(4 * 1024),
        AviMuxOptions::default(),
    );
    let ix = find_ix00_chunks(&bytes);
    assert!(ix.len() >= 2, "expected multi-segment fixture");

    let dmx = open_demuxer(&bytes);
    let entries = dmx
        .super_index_entries(0)
        .expect("stream 0 declares an indx super-index");
    assert_eq!(entries.len(), ix.len(), "one entry per ix00 chunk");
    for (e, &(off, size)) in entries.iter().zip(ix.iter()) {
        assert_eq!(e.qw_offset, off, "qwOffset = ix00 header offset");
        assert_ne!(e.qw_offset, 0, "0 is the spec's unused-entry mark");
        assert_eq!(e.dw_size, size, "dwSize = ix00 chunk size");
    }
    let total: u64 = entries.iter().map(|e| e.dw_duration as u64).sum();
    assert_eq!(total, 16, "durations partition the frame count");

    // The demuxer's own cross-check agrees the targets are live.
    assert!(
        dmx.super_index_target_violations().is_empty(),
        "spec-correct writer output must produce no stale targets"
    );
    assert!(
        !dmx.metadata()
            .iter()
            .any(|(k, _)| k == "avi:indx.0.stale_targets"),
        "divergence-only key absent for the well-formed case"
    );
}

#[test]
fn super_index_entries_accessor_absent_for_avi10() {
    let stream = video_stream();
    let tmp = std::env::temp_dir().join("oxideav-avi-r394-avi10.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::default(),
        )
        .unwrap();
        use oxideav_core::Muxer as _;
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload(1, 128));
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);

    let dmx = open_demuxer(&bytes);
    assert!(
        dmx.super_index_entries(0).is_none(),
        "AVI 1.0 file has no indx super-index"
    );
    assert!(dmx.super_index_target_violations().is_empty());
}

#[test]
fn mid_movi_flush_gets_one_entry_per_ix_chunk() {
    // Periodic mid-movi flush every 3 packets, 10 packets in a single
    // segment (large ceiling) → 4 ix00 chunks (3+3+3+1), each with its
    // own super-index entry per the spec's growth model.
    let bytes = mux_video(
        "midmovi",
        10,
        256,
        RiffSegmentLimit::OneGiB,
        AviMuxOptions::default().with_mid_movi_index(0, 3),
    );
    let ix = find_ix00_chunks(&bytes);
    assert_eq!(ix.len(), 4, "3+3+3 mid-movi flushes + segment tail");

    let dmx = open_demuxer(&bytes);
    let entries = dmx.super_index_entries(0).expect("indx present");
    assert_eq!(entries.len(), 4, "one super-index entry per ix00 chunk");
    for (e, &(off, size)) in entries.iter().zip(ix.iter()) {
        assert_eq!(e.qw_offset, off);
        assert_eq!(e.dw_size, size);
    }
    // Durations: 3, 3, 3, 1 frames in file order.
    let durations: Vec<u32> = entries.iter().map(|e| e.dw_duration).collect();
    assert_eq!(durations, vec![3, 3, 3, 1]);
    assert!(dmx.super_index_target_violations().is_empty());
}

#[test]
fn audio_super_index_durations_are_sample_ticks() {
    // Audio-only OpenDML file: stream 0 is PCM s16le stereo, so one
    // packet of 96 bytes = 24 samples (block_align 4). dwDuration is
    // in stream ticks = samples for CBR audio.
    let stream = audio_stream();
    let tmp = std::env::temp_dir().join("oxideav-avi-r394-audio.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::Bytes(4 * 1024)),
            AviMuxOptions::default(),
        )
        .unwrap();
        use oxideav_core::Muxer as _;
        mux.write_header().unwrap();
        for i in 0..24 {
            let mut pkt = Packet::new(0, stream.time_base, payload(i, 96));
            pkt.pts = Some(i as i64 * 24);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();
    let entries = dmx.super_index_entries(0).expect("indx present");
    assert!(entries.len() >= 2, "multi-segment audio fixture");
    let total: u64 = entries.iter().map(|e| e.dw_duration as u64).sum();
    assert_eq!(total, 24 * 24, "sum of tick spans = total samples");
    assert!(dmx.super_index_target_violations().is_empty());
}

#[test]
fn stale_targets_fire_on_riff_segment_offsets() {
    // Reproduce the legacy (pre-round-394) writer shape by re-pointing
    // the super-index entries at the RIFF segment headers, then verify
    // the demuxer's cross-check + divergence key fire. Entry 0 is
    // rewritten to 0 (the legacy primary-segment stamp) — that's the
    // spec's unused-entry sentinel, so only the non-zero rewritten
    // entries count as stale.
    let mut bytes = mux_video(
        "stale",
        16,
        512,
        RiffSegmentLimit::Bytes(4 * 1024),
        AviMuxOptions::default(),
    );

    // Find the RIFF segment offsets.
    let mut riff_offsets: Vec<u64> = Vec::new();
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
        riff_offsets.push(cursor as u64);
        cursor += (8 + sz + (sz & 1)) as usize;
    }
    assert!(riff_offsets.len() >= 2);

    // Locate the indx chunk and rewrite each entry's qwOffset to the
    // matching RIFF segment offset (the legacy shape).
    let indx_off = (0..bytes.len() - 4)
        .find(|&k| &bytes[k..k + 4] == b"indx")
        .expect("indx chunk");
    let payload_off = indx_off + 8;
    let n_entries = u32::from_le_bytes([
        bytes[payload_off + 4],
        bytes[payload_off + 5],
        bytes[payload_off + 6],
        bytes[payload_off + 7],
    ]) as usize;
    assert_eq!(n_entries, riff_offsets.len());
    for (i, &off) in riff_offsets.iter().enumerate() {
        let base = payload_off + 24 + i * 16;
        bytes[base..base + 8].copy_from_slice(&off.to_le_bytes());
    }

    let dmx = open_demuxer(&bytes);
    let violations = dmx.super_index_target_violations();
    // Entry 0 (offset 0 = unused sentinel) is skipped; all remaining
    // entries point at RIFF headers, not ix00 chunks.
    assert_eq!(
        violations.len(),
        riff_offsets.len() - 1,
        "every non-zero RIFF-offset entry is stale"
    );
    assert!(violations.iter().all(|v| v.stream_index == 0));
    assert_eq!(violations[0].entry_index, 1, "entry 0 skipped as unused");
    let meta = dmx.metadata();
    let stale = meta
        .iter()
        .find(|(k, _)| k == "avi:indx.0.stale_targets")
        .expect("divergence key present");
    assert_eq!(stale.1, (riff_offsets.len() - 1).to_string());

    // The file stays fully demuxable — the demuxer scans movi for ix00
    // chunks instead of dereferencing qwOffset.
    let mut dmx = dmx;
    let mut n = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => n += 1,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(n, 16, "stale super-index never blocks linear demux");
}
