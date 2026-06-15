//! Round-312: surface the `indx` AVISUPERINDEX `dwChunkId` FOURCC on
//! the demuxer's public API + a divergence-only `avi:indx.<n>.chunk_id`
//! metadata key.
//!
//! Per the AVISUPERINDEX layout in
//! `docs/container/riff/avi-riff-file-reference.md` Appendix F (the
//! `dwChunkId` row: *"FOURCC of chunks indexed (e.g., '00dc')."*) and
//! the base AVIMETAINDEX in Appendix E (`dwChunkId` row: *"FOURCC of
//! chunks indexed (e.g., '00dc'); for super index only."*), this DWORD
//! declares which `movi` data-chunk FOURCC every `ix##` standard-index
//! segment referenced by this super-index points at. For a well-formed
//! AVI 2.0 file it spells the indexed stream's own packet FourCC —
//! `00dc` / `00wb` for stream 0 — so the two leading ASCII digits
//! encode the same stream number as the `strl` the super-index lives in.
//!
//! The demuxer parsed the FOURCC (it tags every `ix##` slot) but never
//! surfaced it. Round-312 closes that gap:
//!
//! - `AviDemuxer::super_index_chunk_id(stream) -> Option<[u8; 4]>`
//!   returns the raw 4 bytes verbatim, `None` for streams without an
//!   `indx`.
//! - `avi:indx.<n>.chunk_id` metadata key emits only when the parsed
//!   FOURCC's two leading ASCII stream-digits do NOT decode to the
//!   super-index's own stream slot (a cross-wired / malformed file);
//!   the canonical own-slot FOURCC is omitted so absence stays
//!   observable, per the round-304/197/176/153 "default == absent"
//!   convention. The typed accessor returns the raw value either way.

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
/// are flushed without downcasting the boxed `WriteSeek`).
fn write_opendml_avi() -> Vec<u8> {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 5_000, 128)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r312-superidx-chunkid-src.avi");
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

/// Locate the first `indx` super-index chunk and return the file offset
/// of its `dwChunkId` DWORD (chunk-payload byte 8 → file offset of the
/// `indx` FourCC + 8 header bytes + 8 preamble bytes).
fn indx_dwchunkid_offset(bytes: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"indx" {
            return i + 8 + 8;
        }
        i += 1;
    }
    panic!("no indx super-index chunk found in OpenDML file");
}

/// A well-formed OpenDML file written by this crate's muxer stamps the
/// stream's own packet FourCC (`00dc`) into the super-index `dwChunkId`.
/// The accessor surfaces `Some(*b"00dc")`; the divergence-only metadata
/// key is suppressed because the FOURCC decodes to the super-index's own
/// stream slot 0.
#[test]
fn opendml_super_index_chunk_id_round_trips_canonical() {
    let bytes = write_opendml_avi();
    let reg = registry_with_magicyuv();

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_chunk_id(0),
        Some(*b"00dc"),
        "stream 0 magicyuv super-index dwChunkId is the stream's own packet FourCC 00dc"
    );

    // Out-of-range streams read as None.
    assert_eq!(typed_dmx.super_index_chunk_id(99), None);

    // The metadata key is suppressed because the FOURCC matches the
    // super-index's own slot.
    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.chunk_id").is_none(),
        "a canonical own-slot dwChunkId must not emit the chunk_id metadata key"
    );
}

/// A super-index whose `dwChunkId` declares a *different* stream's
/// chunks than the `strl` it sits in (a cross-wired / malformed file):
/// the accessor still surfaces the raw bytes verbatim, and the metadata
/// key fires with the divergent FOURCC so a repair tool can detect it.
#[test]
fn divergent_super_index_chunk_id_surfaces_via_metadata() {
    let mut bytes = write_opendml_avi();
    let reg = registry_with_magicyuv();

    // Cross-wire stream 0's super-index dwChunkId to declare `01dc`
    // (stream 1's chunks) even though it lives in stream 0's strl.
    let off = indx_dwchunkid_offset(&bytes);
    assert_eq!(
        &bytes[off..off + 4],
        b"00dc",
        "precondition: muxer stamped the canonical 00dc dwChunkId"
    );
    bytes[off..off + 4].copy_from_slice(b"01dc");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_chunk_id(0),
        Some(*b"01dc"),
        "the demuxer surfaces the divergent dwChunkId verbatim"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:indx.0.chunk_id").as_deref(),
        Some("01dc"),
        "a cross-wired dwChunkId must emit the chunk_id metadata key verbatim"
    );
}

/// An AVI-1.0 file has no super-index at all (the `indx` chunk is
/// `AviKind::OpenDml`-only). The accessor distinguishes "no super-index
/// declared" from any FOURCC by returning `None`; the metadata key
/// never emits.
#[test]
fn avi10_file_has_no_super_index_chunk_id() {
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(3_333, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r312-avi10-no-superidx-chunkid.avi");
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

    assert_eq!(
        typed_dmx.super_index_chunk_id(0),
        None,
        "AVI 1.0 has no super-index → accessor must return None"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.chunk_id").is_none(),
        "AVI 1.0 must not emit the chunk_id metadata key"
    );
}
