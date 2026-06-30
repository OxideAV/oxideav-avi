//! Round-381 milestone 2: muxer-side `BITMAPINFOHEADER` scalar-field
//! overrides, round-tripped through the demuxer accessors landed in
//! milestone 1.
//!
//! `AviMuxOptions::with_size_image` / `with_pixels_per_meter` /
//! `with_clr_important` / `with_bmih_planes` patch the named BMIH field
//! verbatim into a video stream's 40-byte fixed header, replacing the
//! muxer's writer-default `0` (`biSizeImage`, `biX/YPelsPerMeter`,
//! `biClrImportant`) / `1` (`biPlanes`) per VfW `wingdi.h`
//! §"BITMAPINFOHEADER". `biClrUsed` stays owned by `with_indexed_video`
//! (it is load-bearing for the color-table length) and is intentionally
//! not overridable here.
//!
//! Clean-room source:
//!   - `docs/container/riff/avi-riff-file-reference.md` §BITMAPINFOHEADER
//!   - `docs/container/riff/metadata/microsoft-riffmci.pdf`

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

fn build_and_open(opts: AviMuxOptions, tag: &str) -> oxideav_avi::demuxer::AviDemuxer {
    let stream = magicyuv_stream(64, 48);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synth(i + 100, 96)).collect();
    let reg = registry_with_magicyuv();
    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r381-{tag}.avi"));
    mux_frames(&tmp, &stream, &frames, opts);
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    open_avi(rs, &reg).unwrap()
}

#[test]
fn size_image_override_roundtrips() {
    let dmx = build_and_open(AviMuxOptions::new().with_size_image(0, 12345), "size");
    assert_eq!(dmx.stream_size_image(0), Some(12345));
    assert_eq!(
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == "avi:vids.0.size_image")
            .map(|(_, v)| v.as_str()),
        Some("12345")
    );
}

#[test]
fn default_size_image_is_zero_and_absent() {
    // No override ⇒ muxer default 0 ⇒ demuxer folds to None, no key.
    let dmx = build_and_open(AviMuxOptions::new(), "size-default");
    assert_eq!(dmx.stream_size_image(0), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:vids.0.size_image"));
}

#[test]
fn pixels_per_meter_override_roundtrips() {
    let dmx = build_and_open(
        AviMuxOptions::new().with_pixels_per_meter(0, 3780, 3779),
        "ppm",
    );
    assert_eq!(dmx.stream_pixels_per_meter(0), Some((3780, 3779)));
    let md = dmx.metadata();
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:vids.0.x_pels_per_meter")
            .map(|(_, v)| v.as_str()),
        Some("3780")
    );
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:vids.0.y_pels_per_meter")
            .map(|(_, v)| v.as_str()),
        Some("3779")
    );
}

#[test]
fn clr_important_override_roundtrips() {
    let dmx = build_and_open(AviMuxOptions::new().with_clr_important(0, 32), "clr");
    assert_eq!(dmx.stream_clr_important(0), Some(32));
    assert_eq!(
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == "avi:vids.0.clr_important")
            .map(|(_, v)| v.as_str()),
        Some("32")
    );
}

#[test]
fn planes_override_roundtrips_nonconforming() {
    // Default biPlanes is the mandated 1 ⇒ no key.
    let def = build_and_open(AviMuxOptions::new(), "planes-default");
    assert_eq!(def.stream_planes(0), Some(1));
    assert!(!def.metadata().iter().any(|(k, _)| k == "avi:vids.0.planes"));

    // A non-conformant override of 2 is observable.
    let dmx = build_and_open(AviMuxOptions::new().with_bmih_planes(0, 2), "planes-2");
    assert_eq!(dmx.stream_planes(0), Some(2));
    assert_eq!(
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == "avi:vids.0.planes")
            .map(|(_, v)| v.as_str()),
        Some("2")
    );
}

#[test]
fn multiple_overrides_accumulate_into_one_header() {
    // All four builders on the same stream accumulate; last call per
    // field wins.
    let opts = AviMuxOptions::new()
        .with_size_image(0, 999)
        .with_pixels_per_meter(0, 100, 200)
        .with_clr_important(0, 8)
        .with_bmih_planes(0, 1) // explicit 1 == conforming default
        .with_size_image(0, 4242); // last call wins
    let dmx = build_and_open(opts, "multi");
    assert_eq!(dmx.stream_size_image(0), Some(4242));
    assert_eq!(dmx.stream_pixels_per_meter(0), Some((100, 200)));
    assert_eq!(dmx.stream_clr_important(0), Some(8));
    // biPlanes stamped explicitly to 1 ⇒ conforming ⇒ no key.
    assert_eq!(dmx.stream_planes(0), Some(1));
    assert!(!dmx.metadata().iter().any(|(k, _)| k == "avi:vids.0.planes"));
}
