//! Round-373: top-level `JUNK` padding-chunk read + write symmetry.
//!
//! Per AVI 1.0 §"Other Data Chunks"
//! (`docs/container/riff/avi-riff-file-reference.md`): *"Data can be
//! aligned in an AVI file by inserting 'JUNK' chunks as needed.
//! Applications should ignore the contents of a 'JUNK' chunk."*
//!
//! Covers:
//! - muxer: `AviMuxOptions::with_top_level_junk(body_size)` appends one
//!   `JUNK` chunk per call as a sibling of `LIST hdrl`, just before the
//!   `movi` LIST. Repeatable; word-pad applied for odd sizes; these sit
//!   outside `movi` so they never shift packet / idx1 offsets.
//! - demuxer: `junk_chunks()` / `junk_chunk_count()` / `junk_total_bytes()`
//!   accessors + `avi:junk.count` / `avi:junk.total_bytes` metadata keys;
//!   each recorded chunk's file-absolute header offset lands on a `JUNK`
//!   FourCC with the declared body size; JUNK contents stay out of the
//!   packet stream and don't perturb the round-trip.
//! - absence: a file with no JUNK has empty `junk_chunks()` and omits the
//!   metadata keys.

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

fn synthesize_payload(seed: u32, base_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(base_len);
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..base_len {
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
fn top_level_junk_roundtrips_through_demuxer() {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..5).map(|i| synthesize_payload(i + 3730, 100)).collect();
    let reg = registry_with_magicyuv();

    // Two top-level JUNK chunks: one even-sized, one odd-sized (exercises
    // the RIFF word-pad path). Declared body sizes are 64 and 17.
    let opts = AviMuxOptions::new()
        .with_top_level_junk(64)
        .with_top_level_junk(17);

    let tmp = std::env::temp_dir().join("oxideav-avi-r373-junk.avi");
    mux_frames(&tmp, &stream, &frames, opts);
    let bytes = std::fs::read(&tmp).unwrap();

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    // Two JUNK chunks recorded, in call order, with verbatim declared sizes.
    assert_eq!(dmx.junk_chunk_count(), 2);
    let junk: Vec<_> = dmx.junk_chunks().to_vec();
    assert_eq!(junk.len(), 2);
    assert_eq!(junk[0].size, 64);
    assert_eq!(junk[1].size, 17);
    // total_bytes sums the declared body sizes (header + pad excluded).
    assert_eq!(dmx.junk_total_bytes(), 64 + 17);

    // Each recorded offset lands on a `JUNK` FourCC, and the on-disk
    // size field matches the recorded size.
    for (k, j) in junk.iter().enumerate() {
        let o = j.offset as usize;
        assert_eq!(
            &bytes[o..o + 4],
            b"JUNK",
            "junk {k} offset must point at JUNK"
        );
        let size_field =
            u32::from_le_bytes([bytes[o + 4], bytes[o + 5], bytes[o + 6], bytes[o + 7]]);
        assert_eq!(size_field, j.size, "junk {k} on-disk size field");
    }

    // Metadata keys mirror the typed surface.
    let meta = dmx.metadata().to_vec();
    let count = meta
        .iter()
        .find(|(k, _)| k == "avi:junk.count")
        .map(|(_, v)| v.clone());
    let total = meta
        .iter()
        .find(|(k, _)| k == "avi:junk.total_bytes")
        .map(|(_, v)| v.clone());
    assert_eq!(count.as_deref(), Some("2"));
    assert_eq!(total.as_deref(), Some("81"));

    // JUNK contents stay out of the packet stream; all frames round-trip.
    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}

#[test]
fn no_junk_file_has_empty_surface() {
    let stream = magicyuv_stream(32, 32);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 3740, 48)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r373-nojunk.avi");
    mux_frames(&tmp, &stream, &frames, AviMuxOptions::new());

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.junk_chunk_count(), 0);
    assert!(dmx.junk_chunks().is_empty());
    assert_eq!(dmx.junk_total_bytes(), 0);

    // Both metadata keys omitted so absence stays observable.
    let meta = dmx.metadata().to_vec();
    assert!(!meta.iter().any(|(k, _)| k == "avi:junk.count"));
    assert!(!meta.iter().any(|(k, _)| k == "avi:junk.total_bytes"));

    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}

#[test]
fn many_junk_chunks_preserve_order_and_sizes() {
    let stream = magicyuv_stream(48, 48);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synthesize_payload(i + 3750, 80)).collect();
    let reg = registry_with_magicyuv();

    let sizes = [0u32, 1, 2, 255, 4096];
    let mut opts = AviMuxOptions::new();
    for &s in &sizes {
        opts = opts.with_top_level_junk(s);
    }

    let tmp = std::env::temp_dir().join("oxideav-avi-r373-junk-many.avi");
    mux_frames(&tmp, &stream, &frames, opts);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = open_avi(rs, &reg).unwrap();

    let junk = dmx.junk_chunks().to_vec();
    assert_eq!(junk.len(), sizes.len());
    for (k, &s) in sizes.iter().enumerate() {
        assert_eq!(junk[k].size, s, "junk {k} size");
    }
    // Offsets strictly increasing (file order).
    for w in junk.windows(2) {
        assert!(w[1].offset > w[0].offset, "junk offsets must increase");
    }
    let expected_total: u64 = sizes.iter().map(|&s| s as u64).sum();
    assert_eq!(dmx.junk_total_bytes(), expected_total);

    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}
