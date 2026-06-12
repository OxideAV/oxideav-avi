//! Round-285: idx1 `rec ` LIST entries (`AVIIF_LIST`).
//!
//! Per AVI 1.0 §"AVI Index Entries", the idx1 chunk "consists of an
//! AVIOLDINDEX structure with entries for each data chunk, including
//! 'rec ' chunks", and per Appendix C the `AVIIF_LIST` flag marks an
//! entry whose chunk "is a 'rec ' list."
//!
//! Covers:
//! - muxer: one idx1 entry per `LIST rec ` cluster (ckid `rec `,
//!   `AVIIF_LIST`, offset at the cluster's `LIST` header, size = the
//!   LIST size-field value), in file order ahead of the grouped
//!   packets' entries; primary segment only.
//! - demuxer: typed `idx1_rec_list_entries()` / `idx1_rec_list_count()`
//!   accessors + `avi:idx1.rec_lists` metadata key; `rec ` entries stay
//!   out of the per-stream seek/flags surfaces.
//! - offset-base probe: a leading `rec ` entry (whose offset points at
//!   a `LIST` FourCC, not its recorded ckid) no longer anchors the
//!   movi-relative vs file-absolute detection.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{open_avi, Idx1Flags, AVIIF_LIST};
use oxideav_avi::muxer::{open_with_options, AviKind, AviMuxOptions, RiffSegmentLimit};

/// Synthetic registry entry for the FOURCC ↔ codec_id mapping the
/// tests below need. Avoids a producer-crate dev-dep — real MagicYUV
/// decode coverage lives in `crates/oxideav-tests`.
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

/// Mux `frames` as keyframes of one MagicYUV video stream with the
/// given options/kind into `path`.
fn mux_frames(
    path: &std::path::Path,
    stream: &StreamInfo,
    frames: &[Vec<u8>],
    kind: AviKind,
    opts: AviMuxOptions,
) {
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_with_options(ws, std::slice::from_ref(stream), kind, opts).unwrap();
    mux.write_header().unwrap();
    for (i, payload) in frames.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
}

/// Count on-disk `LIST` headers whose form type is `rec `.
fn count_rec_lists(bytes: &[u8]) -> usize {
    (0..bytes.len().saturating_sub(12))
        .filter(|&i| &bytes[i..i + 4] == b"LIST" && &bytes[i + 8..i + 12] == b"rec ")
        .count()
}

#[test]
fn rec_clustered_mux_indexes_rec_lists_in_idx1() {
    // 8 frames at cap=3 → 3 clusters (3 + 3 + 2 packets).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..8).map(|i| synthesize_payload(i + 2850, 128)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r285-rec-idx1.avi");
    mux_frames(
        &tmp,
        &stream,
        &frames,
        AviKind::Avi10,
        AviMuxOptions::new().with_rec_cluster_packets(3),
    );
    let bytes = std::fs::read(&tmp).unwrap();
    let on_disk_clusters = count_rec_lists(&bytes);
    assert_eq!(on_disk_clusters, 3, "expected 3 LIST rec clusters");

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    // Typed accessor: one idx1 rec entry per on-disk cluster.
    assert_eq!(dmx.idx1_rec_list_count(), 3);
    let entries: Vec<_> = dmx.idx1_rec_list_entries().to_vec();
    assert_eq!(entries.len(), 3);

    // Metadata key mirrors the count.
    let meta = dmx.metadata().to_vec();
    let rec_meta = meta
        .iter()
        .find(|(k, _)| k == "avi:idx1.rec_lists")
        .map(|(_, v)| v.clone());
    assert_eq!(rec_meta.as_deref(), Some("3"));

    // Each entry: AVIIF_LIST set (typed decode agrees), file-absolute
    // offset lands on the cluster's `LIST` header with a `rec ` form
    // type, and the recorded size equals the LIST size-field value.
    let mut prev_off = 0u64;
    for (k, e) in entries.iter().enumerate() {
        assert_ne!(
            e.flags & AVIIF_LIST,
            0,
            "rec entry {k} must carry AVIIF_LIST"
        );
        assert!(Idx1Flags::from_bits(e.flags).is_list);
        assert!(e.offset > prev_off, "rec entries must be in file order");
        prev_off = e.offset;
        let o = e.offset as usize;
        assert_eq!(&bytes[o..o + 4], b"LIST", "rec entry {k} offset");
        assert_eq!(&bytes[o + 8..o + 12], b"rec ", "rec entry {k} form type");
        let list_size =
            u32::from_le_bytes([bytes[o + 4], bytes[o + 5], bytes[o + 6], bytes[o + 7]]);
        assert_eq!(e.size, list_size, "rec entry {k} size vs LIST size field");
        // 3 + 3 + 2 grouped chunks of 8-byte header + 128-byte body,
        // plus the 4-byte `rec ` form type.
        let expected = 4 + (8 + 128) * if k < 2 { 3 } else { 2 };
        assert_eq!(e.size, expected as u32);
    }

    // The per-stream surfaces stay rec-free: every per-packet idx1
    // flags slot is the keyframe stamp, and the packet walk still
    // round-trips all 8 frames byte-equal.
    for seq in 0..frames.len() {
        let f = dmx.idx1_typed_flags_for_packet(0, seq).unwrap();
        assert!(f.is_keyframe && !f.is_list, "packet {seq} flags");
    }
    assert!(dmx.idx1_typed_flags_for_packet(0, frames.len()).is_none());
    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got, frames);
}

#[test]
fn unclustered_mux_has_no_rec_entries() {
    let stream = magicyuv_stream(32, 32);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synthesize_payload(i + 2860, 64)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r285-no-rec.avi");
    mux_frames(&tmp, &stream, &frames, AviKind::Avi10, AviMuxOptions::new());

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.idx1_rec_list_count(), 0);
    assert!(dmx.idx1_rec_list_entries().is_empty());
    assert!(
        !dmx.metadata()
            .iter()
            .any(|(k, _)| k == "avi:idx1.rec_lists"),
        "metadata key must be omitted when no rec lists are indexed"
    );
}

#[test]
fn opendml_primary_segment_indexes_rec_lists() {
    // Single-segment OpenDML file: the primary RIFF's idx1 carries the
    // rec entries; the running per-packet IndexEntry path (not the
    // idx1-from-ix synthesis) is in effect by default.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..6).map(|i| synthesize_payload(i + 2870, 96)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r285-odml-rec.avi");
    mux_frames(
        &tmp,
        &stream,
        &frames,
        AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        AviMuxOptions::new().with_rec_cluster_packets(2),
    );
    let bytes = std::fs::read(&tmp).unwrap();
    assert_eq!(count_rec_lists(&bytes), 3);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.idx1_rec_list_count(), 3);
    for e in dmx.idx1_rec_list_entries() {
        assert!(Idx1Flags::from_bits(e.flags).is_list);
        let o = e.offset as usize;
        assert_eq!(&bytes[o..o + 4], b"LIST");
        assert_eq!(&bytes[o + 8..o + 12], b"rec ");
    }
}

#[test]
fn idx1_from_ix_synthesis_carries_no_rec_entries() {
    // The ix##-rebuilt idx1 indexes per-packet chunks only — the
    // synthesis path documents that rec LIST entries are not
    // reproduced there.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..6).map(|i| synthesize_payload(i + 2880, 96)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r285-synth-rec.avi");
    mux_frames(
        &tmp,
        &stream,
        &frames,
        AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        AviMuxOptions::new()
            .with_rec_cluster_packets(2)
            .synthesise_idx1_from_ix(true),
    );
    let bytes = std::fs::read(&tmp).unwrap();
    assert_eq!(count_rec_lists(&bytes), 3, "clusters still on disk");

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.idx1_rec_list_count(),
        0,
        "ix##-synthesised idx1 indexes packets only"
    );
    // Packets still round-trip.
    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got, frames);
}

#[test]
fn leading_rec_entry_does_not_anchor_offset_base_probe() {
    // Rewrite a rec-clustered file's idx1 offsets from movi-relative to
    // file-absolute. The FIRST idx1 entry is the `rec ` LIST entry —
    // the bytes at its target are `LIST`, not `rec `, so it can never
    // satisfy the offset-base probe; the probe must skip to the first
    // per-stream entry, detect the file-absolute convention from it,
    // and resolve every entry (rec entries included) correctly.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..5).map(|i| synthesize_payload(i + 2890, 80)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r285-abs-rec.avi");
    mux_frames(
        &tmp,
        &stream,
        &frames,
        AviKind::Avi10,
        AviMuxOptions::new().with_rec_cluster_packets(3),
    );
    let mut bytes = std::fs::read(&tmp).unwrap();

    // Locate the `movi` FourCC (the muxer's idx1 offset base) and the
    // idx1 chunk. Both FourCCs appear exactly once in this fixture's
    // structural region; search from the back for idx1 (it trails the
    // movi list).
    let movi_pos = (0..bytes.len() - 4)
        .find(|&i| &bytes[i..i + 4] == b"movi")
        .expect("movi fourcc");
    let idx1_pos = (0..bytes.len() - 4)
        .rfind(|&i| &bytes[i..i + 4] == b"idx1")
        .expect("idx1 chunk");
    let idx1_size = u32::from_le_bytes([
        bytes[idx1_pos + 4],
        bytes[idx1_pos + 5],
        bytes[idx1_pos + 6],
        bytes[idx1_pos + 7],
    ]) as usize;
    assert_eq!(idx1_size % 16, 0);
    // 5 packet entries + 2 rec entries.
    assert_eq!(idx1_size / 16, 7);
    let body_start = idx1_pos + 8;
    // First entry must be the rec LIST entry (clusters open before the
    // packets they group).
    assert_eq!(&bytes[body_start..body_start + 4], b"rec ");

    // Convert every entry offset to file-absolute.
    for k in 0..idx1_size / 16 {
        let off_pos = body_start + k * 16 + 8;
        let rel = u32::from_le_bytes([
            bytes[off_pos],
            bytes[off_pos + 1],
            bytes[off_pos + 2],
            bytes[off_pos + 3],
        ]);
        let abs = rel + movi_pos as u32;
        bytes[off_pos..off_pos + 4].copy_from_slice(&abs.to_le_bytes());
    }
    let tmp_abs = std::env::temp_dir().join("oxideav-avi-r285-abs-rec-rewritten.avi");
    std::fs::write(&tmp_abs, &bytes).unwrap();

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp_abs).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    // The rec entries resolve to on-disk `LIST` + `rec ` headers, which
    // is only possible if the probe detected the file-absolute base
    // from a per-stream entry rather than defaulting on the leading
    // rec entry.
    assert_eq!(dmx.idx1_rec_list_count(), 2);
    for e in dmx.idx1_rec_list_entries() {
        let o = e.offset as usize;
        assert_eq!(&bytes[o..o + 4], b"LIST");
        assert_eq!(&bytes[o + 8..o + 12], b"rec ");
    }

    // And the idx1-driven keyframe seek still lands on frame 0 data.
    dmx.seek_to(0, 0).unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frames[0]);
}
