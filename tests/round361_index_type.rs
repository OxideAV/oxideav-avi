//! Round-361: surface the OpenDML 2.0 `AVIMETAINDEX` `bIndexType` byte
//! on the demuxer's public API as a typed [`AviIndexType`], plus a
//! divergence-only `avi:indx.<n>.index_type` metadata key.
//!
//! Per the OpenDML 2.0 *"Index Structures"* `// bIndexType codes` block
//! (clean-room source: `docs/container/riff/opendml-avi-2.0.pdf`
//! §"Index Structures"), the base `AVIMETAINDEX` header carries a
//! one-byte `bIndexType` selecting how its `aIndex[]` table is read:
//!
//! - `AVI_INDEX_OF_INDEXES` (`0x00`) — entries point at sub-indexes
//!   (the `strl`-level `indx` super-index of indexes).
//! - `AVI_INDEX_OF_CHUNKS` (`0x01`) — entries point at `movi` data
//!   chunks (the per-segment `ix##` standard / field index).
//! - `AVI_INDEX_IS_DATA` (`0x80`) — entries are the data themselves.
//!
//! The demuxer read the byte (to *validate* it) but discarded the
//! value. Round-361 closes that gap:
//!
//! - `AviDemuxer::super_index_index_type(stream) -> Option<AviIndexType>`
//!   decodes the `indx`'s declared type, surfacing the value even for a
//!   present-but-mislabelled (entry-less) super-index, `None` for no
//!   `indx` at all.
//! - `AviDemuxer::std_index_index_types(stream) -> Vec<AviIndexType>`
//!   decodes every `ix##` segment's type in file order.
//! - `avi:indx.<n>.index_type` metadata key emits the decoded label only
//!   when the super-index's `bIndexType` diverges from the canonical
//!   `AVI_INDEX_OF_INDEXES`, per the round-312/304/197 "default ==
//!   absent" convention.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::AviIndexType;
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

/// Write a 3-frame single-magicyuv-stream OpenDML AVI to a temp file and
/// return its final bytes (so the muxer's seek-back back-patches flush).
/// `tag` keeps the temp path unique so parallel tests don't race the same
/// file.
fn write_opendml_avi(tag: &str) -> Vec<u8> {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 7_000, 128)).collect();

    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r361-index-type-{tag}.avi"));
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
/// of its `bIndexType` byte. The AVISUPERINDEX body begins 8 bytes past
/// the `indx` FourCC (4-byte FourCC + 4-byte size), and within the body
/// `bIndexType` is at offset 3 (`wLongsPerEntry`:2 + `bIndexSubType`:1).
fn indx_bindextype_offset(bytes: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"indx" {
            return i + 8 + 3;
        }
        i += 1;
    }
    panic!("no indx super-index chunk found in OpenDML file");
}

/// Locate the first `ix00` standard-index chunk and return the file
/// offset of its `bIndexType` byte (same body layout as the super-index:
/// FourCC + size = 8, then `bIndexType` at body offset 3).
fn ix00_bindextype_offset(bytes: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ix00" {
            return i + 8 + 3;
        }
        i += 1;
    }
    panic!("no ix00 standard-index chunk found in OpenDML file");
}

/// A well-formed OpenDML file: the `indx` declares `AVI_INDEX_OF_INDEXES`
/// and each `ix00` declares `AVI_INDEX_OF_CHUNKS`. The accessors decode
/// the canonical types; the divergence-only metadata key is suppressed.
#[test]
fn opendml_canonical_index_types() {
    let bytes = write_opendml_avi("canonical");
    let reg = registry_with_magicyuv();

    // Precondition: muxer stamped the canonical bytes.
    let indx_off = indx_bindextype_offset(&bytes);
    assert_eq!(
        bytes[indx_off], 0x00,
        "indx bIndexType = AVI_INDEX_OF_INDEXES"
    );
    let ix_off = ix00_bindextype_offset(&bytes);
    assert_eq!(bytes[ix_off], 0x01, "ix00 bIndexType = AVI_INDEX_OF_CHUNKS");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_index_type(0),
        Some(AviIndexType::OfIndexes),
        "stream 0 super-index declares AVI_INDEX_OF_INDEXES"
    );
    // Out-of-range stream reads None.
    assert_eq!(typed_dmx.super_index_index_type(99), None);

    let ix_types = typed_dmx.std_index_index_types(0);
    assert!(
        !ix_types.is_empty(),
        "OpenDML file has at least one ix00 segment"
    );
    assert!(
        ix_types.iter().all(|t| *t == AviIndexType::OfChunks),
        "every ix00 segment declares AVI_INDEX_OF_CHUNKS, got {ix_types:?}"
    );
    assert!(
        typed_dmx.std_index_index_types(99).is_empty(),
        "out-of-range stream has no ix## segments"
    );

    // The divergence-only metadata key is suppressed for the canonical
    // super-index type.
    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.index_type").is_none(),
        "a canonical AVI_INDEX_OF_INDEXES super-index must not emit the index_type key"
    );
}

/// A super-index whose `bIndexType` is mislabelled `AVI_INDEX_OF_CHUNKS`
/// (`0x01`) instead of `AVI_INDEX_OF_INDEXES`: `parse_indx` folds it to
/// an entry-less slot (so seek treats it as absent), but the accessor
/// still reports the declared (wrong) type and the metadata key fires.
#[test]
fn divergent_super_index_type_surfaces_via_metadata() {
    let mut bytes = write_opendml_avi("divergent");
    let reg = registry_with_magicyuv();

    let off = indx_bindextype_offset(&bytes);
    assert_eq!(
        bytes[off], 0x00,
        "precondition: canonical AVI_INDEX_OF_INDEXES"
    );
    // Mislabel the super-index as an index-of-chunks.
    bytes[off] = 0x01;

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_index_type(0),
        Some(AviIndexType::OfChunks),
        "the demuxer surfaces the mislabelled super-index bIndexType verbatim"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:indx.0.index_type").as_deref(),
        Some("of_chunks"),
        "a mislabelled super-index bIndexType must emit the index_type key with its label"
    );
}

/// An `AVI_INDEX_IS_DATA` (`0x80`) super-index byte decodes to the typed
/// `IsData` variant and emits the `is_data` label.
#[test]
fn is_data_super_index_type_surfaces() {
    let mut bytes = write_opendml_avi("isdata");
    let reg = registry_with_magicyuv();

    let off = indx_bindextype_offset(&bytes);
    bytes[off] = 0x80;

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_index_type(0),
        Some(AviIndexType::IsData),
        "AVI_INDEX_IS_DATA decodes to the typed IsData variant"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:indx.0.index_type").as_deref(),
        Some("is_data"),
        "AVI_INDEX_IS_DATA must emit the is_data label"
    );
}

/// An unrecognised `bIndexType` byte decodes to `Other(byte)` verbatim
/// but suppresses the metadata key (no label for an unknown code), so the
/// raw value stays reachable only through the typed accessor.
#[test]
fn other_super_index_type_has_no_label_key() {
    let mut bytes = write_opendml_avi("other");
    let reg = registry_with_magicyuv();

    let off = indx_bindextype_offset(&bytes);
    bytes[off] = 0x42;

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_index_type(0),
        Some(AviIndexType::Other(0x42)),
        "an unrecognised bIndexType is preserved verbatim as Other"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.index_type").is_none(),
        "an unrecognised bIndexType has no label, so the metadata key is suppressed"
    );
}

/// An AVI-1.0 file has no super-index and no `ix##`: the accessors
/// distinguish "no index declared" by returning `None` / an empty `Vec`,
/// and the metadata key never emits.
#[test]
fn avi10_file_has_no_index_type() {
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(4_444, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r361-avi10-no-index-type.avi");
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
        typed_dmx.super_index_index_type(0),
        None,
        "AVI 1.0 has no super-index → accessor must return None"
    );
    assert!(
        typed_dmx.std_index_index_types(0).is_empty(),
        "AVI 1.0 has no ix## → accessor must return an empty Vec"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.index_type").is_none(),
        "AVI 1.0 must not emit the index_type metadata key"
    );
}

/// `AviIndexType` round-trips raw bytes through `from_raw` / `to_raw` and
/// the label table matches the three documented codes.
#[test]
fn index_type_raw_roundtrip_and_labels() {
    for (raw, expect, label) in [
        (0x00u8, AviIndexType::OfIndexes, Some("of_indexes")),
        (0x01u8, AviIndexType::OfChunks, Some("of_chunks")),
        (0x80u8, AviIndexType::IsData, Some("is_data")),
        (0x42u8, AviIndexType::Other(0x42), None),
    ] {
        let decoded = AviIndexType::from_raw(raw);
        assert_eq!(decoded, expect, "from_raw({raw:#x}) decode");
        assert_eq!(decoded.to_raw(), raw, "to_raw round-trips {raw:#x}");
        assert_eq!(decoded.label(), label, "label for {raw:#x}");
    }
}
