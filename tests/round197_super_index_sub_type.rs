//! Round-197: surface the `indx` AVISUPERINDEX `bIndexSubType` byte
//! on the demuxer's public API + `avi:indx.<n>.sub_type_2field`
//! metadata key.
//!
//! Per the AVISUPERINDEX layout in
//! `docs/container/riff/avi-riff-file-reference.md` Appendix F (the
//! `bIndexSubType` row): *"The index subtype. The value must be zero
//! or AVI_INDEX_SUB_2FIELD."* And per Appendix E §"Sub-types",
//! `AVI_INDEX_SUB_2FIELD == 0x01`. The super-index inherits the
//! sub-type of the pointed-to per-segment `ix##` standard indexes —
//! so an OpenDML reader that sees `bIndexSubType == 0x01` on the
//! super-index knows every pointed-to segment's `ix##` will carry
//! 12-byte 2-field entries.
//!
//! The muxer (round-4 P1) already stamps `AVI_INDEX_SUB_2FIELD` on
//! the super-index of any stream registered via
//! `AviMuxOptions::with_field2_stream(stream_index)`. The demuxer
//! parsed the byte but never surfaced it: callers had to wait for
//! the in-`movi` `ix##` scan to fire the existing
//! `avi:ix.<n>.is_2field` hint, which means a `strl`-level reader
//! couldn't detect interlaced carriage from the super-index alone.
//!
//! Round-197 closes that gap:
//!
//! - `AviDemuxer::super_index_sub_type(stream) -> Option<u8>`
//!   returns the raw byte verbatim, `None` for streams without an
//!   `indx`.
//! - `AviDemuxer::super_index_is_2field(stream) -> bool` folds the
//!   raw byte into a boolean for the common "is this interlaced?"
//!   question.
//! - `avi:indx.<n>.sub_type_2field = "true"` metadata key emits
//!   only when the super-index sub-type byte is `AVI_INDEX_SUB_2FIELD`
//!   (the `0` default is omitted so absence stays observable, per
//!   the round-176/153/119/115/107 convention).

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open as demuxer_open;
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

/// Mux + demux a 2-field OpenDML file. The muxer stamps
/// `bIndexSubType = AVI_INDEX_SUB_2FIELD` on stream 0's super-index;
/// the demuxer surfaces it via the new accessors + metadata key.
#[test]
fn two_field_super_index_sub_type_round_trips() {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 9_000, 128)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r197-2field-superidx-subtype.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_field2_stream(0);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            mux.set_field2_offset(64);
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open(rs, &reg).unwrap();

    // The native typed accessors expose the raw byte + a boolean
    // convenience. The raw byte must be `0x01` per OpenDML 2.0
    // Appendix E `AVI_INDEX_SUB_2FIELD`.
    //
    // Since `AviDemuxer::super_index_sub_type` is on the concrete
    // type, downcast the `Box<dyn Demuxer>` to it. The `demuxer_open`
    // entry returns the trait object so we go via the typed entry
    // `open_avi` for the round-trip half.
    drop(dmx);
    let rs2: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs2, &reg).unwrap();
    assert_eq!(
        typed_dmx.super_index_sub_type(0),
        Some(0x01),
        "AVI_INDEX_SUB_2FIELD must be surfaced on stream 0's super-index"
    );
    assert!(
        typed_dmx.super_index_is_2field(0),
        "super_index_is_2field convenience must report true for the 2-field stream"
    );

    // Streams that don't exist must read as `None` / `false`.
    assert_eq!(typed_dmx.super_index_sub_type(99), None);
    assert!(!typed_dmx.super_index_is_2field(99));

    // The `avi:indx.<n>.sub_type_2field = "true"` metadata key emits
    // only for the 2-field stream.
    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:indx.0.sub_type_2field").as_deref(),
        Some("true"),
        "the metadata key must surface on the 2-field stream"
    );
}

/// A non-2-field OpenDML file (no `with_field2_stream` call) leaves
/// the super-index sub-type byte at the `0` default. The accessor
/// returns `Some(0)` (the super-index *exists* and explicitly carries
/// 0); the boolean convenience reports `false`; and the metadata key
/// is suppressed entirely so absence of the key stays observable —
/// the round-176/153/119/115/107 "default == absent" convention.
#[test]
fn default_subtype_super_index_emits_no_metadata_key() {
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(7_777, 128);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r197-default-subtype.avi");
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
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_sub_type(0),
        Some(0x00),
        "non-2-field super-index must explicitly carry sub-type 0"
    );
    assert!(
        !typed_dmx.super_index_is_2field(0),
        "super_index_is_2field must report false on a default-subtype stream"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.sub_type_2field").is_none(),
        "the metadata key must be omitted on a default-subtype super-index"
    );
}

/// An AVI-1.0 file has no super-index at all (the `indx` chunk is
/// `AviKind::OpenDml`-only — see `oxideav_avi::muxer` §"OpenDML 2.0
/// super-index"). The accessor distinguishes "no super-index
/// declared" from "super-index sub-type 0" by returning `None`
/// rather than `Some(0)`; the boolean convenience reports `false`;
/// and the metadata key never emits.
#[test]
fn avi10_file_has_no_super_index_sub_type() {
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(3_333, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r197-avi10-no-superidx.avi");
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
        typed_dmx.super_index_sub_type(0),
        None,
        "AVI 1.0 has no super-index → accessor must return None"
    );
    assert!(
        !typed_dmx.super_index_is_2field(0),
        "AVI 1.0 has no super-index → convenience must report false"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(
        get("avi:indx.0.sub_type_2field").is_none(),
        "AVI 1.0 must not emit the sub_type_2field metadata key"
    );
}
