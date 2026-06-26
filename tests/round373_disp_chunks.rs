//! Round-373: top-level `DISP` chunk read + write symmetry.
//!
//! `DISP` is a RIFF-level display / sound-scheme-title chunk listed in
//! the RIFF tag registry
//! (`docs/container/riff/metadata/exiftool-riff-tags.html` maps
//! `'DISP'` → `SoundSchemeTitle`). It is a regular RIFF chunk carried
//! at the top level alongside `LIST hdrl` / `LIST INFO`. Unlike `JUNK`
//! its body is meaningful, so the demuxer retains the raw body bytes
//! verbatim. The leading 4-byte clipboard-format code is NOT
//! interpreted (its structure is not in the in-tree docs — see the
//! round-373 docs-gap note).
//!
//! Covers:
//! - muxer: `AviMuxOptions::with_disp_chunk(body)` appends one `DISP`
//!   chunk per call as a sibling of `LIST hdrl`, just before `movi`.
//! - demuxer: `disp_chunks()` / `disp_chunk_count()` + `avi:disp.count`
//!   metadata key; raw body round-trips byte-equal; odd-length bodies
//!   exercise the word-pad path; absence omits the key.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi;
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
fn disp_chunk_body_roundtrips_byte_equal() {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth(i + 3780, 96)).collect();
    let reg = registry_with_magicyuv();

    // Two DISP bodies: a CF_TEXT-shaped one (leading 4-byte format code
    // 0x0001 + ASCII title) and an odd-length one (word-pad path).
    let mut body0 = vec![0x01, 0x00, 0x00, 0x00];
    body0.extend_from_slice(b"Sound Scheme Title\0");
    let body1 = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x42]; // 5 bytes (odd)

    let opts = AviMuxOptions::new()
        .with_disp_chunk(body0.clone())
        .with_disp_chunk(body1.clone());

    let tmp = std::env::temp_dir().join("oxideav-avi-r373-disp.avi");
    mux_frames(&tmp, &stream, &frames, opts);
    let bytes = std::fs::read(&tmp).unwrap();

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.disp_chunk_count(), 2);
    let disp = dmx.disp_chunks().to_vec();
    assert_eq!(disp.len(), 2);

    // Bodies round-trip byte-equal; declared size == body length.
    assert_eq!(disp[0].body, body0);
    assert_eq!(disp[0].size as usize, body0.len());
    assert_eq!(disp[1].body, body1);
    assert_eq!(disp[1].size as usize, body1.len());

    // Each recorded offset points at a `DISP` FourCC with a matching
    // on-disk size field.
    for (k, d) in disp.iter().enumerate() {
        let o = d.offset as usize;
        assert_eq!(&bytes[o..o + 4], b"DISP", "disp {k} offset");
        let size_field =
            u32::from_le_bytes([bytes[o + 4], bytes[o + 5], bytes[o + 6], bytes[o + 7]]);
        assert_eq!(size_field, d.size, "disp {k} on-disk size field");
    }

    // Metadata key mirrors the count.
    let meta = dmx.metadata().to_vec();
    let count = meta
        .iter()
        .find(|(k, _)| k == "avi:disp.count")
        .map(|(_, v)| v.clone());
    assert_eq!(count.as_deref(), Some("2"));

    // DISP stays out of the packet stream; frames round-trip.
    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}

#[test]
fn no_disp_file_omits_key_and_has_empty_surface() {
    let stream = magicyuv_stream(32, 32);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synth(i + 3790, 48)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r373-nodisp.avi");
    mux_frames(&tmp, &stream, &frames, AviMuxOptions::new());

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.disp_chunk_count(), 0);
    assert!(dmx.disp_chunks().is_empty());
    let meta = dmx.metadata().to_vec();
    assert!(!meta.iter().any(|(k, _)| k == "avi:disp.count"));

    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}

#[test]
fn empty_disp_body_roundtrips() {
    let stream = magicyuv_stream(48, 48);
    let frames: Vec<Vec<u8>> = (0..2).map(|i| synth(i + 3800, 64)).collect();
    let reg = registry_with_magicyuv();

    // A zero-length DISP body is still a present chunk.
    let opts = AviMuxOptions::new().with_disp_chunk(Vec::<u8>::new());
    let tmp = std::env::temp_dir().join("oxideav-avi-r373-disp-empty.avi");
    mux_frames(&tmp, &stream, &frames, opts);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.disp_chunk_count(), 1);
    assert_eq!(dmx.disp_chunks()[0].size, 0);
    assert!(dmx.disp_chunks()[0].body.is_empty());

    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}
