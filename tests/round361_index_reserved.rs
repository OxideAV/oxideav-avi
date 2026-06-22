//! Round-361: surface a non-conformant non-zero reserved field on the
//! OpenDML 2.0 `AVIMETAINDEX` headers.
//!
//! Per the AVISUPERINDEX / AVISTDINDEX layouts (clean-room source:
//! `docs/container/riff/opendml-avi-2.0.pdf` §"Index Structures"):
//!
//! - `AVISUPERINDEX.dwReserved[3]` — 3 DWORDs after `dwChunkId`,
//!   *"meaning differs for each index type/subtype. 0 if unused"*. For
//!   the AVI 2.0 super-index of indexes this crate reads/writes it is
//!   reserved and a conforming writer leaves it all-zero.
//! - `AVISTDINDEX.dwReserved3` — a single DWORD after `qwBaseOffset`,
//!   *"must be 0"*.
//!
//! The demuxer skipped both. Round-361:
//!
//! - `AviDemuxer::super_index_reserved(stream) -> Option<[u32; 3]>`
//!   returns the array only when non-zero, plus the
//!   `avi:indx.<n>.reserved` metadata key (comma-joined `0x`-hex) on the
//!   non-conformant case — mirroring the round-330 `avih_reserved`
//!   shape.
//! - `AviDemuxer::std_index_reserved(stream) -> Vec<(usize, u32)>`
//!   returns `(segment, dwReserved3)` for each `ix##` with a non-zero
//!   reserved DWORD, in file order.

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

fn write_opendml_avi(tag: &str) -> Vec<u8> {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 9_000, 128)).collect();

    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r361-index-reserved-{tag}.avi"));
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

/// File offset of the first `indx` super-index body (just past the
/// 8-byte chunk header).
fn indx_body_offset(bytes: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"indx" {
            return i + 8;
        }
        i += 1;
    }
    panic!("no indx super-index chunk found");
}

/// File offset of the first `ix00` standard-index body.
fn ix00_body_offset(bytes: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ix00" {
            return i + 8;
        }
        i += 1;
    }
    panic!("no ix00 standard-index chunk found");
}

/// A well-formed OpenDML file leaves both reserved fields zero, so the
/// accessors read as `None` / empty and no reserved metadata key emits.
#[test]
fn conforming_reserved_fields_read_absent() {
    let bytes = write_opendml_avi("conforming");
    let reg = registry_with_magicyuv();

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_reserved(0),
        None,
        "conforming super-index dwReserved[3] is all-zero → None"
    );
    assert!(
        typed_dmx.std_index_reserved(0).is_empty(),
        "conforming ix## dwReserved3 is 0 → empty Vec"
    );
    // Out-of-range reads are absent too.
    assert_eq!(typed_dmx.super_index_reserved(99), None);
    assert!(typed_dmx.std_index_reserved(99).is_empty());

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert!(get("avi:indx.0.reserved").is_none());
}

/// A non-zero `dwReserved[3]` on the super-index surfaces verbatim via
/// the accessor and the comma-joined `0x`-hex metadata key.
#[test]
fn nonzero_super_index_reserved_surfaces() {
    let mut bytes = write_opendml_avi("super");
    let reg = registry_with_magicyuv();

    // dwReserved[3] is at super-index body offset 12 (3 DWORDs).
    let base = indx_body_offset(&bytes) + 12;
    assert_eq!(
        &bytes[base..base + 12],
        &[0u8; 12],
        "precondition: reserved zero"
    );
    bytes[base + 4..base + 8].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.super_index_reserved(0),
        Some([0, 0xDEAD_BEEF, 0]),
        "the demuxer surfaces the non-zero dwReserved[3] verbatim"
    );

    let md = typed_dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("avi:indx.0.reserved").as_deref(),
        Some("0x00000000,0xDEADBEEF,0x00000000"),
        "the non-conformant reserved array emits the comma-joined hex key"
    );
}

/// A non-zero `dwReserved3` on an `ix##` surfaces as a
/// `(segment, value)` pair via the accessor.
#[test]
fn nonzero_std_index_reserved_surfaces() {
    let mut bytes = write_opendml_avi("std");
    let reg = registry_with_magicyuv();

    // dwReserved3 is at std-index body offset 20 (single DWORD).
    let base = ix00_body_offset(&bytes) + 20;
    assert_eq!(
        &bytes[base..base + 4],
        &[0u8; 4],
        "precondition: reserved zero"
    );
    bytes[base..base + 4].copy_from_slice(&0x0000_C0DEu32.to_le_bytes());

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let typed_dmx = oxideav_avi::demuxer::open_avi(rs, &reg).unwrap();

    assert_eq!(
        typed_dmx.std_index_reserved(0),
        vec![(0usize, 0x0000_C0DEu32)],
        "the first ix00 segment's non-zero dwReserved3 surfaces as (0, value)"
    );
}
