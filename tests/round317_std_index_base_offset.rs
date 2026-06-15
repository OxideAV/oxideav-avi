//! Round-317: surface the per-stream `ix##` AVISTDINDEX `qwBaseOffset`
//! on the demuxer's public API + a `movi`-region cross-check validator
//! and a violation-only `avi:ix.<n>.<seg>.base_outside_movi` metadata
//! key.
//!
//! Per the AVISTDINDEX layout in
//! `docs/container/riff/avi-riff-file-reference.md` Appendix G (the
//! `qwBaseOffset` row: *"Base offset (typically the file offset of the
//! 'movi' list)."*), every `AVISTDINDEX_ENTRY.dwOffset` is added to the
//! chunk-level `qwBaseOffset` to recover the file-absolute position of
//! the indexed data chunk. The demuxer already parsed and used this
//! base internally for OpenDML seeking, but never surfaced it. Round-317
//! closes that gap:
//!
//! - `AviDemuxer::std_index_base_offsets(stream) -> Vec<u64>` returns the
//!   verbatim `qwBaseOffset` per `ix##` chunk for the stream, in file
//!   order (one per segment); empty for AVI-1.0 / no-`ix##` files.
//! - `AviDemuxer::std_index_base_offset_violations()` returns one
//!   `StdIndexBaseOffsetViolation` per `ix##` whose `qwBaseOffset` falls
//!   outside every `movi` LIST region — informational, never fails
//!   `open()`.
//! - `avi:ix.<n>.<seg>.base_outside_movi` metadata key fires only on a
//!   violation; the well-formed in-`movi` case emits no key so absence
//!   stays observable, per the "default == absent" convention.

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
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 5_000, 128)).collect();

    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r317-stdidx-base-{tag}.avi"));
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

/// Locate the first `ix00` standard-index chunk and return the file
/// offset of its `qwBaseOffset` field. The AVISTDINDEX layout (Appendix
/// G) is: fcc(4) cb(4) | wLongsPerEntry(2) bIndexSubType(1) bIndexType(1)
/// nEntriesInUse(4) dwChunkId(4) qwBaseOffset(8) ... — so the
/// `qwBaseOffset` DWORDLONG begins at the `ix00` FourCC offset + 20.
fn ix00_qwbaseoffset_offset(bytes: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ix00" {
            return i + 20;
        }
        i += 1;
    }
    panic!("no ix00 standard-index chunk found in OpenDML file");
}

/// A well-formed OpenDML file written by this crate's muxer anchors the
/// `ix##` `qwBaseOffset` inside the `movi` LIST. The accessor surfaces
/// the verbatim base; the violation validator and metadata key stay
/// empty because the base anchors inside `movi`.
#[test]
fn opendml_std_index_base_offset_round_trips_inside_movi() {
    let bytes = write_opendml_avi("canonical");
    let reg = registry_with_magicyuv();

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.clone()));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    let bases = typed_dmx.std_index_base_offsets(0);
    assert_eq!(
        bases.len(),
        1,
        "single-segment OpenDML file has exactly one ix00 for stream 0"
    );
    let base = bases[0];
    assert!(base > 0, "qwBaseOffset is a real file offset, not zero");

    // The verbatim accessor matches the bytes physically stamped at the
    // ix00 chunk's qwBaseOffset field.
    let off = ix00_qwbaseoffset_offset(&bytes);
    let stamped = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
    assert_eq!(
        base, stamped,
        "accessor returns the verbatim on-disk qwBaseOffset"
    );

    // Out-of-range stream → empty Vec.
    assert!(typed_dmx.std_index_base_offsets(99).is_empty());

    // A canonical base anchored inside movi raises no violation.
    assert!(
        typed_dmx.std_index_base_offset_violations().is_empty(),
        "a qwBaseOffset inside movi must not raise a violation"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:ix.0.0.base_outside_movi").is_none(),
        "a canonical in-movi base must not emit the base_outside_movi key"
    );
}

/// An `ix##` whose `qwBaseOffset` is corrupted to point outside every
/// `movi` region: the accessor still surfaces the (now bogus) base
/// verbatim, and both the typed violation validator and the metadata key
/// fire so a repair tool can detect the malformed anchor.
#[test]
fn std_index_base_offset_outside_movi_surfaces_violation() {
    let mut bytes = write_opendml_avi("corrupt");
    let reg = registry_with_magicyuv();

    let off = ix00_qwbaseoffset_offset(&bytes);
    // Stamp a clearly-out-of-range base (far past EOF).
    let bogus: u64 = 0xDEAD_BEEF_0000_0000;
    bytes[off..off + 8].copy_from_slice(&bogus.to_le_bytes());

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    let bases = typed_dmx.std_index_base_offsets(0);
    assert_eq!(
        bases,
        vec![bogus],
        "accessor surfaces the bogus base verbatim"
    );

    let viols = typed_dmx.std_index_base_offset_violations();
    assert_eq!(viols.len(), 1, "one out-of-movi ix## raises one violation");
    assert_eq!(viols[0].stream_index, 0);
    assert_eq!(viols[0].segment_index, 0);
    assert_eq!(viols[0].qw_base_offset, bogus);

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:ix.0.0.base_outside_movi").as_deref(),
        Some(bogus.to_string().as_str()),
        "an out-of-movi base must emit the base_outside_movi metadata key verbatim"
    );
}

/// An AVI-1.0 file carries no `ix##` standard index at all (the chunk is
/// `AviKind::OpenDml`-only). The accessor returns an empty Vec, the
/// violation list is empty, and no metadata key emits.
#[test]
fn avi10_file_has_no_std_index_base_offsets() {
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(3_333, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r317-avi10-no-stdidx-base.avi");
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
        typed_dmx.std_index_base_offsets(0).is_empty(),
        "AVI 1.0 has no ix## → accessor must return an empty Vec"
    );
    assert!(
        typed_dmx.std_index_base_offset_violations().is_empty(),
        "AVI 1.0 has no ix## → no violations"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:ix.0.0.base_outside_movi").is_none(),
        "AVI 1.0 must not emit the base_outside_movi key"
    );
}
