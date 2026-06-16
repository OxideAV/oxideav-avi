//! Round-322: surface the per-segment `ix##` AVISTDINDEX `dwChunkId`
//! FOURCC on the demuxer's public API + a divergence-only
//! `avi:ix.<n>.<seg>.chunk_id` metadata key.
//!
//! Per the AVISTDINDEX / AVIMETAINDEX layout in
//! `docs/container/riff/avi-riff-file-reference.md` Appendix G (the
//! `dwChunkId` row: *"FOURCC of indexed chunks."*) and Appendix E
//! (*"FOURCC of chunks indexed (e.g., '00dc')."*), each `ix##` standard
//! index chunk's body declares which `movi` data-chunk FOURCC its entries
//! point at. For a well-formed file that FOURCC's two leading ASCII digits
//! encode the same stream the `ix##` chunk itself was emitted for. The
//! demuxer already parsed `dwChunkId` (and keyed each `ix##` to a stream by
//! it) but never surfaced the raw FOURCC. Round-322 closes that gap:
//!
//! - `AviDemuxer::std_index_chunk_ids(stream) -> Vec<[u8; 4]>` returns the
//!   verbatim `dwChunkId` per `ix##` chunk for the stream, in file order
//!   (one per segment); empty for AVI-1.0 / no-`ix##` files.
//! - `avi:ix.<n>.<seg>.chunk_id` metadata key fires only when the `ix##`
//!   chunk's own FourCC stream-digits diverge from the body's `dwChunkId`
//!   stream-digits (a cross-wired index); the canonical own-slot case emits
//!   no key so absence stays observable, per the "default == absent"
//!   convention.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG"));
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

/// Write a 3-frame single-magicyuv-stream OpenDML AVI and return its
/// final bytes (via a temp file, so the muxer's seek-back back-patches
/// are flushed).
fn write_opendml_avi(tag: &str) -> Vec<u8> {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 9_000, 128)).collect();

    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r322-stdidx-cid-{tag}.avi"));
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
    for (i, payload) in frames.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    std::fs::read(&tmp).unwrap()
}

/// Locate the first `ix00` standard-index chunk and return the file offset
/// of its `ix00` FourCC. The AVISTDINDEX layout (Appendix G) is:
/// fcc(4) cb(4) | wLongsPerEntry(2) bIndexSubType(1) bIndexType(1)
/// nEntriesInUse(4) dwChunkId(4) qwBaseOffset(8) ... — so `dwChunkId`
/// begins at the `ix00` FourCC offset + 16.
fn ix00_fourcc_offset(bytes: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ix00" {
            return i;
        }
        i += 1;
    }
    panic!("no ix00 standard-index chunk found in OpenDML file");
}

/// A well-formed OpenDML file written by this crate's muxer stamps a
/// canonical `dwChunkId` (`00dc` for the magicyuv video stream 0). The
/// accessor surfaces the verbatim FOURCC; the divergence metadata key
/// stays absent because the `ix00` chunk's own stream matches `dwChunkId`.
#[test]
fn opendml_std_index_chunk_id_round_trips_canonical() {
    let bytes = write_opendml_avi("canonical");
    let reg = registry_with_magicyuv();

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.clone()));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    let cids = typed_dmx.std_index_chunk_ids(0);
    assert_eq!(
        cids.len(),
        1,
        "single-segment OpenDML file has exactly one ix00 for stream 0"
    );
    assert_eq!(
        &cids[0], b"00dc",
        "magicyuv compressed video stream 0 indexes 00dc chunks"
    );

    // The verbatim accessor matches the bytes physically stamped at the
    // ix00 chunk's dwChunkId field (FourCC offset + 16).
    let off = ix00_fourcc_offset(&bytes) + 16;
    assert_eq!(
        &cids[0],
        &bytes[off..off + 4],
        "accessor returns the verbatim on-disk dwChunkId"
    );

    // Out-of-range stream → empty Vec.
    assert!(typed_dmx.std_index_chunk_ids(99).is_empty());

    // A canonical own-slot dwChunkId raises no divergence metadata key.
    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:ix.0.0.chunk_id").is_none(),
        "a canonical own-slot dwChunkId must not emit the chunk_id key"
    );
}

/// Corrupt the `ix00` chunk's own RIFF FourCC to `ix01` while leaving its
/// body `dwChunkId` as `00dc`: the standard index now declares stream 0's
/// chunks but was emitted under stream 1's `ix##` FourCC. The accessor
/// surfaces the verbatim `dwChunkId` (keyed by it, i.e. under stream 0),
/// and the divergence metadata key fires under the own-FourCC stream so a
/// repair tool can detect the cross-wiring.
#[test]
fn std_index_chunk_id_divergence_surfaces_metadata() {
    let mut bytes = write_opendml_avi("divergent");
    let reg = registry_with_magicyuv();

    let off = ix00_fourcc_offset(&bytes);
    // Rewrite the chunk-header FourCC `ix00` → `ix01`. The body's
    // dwChunkId stays `00dc` (stream 0), so own-stream (1) diverges.
    bytes[off..off + 4].copy_from_slice(b"ix01");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    // The ix## is still keyed to stream 0 by its dwChunkId, so the typed
    // accessor surfaces the verbatim 00dc there.
    let cids = typed_dmx.std_index_chunk_ids(0);
    assert_eq!(cids, vec![*b"00dc"], "dwChunkId surfaced verbatim");

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:ix.1.0.chunk_id").as_deref(),
        Some("00dc"),
        "a cross-wired ix## must emit the divergence chunk_id key under its own-FourCC stream"
    );
    // And the canonical (stream-0-keyed) key stays absent.
    assert!(
        get("avi:ix.0.0.chunk_id").is_none(),
        "the divergence key is filed under the own-FourCC stream, not the dwChunkId stream"
    );
}

/// An AVI-1.0 file carries no `ix##` standard index at all. The accessor
/// returns an empty Vec and no divergence metadata key emits.
#[test]
fn avi10_file_has_no_std_index_chunk_ids() {
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(2_222, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r322-avi10-no-stdidx-cid.avi");
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
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert!(
        typed_dmx.std_index_chunk_ids(0).is_empty(),
        "AVI 1.0 has no ix## → accessor must return an empty Vec"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:ix.0.0.chunk_id").is_none(),
        "AVI 1.0 must not emit the chunk_id divergence key"
    );
}
