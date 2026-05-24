//! Round-104: typed `vprp` active-frame-aspect-ratio accessor.
//!
//! Exercises [`AviDemuxer::vprp_frame_aspect_ratio`], the typed
//! companion to the `avi:vprp.<index>.frame_aspect_ratio` metadata key.
//! Per OpenDML 2.0 §5.0 "Active Frame Aspect Ratio" the
//! `dwFrameAspectRatio` field packs `(x << 16) | y` (high WORD = x term,
//! low WORD = y term), so `0x0004_0003` decodes to `(4, 3)` and
//! `0x0010_0009` to `(16, 9)`. The accessor unpacks that into a numeric
//! `(u16, u16)` pair so callers don't have to re-parse the `"x:y"`
//! metadata string to compute a pixel aspect ratio from frame
//! dimensions.
//!
//! Clean-room source: `docs/container/riff/opendml-avi-2.0.pdf`
//! §5.0 "Source and Header Information Storage" → "Video Properties
//! Header (vprp)".

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{
    open_avi, open_with_options, AviKind, AviMuxOptions, RiffSegmentLimit, VprpConfig,
};

fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv"))
        .tag(CodecTag::fourcc(b"M8RG"))
        .tag(CodecTag::fourcc(b"M8YA"));
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

/// Write a one-video-stream OpenDML AVI with the supplied mux options,
/// returning the raw file bytes.
fn write_opendml(stream: &StreamInfo, opts: AviMuxOptions) -> Vec<u8> {
    // A per-process atomic counter guarantees a unique temp path even when
    // two tests run concurrently and land on the same wall-clock
    // nanosecond — without it, parallel tests collided on the timestamp
    // and one read another's (or a freshly-removed) file, surfacing as
    // intermittent `open().unwrap()` panics / wrong-aspect assertions on
    // higher-core CI runners.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-avi-r104-{}-{}-{}.avi",
        std::process::id(),
        seq,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, synthesize_payload(7, 128));
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

#[test]
fn custom_16x9_aspect_round_trips_as_numeric_pair() {
    // VprpConfig::with_aspect(16, 9) ⇒ dwFrameAspectRatio = 0x0010_0009.
    let stream = magicyuv_stream(1920, 1080);
    let cfg = VprpConfig::default().with_aspect(16, 9);
    let opts = AviMuxOptions::new().with_vprp(0, cfg);
    let bytes = write_opendml(&stream, opts);

    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.vprp_frame_aspect_ratio(0), Some((16, 9)));

    // The typed pair must agree with the existing "x:y" metadata string.
    let md = dmx.metadata();
    let aspect = md
        .iter()
        .find(|(k, _)| k == "avi:vprp.0.frame_aspect_ratio")
        .map(|(_, v)| v.clone());
    assert_eq!(aspect.as_deref(), Some("16:9"));
}

#[test]
fn ntsc_preset_default_aspect_is_4_3() {
    // VprpConfig::ntsc() leaves the aspect at its 4:3 default
    // (0x0004_0003).
    let stream = magicyuv_stream(720, 480);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::ntsc());
    let bytes = write_opendml(&stream, opts);

    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.vprp_frame_aspect_ratio(0), Some((4, 3)));
}

#[test]
fn no_vprp_chunk_returns_none() {
    // Legacy AVI 1.0 emits no `vprp` chunk, so the accessor must
    // report absence rather than a synthesised default.
    let stream = magicyuv_stream(640, 480);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r104-novprp.avi");
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
        let mut pkt = Packet::new(0, stream.time_base, synthesize_payload(11, 128));
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let _ = std::fs::remove_file(&tmp);

    assert_eq!(dmx.vprp_frame_aspect_ratio(0), None);
    // Out-of-range stream index is also None.
    assert_eq!(dmx.vprp_frame_aspect_ratio(7), None);
}

#[test]
fn zero_aspect_in_present_vprp_returns_none() {
    // A `vprp` whose dwFrameAspectRatio is 0 (writer left it
    // unspecified) must report None — matching the metadata surface
    // which omits the key entirely when the value is 0. The muxer
    // always defaults to 4:3, so byte-patch the field to 0 in a real
    // OpenDML file to drive the accessor's zero branch.
    let stream = magicyuv_stream(720, 576);
    let cfg = VprpConfig::default().with_aspect(16, 9);
    let opts = AviMuxOptions::new().with_vprp(0, cfg);
    let mut bytes = write_opendml(&stream, opts);

    // Locate the `vprp` chunk and zero out its dwFrameAspectRatio,
    // which sits at body offset 20 (the 6th DWORD: VideoFormatToken,
    // VideoStandard, dwVerticalRefreshRate, dwHTotalInT,
    // dwVTotalInLines, dwFrameAspectRatio).
    let mut patched = false;
    for i in 0..bytes.len().saturating_sub(8) {
        if &bytes[i..i + 4] == b"vprp" {
            let body = i + 8;
            let off = body + 20;
            // Sanity: confirm the bytes we're about to clear currently
            // hold the 16:9 value we wrote, so a layout shift can't make
            // this test silently pass.
            assert_eq!(
                u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3],]),
                (16u32 << 16) | 9,
            );
            bytes[off] = 0;
            bytes[off + 1] = 0;
            bytes[off + 2] = 0;
            bytes[off + 3] = 0;
            patched = true;
            break;
        }
    }
    assert!(patched, "expected a vprp chunk in OpenDML output");

    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // vprp is still present (nbFieldPerFrame > 0) but the aspect is 0.
    assert_eq!(dmx.vprp_frame_aspect_ratio(0), None);
    let md = dmx.metadata();
    let has_aspect_key = md.iter().any(|(k, _)| k == "avi:vprp.0.frame_aspect_ratio");
    assert!(
        !has_aspect_key,
        "metadata surface must omit the aspect key when dwFrameAspectRatio == 0"
    );
}
