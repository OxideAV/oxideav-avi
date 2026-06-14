//! Round-304: surface the `indx` AVISUPERINDEX `wLongsPerEntry` WORD
//! on the demuxer's public API + `avi:indx.<n>.longs_per_entry`
//! metadata key.
//!
//! Per the AVISUPERINDEX layout in
//! `docs/container/riff/avi-riff-file-reference.md` Appendix F (the
//! `wLongsPerEntry` row: *"4 (each entry is 16 bytes)."*) and the
//! base AVIMETAINDEX in Appendix E (`wLongsPerEntry` row: *"Size of
//! each index entry, in 4-byte units."*), this WORD declares the
//! per-entry stride of the super-index's `aIndex[]` table in units of
//! 4-byte DWORDs. For a well-formed AVI 2.0 super-index it is always
//! `4` — each `_avisuperindex_entry` is `(qwOffset:8, dwSize:4,
//! dwDuration:4)` = 16 bytes = 4 longs.
//!
//! The demuxer parsed the WORD (it drives the 16-byte-stride entry
//! walk in `parse_indx`) but never surfaced it. Round-304 closes that
//! gap:
//!
//! - `AviDemuxer::super_index_longs_per_entry(stream) -> Option<u16>`
//!   returns the raw WORD verbatim, `None` for streams without an
//!   `indx`.
//! - `avi:indx.<n>.longs_per_entry` metadata key emits only when the
//!   stride differs from the spec-default `4` (the `4` default is
//!   omitted so absence stays observable, per the
//!   round-197/176/153/119/115/107 "default == absent" convention).
//!   The typed accessor returns the raw value either way.

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

/// An OpenDML file written by this crate's muxer always stamps the
/// spec-default `wLongsPerEntry = 4` (16-byte super-index entries).
/// The accessor surfaces `Some(4)`; the metadata key is suppressed
/// because `4` is the spec default (default == absent).
#[test]
fn opendml_super_index_longs_per_entry_round_trips() {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 5_000, 128)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r304-superidx-longs.avi");
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
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_longs_per_entry(0),
        Some(4),
        "a well-formed AVI 2.0 super-index declares wLongsPerEntry = 4"
    );

    // Out-of-range streams read as None.
    assert_eq!(typed_dmx.super_index_longs_per_entry(99), None);

    // The metadata key is suppressed for the spec-default `4`.
    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.longs_per_entry").is_none(),
        "the spec-default wLongsPerEntry = 4 must not emit a metadata key"
    );
}

/// An AVI-1.0 file has no super-index at all (the `indx` chunk is
/// `AviKind::OpenDml`-only). The accessor distinguishes "no
/// super-index declared" from "super-index stride 4" by returning
/// `None` rather than `Some(4)`; the metadata key never emits.
#[test]
fn avi10_file_has_no_super_index_longs_per_entry() {
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(3_333, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r304-avi10-no-superidx-longs.avi");
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
        typed_dmx.super_index_longs_per_entry(0),
        None,
        "AVI 1.0 has no super-index → accessor must return None"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.longs_per_entry").is_none(),
        "AVI 1.0 must not emit the longs_per_entry metadata key"
    );
}
