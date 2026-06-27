//! Round-377: top-level `CSET` (character-set) chunk read + write
//! symmetry.
//!
//! `CSET` is a generic RIFF chunk that overrides the character set,
//! language, country and dialect of the text chunks in the enclosing
//! form. Its body is a fixed 8-byte record of four little-endian 16-bit
//! fields (`docs/container/riff/metadata/exiftool-riff-tags.html` "RIFF
//! CSET Tags": index 0 → `CodePage`, 1 → `CountryCode`, 2 →
//! `LanguageCode`, 3 → `Dialect`). All-zero is the documented
//! "use the current locale" default.
//!
//! Covers:
//! - muxer: `AviMuxOptions::with_cset(cset)` /
//!   `with_cset_fields(cp, cc, lc, d)` append one `CSET` chunk per call
//!   as a sibling of `LIST hdrl`, just before `movi`.
//! - demuxer: `cset_chunks()` / `cset_chunk_count()` + `avi:cset.count`
//!   metadata key; the four typed fields round-trip; the on-disk body
//!   matches `CsetChunk::to_bytes`; absence omits the key.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{open_avi, CsetChunk};
use oxideav_avi::muxer::{open_with_options, AviKind, AviMuxOptions};

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

fn synth(seed: u32, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..n {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

fn mux_frames(
    path: &std::path::Path,
    stream: &StreamInfo,
    frames: &[Vec<u8>],
    opts: AviMuxOptions,
) {
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux =
        open_with_options(ws, std::slice::from_ref(stream), AviKind::Avi10, opts).unwrap();
    mux.write_header().unwrap();
    for (i, payload) in frames.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
}

fn demux_all(dmx: &mut dyn Demuxer) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    got
}

#[test]
fn cset_fields_roundtrip() {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth(i + 3770, 96)).collect();
    let reg = registry_with_magicyuv();

    // Two CSET overrides authored two different ways (typed struct +
    // raw fields). code_page 1252 (Windows-1252), country 1 (US-style),
    // language 0x0409 (en-US), dialect 1; and an all-zero "current
    // locale" default.
    let c0 = CsetChunk {
        offset: 0,
        code_page: 1252,
        country_code: 1,
        language_code: 0x0409,
        dialect: 1,
    };

    let opts = AviMuxOptions::new()
        .with_cset(c0)
        .with_cset_fields(0, 0, 0, 0);

    let tmp = std::env::temp_dir().join("oxideav-avi-r377-cset.avi");
    mux_frames(&tmp, &stream, &frames, opts);
    let bytes = std::fs::read(&tmp).unwrap();

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.cset_chunk_count(), 2);
    let cset = dmx.cset_chunks().to_vec();
    assert_eq!(cset.len(), 2);

    // Typed fields round-trip (ignoring offset, which is read-side
    // layout metadata).
    assert_eq!(cset[0].code_page, 1252);
    assert_eq!(cset[0].country_code, 1);
    assert_eq!(cset[0].language_code, 0x0409);
    assert_eq!(cset[0].dialect, 1);

    assert_eq!(cset[1].code_page, 0);
    assert_eq!(cset[1].country_code, 0);
    assert_eq!(cset[1].language_code, 0);
    assert_eq!(cset[1].dialect, 0);

    // Each recorded offset points at a `CSET` FourCC with an 8-byte
    // declared size and a body byte-equal to `to_bytes`.
    for (k, c) in cset.iter().enumerate() {
        let o = c.offset as usize;
        assert_eq!(&bytes[o..o + 4], b"CSET", "cset {k} offset");
        let size_field =
            u32::from_le_bytes([bytes[o + 4], bytes[o + 5], bytes[o + 6], bytes[o + 7]]);
        assert_eq!(size_field, 8, "cset {k} on-disk size field");
        assert_eq!(&bytes[o + 8..o + 16], &c.to_bytes(), "cset {k} body bytes");
    }

    // Metadata key mirrors the count.
    let meta = dmx.metadata().to_vec();
    let count = meta
        .iter()
        .find(|(k, _)| k == "avi:cset.count")
        .map(|(_, v)| v.clone());
    assert_eq!(count.as_deref(), Some("2"));

    // CSET stays out of the packet stream; frames round-trip.
    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}

#[test]
fn cset_to_bytes_is_little_endian_field_order() {
    // Independent check of the on-wire layout: the four fields are
    // emitted in code_page / country / language / dialect order, each
    // 16-bit little-endian.
    let c = CsetChunk {
        offset: 999, // ignored on serialise
        code_page: 0x1234,
        country_code: 0x5678,
        language_code: 0x9ABC,
        dialect: 0xDEF0,
    };
    assert_eq!(
        c.to_bytes(),
        [0x34, 0x12, 0x78, 0x56, 0xBC, 0x9A, 0xF0, 0xDE]
    );
}

#[test]
fn no_cset_file_omits_key_and_has_empty_surface() {
    let stream = magicyuv_stream(32, 32);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synth(i + 3760, 48)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r377-nocset.avi");
    mux_frames(&tmp, &stream, &frames, AviMuxOptions::new());

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.cset_chunk_count(), 0);
    assert!(dmx.cset_chunks().is_empty());
    let meta = dmx.metadata().to_vec();
    assert!(!meta.iter().any(|(k, _)| k == "avi:cset.count"));

    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}
