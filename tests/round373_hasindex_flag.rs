//! Round-373: `AVIF_HASINDEX` flag ↔ `idx1`-presence cross-check.
//!
//! Per AVI 1.0 §"AVI Index Entries"
//! (`docs/container/riff/avi-riff-file-reference.md`): *"If the file
//! contains an index, set the AVIF_HASINDEX flag in the dwFlags member
//! of the AVIMAINHEADER structure."*
//!
//! `AviDemuxer::has_index_flag_violation()` returns `Some((flag_set,
//! idx1_present))` when the `avih.dwFlags` `AVIF_HASINDEX` bit
//! disagrees with whether an `idx1` chunk is physically present, `None`
//! when they agree. Informational — never fails `open()`.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi as muxer_open_avi, AviKind, AviMuxOptions};

fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG"));
    reg.register(info);
    reg
}

fn magicyuv_stream() -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(64);
    params.height = Some(64);
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

fn mux(path: &std::path::Path, stream: &StreamInfo, frames: &[Vec<u8>], opts: AviMuxOptions) {
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = muxer_open_avi(ws, std::slice::from_ref(stream), AviKind::Avi10, opts).unwrap();
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
fn default_avi10_file_is_conforming() {
    // The AVI 1.0 muxer writes idx1 and sets AVIF_HASINDEX by default.
    let stream = magicyuv_stream();
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth(i + 3810, 80)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r373-hasindex-default.avi");
    mux(&tmp, &stream, &frames, AviMuxOptions::new());

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.has_index_flag_violation(),
        None,
        "default file: idx1 present + flag set"
    );
    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}

#[test]
fn idx1_present_but_flag_cleared_is_a_violation() {
    // Clear AVIF_HASINDEX while the muxer still writes idx1 → mismatch.
    let stream = magicyuv_stream();
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth(i + 3820, 80)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r373-hasindex-cleared.avi");
    mux(
        &tmp,
        &stream,
        &frames,
        AviMuxOptions::new().with_has_index(false),
    );

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    let v = dmx.has_index_flag_violation();
    assert_eq!(
        v,
        Some((false, true)),
        "flag clear but idx1 present must be a violation"
    );
    // The demuxer still uses the index regardless of the flag — frames
    // round-trip.
    let got = demux_all(&mut dmx);
    assert_eq!(got, frames);
}
