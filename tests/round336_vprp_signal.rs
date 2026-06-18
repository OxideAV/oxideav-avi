//! Round-336: typed `vprp` VideoFormatToken / VideoStandard accessors.
//!
//! Exercises [`AviDemuxer::vprp_video_format`] and
//! [`AviDemuxer::vprp_video_standard`], the typed companions to the
//! `avi:vprp.<index>.video_format_token` / `.video_standard` raw
//! metadata keys and the new `.video_format_label` / `.video_standard_label`
//! named-label keys.
//!
//! Per OpenDML 2.0 §5.0 "Video Properties Header (vprp)":
//!   - "Video Format Token" enumerates `{FORMAT_UNKNOWN,
//!     FORMAT_PAL_SQUARE, FORMAT_PAL_CCIR_601, FORMAT_NTSC_SQUARE,
//!     FORMAT_NTSC_CCIR_601, ...}` (ordinals 0..=4, the trailing `...`
//!     leaving room for vendor/future tokens).
//!   - "Video Standard" enumerates the closed set `{STANDARD_UNKNOWN,
//!     STANDARD_PAL, STANDARD_NTSC, STANDARD_SECAM}` (ordinals 0..=3).
//!
//! The muxer's `VprpConfig::{ntsc, pal, secam}` presets stamp:
//!   - ntsc:  FORMAT_NTSC_CCIR_601 (4) + STANDARD_NTSC  (2)
//!   - pal:   FORMAT_PAL_CCIR_601  (2) + STANDARD_PAL   (1)
//!   - secam: FORMAT_UNKNOWN       (0) + STANDARD_SECAM (3)
//!
//! Clean-room source: `docs/container/riff/opendml-avi-2.0.pdf`
//! §5.0 "Source and Header Information Storage" → "Video Properties
//! Header (vprp)".

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Muxer, Packet, Rational,
    ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{open_avi as demuxer_open_avi, VprpVideoFormat, VprpVideoStandard};
use oxideav_avi::muxer::{open_with_options, AviKind, AviMuxOptions, RiffSegmentLimit, VprpConfig};
use oxideav_core::MediaType;

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
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-avi-r336-{}-{}-{}.avi",
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

fn label(md: &[(String, String)], key: &str) -> Option<String> {
    md.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

#[test]
fn ntsc_preset_decodes_to_named_format_and_standard() {
    let stream = magicyuv_stream(720, 480);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::ntsc());
    let bytes = write_opendml(&stream, opts);

    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.vprp_video_format(0), Some(VprpVideoFormat::NtscCcir601));
    assert_eq!(dmx.vprp_video_standard(0), Some(VprpVideoStandard::Ntsc));

    // Named-label metadata keys.
    let md = dmx.metadata();
    assert_eq!(
        label(md, "avi:vprp.0.video_format_label").as_deref(),
        Some("ntsc_ccir_601")
    );
    assert_eq!(
        label(md, "avi:vprp.0.video_standard_label").as_deref(),
        Some("ntsc")
    );
    // Raw keys still present and agree with to_raw().
    assert_eq!(
        label(md, "avi:vprp.0.video_format_token").as_deref(),
        Some(VprpVideoFormat::NtscCcir601.to_raw().to_string().as_str())
    );
    assert_eq!(
        label(md, "avi:vprp.0.video_standard").as_deref(),
        Some(VprpVideoStandard::Ntsc.to_raw().to_string().as_str())
    );
}

#[test]
fn pal_preset_decodes_to_named_format_and_standard() {
    let stream = magicyuv_stream(720, 576);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::pal());
    let bytes = write_opendml(&stream, opts);

    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.vprp_video_format(0), Some(VprpVideoFormat::PalCcir601));
    assert_eq!(dmx.vprp_video_standard(0), Some(VprpVideoStandard::Pal));

    let md = dmx.metadata();
    assert_eq!(
        label(md, "avi:vprp.0.video_format_label").as_deref(),
        Some("pal_ccir_601")
    );
    assert_eq!(
        label(md, "avi:vprp.0.video_standard_label").as_deref(),
        Some("pal")
    );
}

#[test]
fn secam_preset_has_unknown_format_but_secam_standard() {
    // The SECAM preset uses FORMAT_UNKNOWN (no SECAM format token in
    // §5.0) + STANDARD_SECAM. So the format decodes to the meaningful
    // `Unknown` variant (not `None`) and its label key is omitted,
    // while the standard decodes to `Secam` with a label.
    let stream = magicyuv_stream(720, 576);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::secam());
    let bytes = write_opendml(&stream, opts);

    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.vprp_video_format(0), Some(VprpVideoFormat::Unknown));
    assert_eq!(dmx.vprp_video_standard(0), Some(VprpVideoStandard::Secam));

    let md = dmx.metadata();
    // FORMAT_UNKNOWN ⇒ no label key (default == absent).
    assert!(
        label(md, "avi:vprp.0.video_format_label").is_none(),
        "FORMAT_UNKNOWN must omit the video_format_label key"
    );
    assert_eq!(
        label(md, "avi:vprp.0.video_standard_label").as_deref(),
        Some("secam")
    );
    // Raw video_format_token is still emitted (always-present) and is 0.
    assert_eq!(
        label(md, "avi:vprp.0.video_format_token").as_deref(),
        Some("0")
    );
}

#[test]
fn no_vprp_chunk_returns_none() {
    // Legacy AVI 1.0 emits no `vprp` chunk → both accessors report
    // absence, and an out-of-range stream index is also None.
    let stream = magicyuv_stream(640, 480);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join(format!(
        "oxideav-avi-r336-novprp-{}.avi",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(
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

    assert_eq!(dmx.vprp_video_format(0), None);
    assert_eq!(dmx.vprp_video_standard(0), None);
    assert_eq!(dmx.vprp_video_format(7), None);
    assert_eq!(dmx.vprp_video_standard(7), None);
}

#[test]
fn out_of_range_ordinals_surface_verbatim_as_other() {
    // Byte-patch the VideoFormatToken (body offset 0) and VideoStandard
    // (body offset 4) of a real OpenDML file to ordinals outside the
    // documented enums, confirming they round-trip through `Other`
    // (the spec's format enum ends in `...`; the standard enum is
    // closed, so an out-of-range value is a malformed/vendor value —
    // both surface verbatim rather than being rejected).
    let stream = magicyuv_stream(720, 480);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::ntsc());
    let mut bytes = write_opendml(&stream, opts);

    let mut patched = false;
    for i in 0..bytes.len().saturating_sub(8) {
        if &bytes[i..i + 4] == b"vprp" {
            let body = i + 8;
            // Sanity: the NTSC preset wrote token=4, standard=2.
            assert_eq!(
                u32::from_le_bytes([
                    bytes[body],
                    bytes[body + 1],
                    bytes[body + 2],
                    bytes[body + 3]
                ]),
                4,
            );
            assert_eq!(
                u32::from_le_bytes([
                    bytes[body + 4],
                    bytes[body + 5],
                    bytes[body + 6],
                    bytes[body + 7],
                ]),
                2,
            );
            bytes[body..body + 4].copy_from_slice(&99u32.to_le_bytes());
            bytes[body + 4..body + 8].copy_from_slice(&77u32.to_le_bytes());
            patched = true;
            break;
        }
    }
    assert!(patched, "expected a vprp chunk in OpenDML output");

    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.vprp_video_format(0), Some(VprpVideoFormat::Other(99)));
    assert_eq!(
        dmx.vprp_video_standard(0),
        Some(VprpVideoStandard::Other(77))
    );

    // Out-of-range ⇒ no label keys (recognised-values-only gating).
    let md = dmx.metadata();
    assert!(label(md, "avi:vprp.0.video_format_label").is_none());
    assert!(label(md, "avi:vprp.0.video_standard_label").is_none());
    // Raw keys carry the patched ordinals verbatim.
    assert_eq!(
        label(md, "avi:vprp.0.video_format_token").as_deref(),
        Some("99")
    );
    assert_eq!(
        label(md, "avi:vprp.0.video_standard").as_deref(),
        Some("77")
    );
}

#[test]
fn enum_roundtrip_from_raw_to_raw_is_identity() {
    for raw in 0u32..=6 {
        assert_eq!(VprpVideoFormat::from_raw(raw).to_raw(), raw);
        assert_eq!(VprpVideoStandard::from_raw(raw).to_raw(), raw);
    }
    // Spot-check the named variants.
    assert_eq!(VprpVideoFormat::from_raw(0), VprpVideoFormat::Unknown);
    assert_eq!(VprpVideoFormat::from_raw(1), VprpVideoFormat::PalSquare);
    assert_eq!(VprpVideoFormat::from_raw(2), VprpVideoFormat::PalCcir601);
    assert_eq!(VprpVideoFormat::from_raw(3), VprpVideoFormat::NtscSquare);
    assert_eq!(VprpVideoFormat::from_raw(4), VprpVideoFormat::NtscCcir601);
    assert_eq!(VprpVideoStandard::from_raw(0), VprpVideoStandard::Unknown);
    assert_eq!(VprpVideoStandard::from_raw(1), VprpVideoStandard::Pal);
    assert_eq!(VprpVideoStandard::from_raw(2), VprpVideoStandard::Ntsc);
    assert_eq!(VprpVideoStandard::from_raw(3), VprpVideoStandard::Secam);

    // Labels fire only for documented non-UNKNOWN values.
    assert_eq!(VprpVideoFormat::Unknown.label(), None);
    assert_eq!(VprpVideoFormat::PalSquare.label(), Some("pal_square"));
    assert_eq!(VprpVideoFormat::Other(42).label(), None);
    assert_eq!(VprpVideoStandard::Unknown.label(), None);
    assert_eq!(VprpVideoStandard::Secam.label(), Some("secam"));
    assert_eq!(VprpVideoStandard::Other(42).label(), None);
}
