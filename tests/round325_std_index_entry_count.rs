//! Round-325: surface the per-segment `ix##` AVISTDINDEX `nEntriesInUse`
//! declared entry count on the demuxer's public API + a truncation
//! cross-check validator and a violation-only
//! `avi:ix.<n>.<seg>.declared_entries` metadata key.
//!
//! Per the AVISTDINDEX layout in
//! `docs/container/riff/avi-riff-file-reference.md` Appendix G and the
//! base AVIMETAINDEX in Appendix E (the `nEntriesInUse` row: *"Number of
//! valid entries in adwIndex."*), the standard-index chunk declares how
//! many `AVISTDINDEX_ENTRY` records its body holds. A well-formed chunk
//! body carries exactly that many entries; a truncated capture
//! crash-dump or hand-edited file can stamp a larger count than the body
//! physically contains. The demuxer now parses the entries it can read
//! (rather than discarding the whole chunk, the previous behaviour) and
//! retains the declared count verbatim. Round-325 closes the gap:
//!
//! - `AviDemuxer::std_index_declared_entry_counts(stream) -> Vec<u32>`
//!   returns the verbatim `nEntriesInUse` per `ix##` chunk for the
//!   stream, in file order (one per segment); empty for AVI-1.0 /
//!   no-`ix##` files.
//! - `AviDemuxer::std_index_entry_count_violations()` returns one
//!   `StdIndexEntryCountViolation` per `ix##` whose declared count
//!   exceeds the parsed count — informational, never fails `open()`.
//! - `avi:ix.<n>.<seg>.declared_entries = "<declared>/<parsed>"` metadata
//!   key fires only on a truncation; the well-formed case emits no key so
//!   absence stays observable, per the "default == absent" convention.

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
/// final bytes.
fn write_opendml_avi(tag: &str) -> Vec<u8> {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 7_000, 128)).collect();

    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r325-stdidx-nentries-{tag}.avi"));
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
/// offset of its `nEntriesInUse` field. The AVISTDINDEX layout (Appendix
/// G) is: fcc(4) cb(4) | wLongsPerEntry(2) bIndexSubType(1) bIndexType(1)
/// nEntriesInUse(4) dwChunkId(4) qwBaseOffset(8) ... — so the
/// `nEntriesInUse` DWORD begins at the `ix00` FourCC offset + 12.
fn ix00_nentries_offset(bytes: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ix00" {
            return i + 12;
        }
        i += 1;
    }
    panic!("no ix00 standard-index chunk found in OpenDML file");
}

/// A well-formed OpenDML file written by this crate's muxer carries an
/// `ix##` whose `nEntriesInUse` matches the number of entries physically
/// present. The accessor surfaces the verbatim count; the violation
/// validator and metadata key stay empty.
#[test]
fn opendml_std_index_declared_entry_count_round_trips() {
    let bytes = write_opendml_avi("canonical");
    let reg = registry_with_magicyuv();

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.clone()));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    let counts = typed_dmx.std_index_declared_entry_counts(0);
    assert_eq!(
        counts.len(),
        1,
        "single-segment OpenDML file has exactly one ix00 for stream 0"
    );
    // Three frames were written, so the standard index declares 3 entries.
    assert_eq!(counts[0], 3, "ix00 declares one entry per indexed chunk");

    // The verbatim accessor matches the bytes physically stamped at the
    // ix00 chunk's nEntriesInUse field.
    let off = ix00_nentries_offset(&bytes);
    let stamped = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    assert_eq!(
        counts[0], stamped,
        "accessor returns the verbatim on-disk nEntriesInUse"
    );

    // Out-of-range stream → empty Vec.
    assert!(typed_dmx.std_index_declared_entry_counts(99).is_empty());

    // A canonical (declared == parsed) chunk raises no violation.
    assert!(
        typed_dmx.std_index_entry_count_violations().is_empty(),
        "a well-formed ix## must not raise a truncation violation"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:ix.0.0.declared_entries").is_none(),
        "a well-formed ix## must not emit the declared_entries key"
    );
}

/// An `ix##` whose `nEntriesInUse` is corrupted to claim more entries
/// than the chunk body physically holds: the demuxer parses the entries
/// it can read (rather than discarding the chunk), the declared-count
/// accessor surfaces the (now inflated) declared value verbatim, and both
/// the typed violation validator and the metadata key fire so a repair
/// tool can detect the loss.
#[test]
fn std_index_truncated_entry_table_surfaces_violation() {
    let mut bytes = write_opendml_avi("truncated");
    let reg = registry_with_magicyuv();

    let off = ix00_nentries_offset(&bytes);
    let actual = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    // Inflate nEntriesInUse without growing the chunk body, so the body
    // now short-reads relative to the declared count.
    let inflated = actual + 10;
    bytes[off..off + 4].copy_from_slice(&inflated.to_le_bytes());

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    // The declared-count accessor surfaces the inflated value verbatim.
    let counts = typed_dmx.std_index_declared_entry_counts(0);
    assert_eq!(
        counts,
        vec![inflated],
        "accessor surfaces the inflated nEntriesInUse verbatim"
    );

    let viols = typed_dmx.std_index_entry_count_violations();
    assert_eq!(viols.len(), 1, "one truncated ix## raises one violation");
    assert_eq!(viols[0].stream_index, 0);
    assert_eq!(viols[0].segment_index, 0);
    assert_eq!(viols[0].declared_entries, inflated);
    assert_eq!(
        viols[0].parsed_entries, actual,
        "the parsed count is the number of entries the body physically held"
    );
    assert!(
        viols[0].parsed_entries < viols[0].declared_entries,
        "a reported violation always has parsed < declared"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:ix.0.0.declared_entries").as_deref(),
        Some(format!("{inflated}/{actual}").as_str()),
        "a truncated ix## must emit the declared_entries key as <declared>/<parsed>"
    );
}

/// An AVI-1.0 file carries no `ix##` standard index at all. The accessor
/// returns an empty Vec, the violation list is empty, and no metadata key
/// emits.
#[test]
fn avi10_file_has_no_std_index_declared_entry_counts() {
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(4_444, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r325-avi10-no-stdidx-nentries.avi");
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
        typed_dmx.std_index_declared_entry_counts(0).is_empty(),
        "AVI 1.0 has no ix## → accessor must return an empty Vec"
    );
    assert!(
        typed_dmx.std_index_entry_count_violations().is_empty(),
        "AVI 1.0 has no ix## → no violations"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:ix.0.0.declared_entries").is_none(),
        "AVI 1.0 must not emit the declared_entries key"
    );
}
