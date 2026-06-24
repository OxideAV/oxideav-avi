//! Round-365: typed `vprp` signal-shape scalar accessors.
//!
//! Exercises the typed companions to the
//! `avi:vprp.<index>.{vertical_refresh_rate,h_total_in_t,
//! v_total_in_lines,frame_width_in_pixels,frame_height_in_lines}` raw
//! metadata keys:
//!   - [`AviDemuxer::vprp_vertical_refresh_rate`]
//!   - [`AviDemuxer::vprp_h_total_in_t`]
//!   - [`AviDemuxer::vprp_v_total_in_lines`]
//!   - [`AviDemuxer::vprp_frame_width_in_pixels`]
//!   - [`AviDemuxer::vprp_frame_height_in_lines`]
//!   - [`AviDemuxer::vprp_signal_shape`] (the five-DWORD bundle)
//!
//! Per OpenDML 2.0 §5.0 "Video Properties Header (vprp)":
//!   - "Vertical Refresh Rate" — "Used when an unknown standard is
//!     specified. Normally, 60 for NTSC, and 50 for PAL."
//!   - "H-Total in T" — "Defines the horizontal total, in T (one
//!     luminance sample: pixel)."
//!   - "V-Total in Lines" — "Defines the vertical total, in lines."
//!   - "Active Frame Width in Pixels" / "Active Frame Height in Lines"
//!     — "the active frame width/height in pixels/lines."
//!
//! The muxer derives `dwHTotalInT` / `dwVTotalInLines` /
//! `dwFrameWidthInPixels` / `dwFrameHeightInLines` from the stream's
//! coded width/height, and `dwVerticalRefreshRate` from the
//! `VprpConfig::vertical_refresh_rate` override (or the stream fps when
//! `0`). So a 720x480 stream with `VprpConfig::ntsc()` (refresh 60)
//! round-trips through every accessor with the values asserted below.
//!
//! Clean-room source: `docs/container/riff/opendml-avi-2.0.pdf`
//! §5.0 "Source and Header Information Storage" -> "Video Properties
//! Header (vprp)".

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{open_avi as demuxer_open_avi, VprpSignalShape};
use oxideav_avi::muxer::{open_with_options, AviKind, AviMuxOptions, RiffSegmentLimit, VprpConfig};

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

fn write_opendml(stream: &StreamInfo, opts: AviMuxOptions) -> Vec<u8> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-avi-r365-{}-{}-{}.avi",
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
        let mut pkt = Packet::new(0, stream.time_base, vec![0u8; 128]);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn open(bytes: Vec<u8>) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = registry_with_magicyuv();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    demuxer_open_avi(rs, &reg).unwrap()
}

fn meta(dmx: &oxideav_avi::demuxer::AviDemuxer, key: &str) -> Option<String> {
    dmx.metadata()
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// NTSC preset on a 720x480 stream: refresh 60 (from the preset),
/// h_total / frame_width = 720, v_total / frame_height = 480 (derived
/// from the coded dimensions).
#[test]
fn ntsc_preset_signal_shape_round_trips() {
    let stream = magicyuv_stream(720, 480);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::ntsc());
    let dmx = open(write_opendml(&stream, opts));

    assert_eq!(dmx.vprp_vertical_refresh_rate(0), Some(60));
    assert_eq!(dmx.vprp_h_total_in_t(0), Some(720));
    assert_eq!(dmx.vprp_v_total_in_lines(0), Some(480));
    assert_eq!(dmx.vprp_frame_width_in_pixels(0), Some(720));
    assert_eq!(dmx.vprp_frame_height_in_lines(0), Some(480));

    assert_eq!(
        dmx.vprp_signal_shape(0),
        Some(VprpSignalShape {
            vertical_refresh_rate: 60,
            h_total_in_t: 720,
            v_total_in_lines: 480,
            frame_width_in_pixels: 720,
            frame_height_in_lines: 480,
        })
    );
}

/// PAL preset on a 720x576 stream: refresh 50, dimensions 720x576.
#[test]
fn pal_preset_signal_shape_round_trips() {
    let stream = magicyuv_stream(720, 576);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::pal());
    let dmx = open(write_opendml(&stream, opts));

    assert_eq!(dmx.vprp_vertical_refresh_rate(0), Some(50));
    assert_eq!(dmx.vprp_h_total_in_t(0), Some(720));
    assert_eq!(dmx.vprp_v_total_in_lines(0), Some(576));
    assert_eq!(dmx.vprp_frame_width_in_pixels(0), Some(720));
    assert_eq!(dmx.vprp_frame_height_in_lines(0), Some(576));
}

/// Typed accessors agree with the raw `avi:vprp.0.*` metadata keys.
#[test]
fn accessors_agree_with_metadata_keys() {
    let stream = magicyuv_stream(640, 480);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::ntsc());
    let dmx = open(write_opendml(&stream, opts));

    assert_eq!(
        meta(&dmx, "avi:vprp.0.vertical_refresh_rate").as_deref(),
        dmx.vprp_vertical_refresh_rate(0)
            .map(|v| v.to_string())
            .as_deref()
    );
    assert_eq!(
        meta(&dmx, "avi:vprp.0.h_total_in_t").as_deref(),
        dmx.vprp_h_total_in_t(0).map(|v| v.to_string()).as_deref()
    );
    assert_eq!(
        meta(&dmx, "avi:vprp.0.v_total_in_lines").as_deref(),
        dmx.vprp_v_total_in_lines(0)
            .map(|v| v.to_string())
            .as_deref()
    );
    assert_eq!(
        meta(&dmx, "avi:vprp.0.frame_width_in_pixels").as_deref(),
        dmx.vprp_frame_width_in_pixels(0)
            .map(|v| v.to_string())
            .as_deref()
    );
    assert_eq!(
        meta(&dmx, "avi:vprp.0.frame_height_in_lines").as_deref(),
        dmx.vprp_frame_height_in_lines(0)
            .map(|v| v.to_string())
            .as_deref()
    );
}

/// A stream with no `vprp` chunk: every accessor returns `None`. (An
/// `Avi10`-kind file carries no `vprp` at all.)
#[test]
fn no_vprp_chunk_returns_none() {
    let stream = magicyuv_stream(320, 240);
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-avi-r365-novprp-{}-{}.avi",
        std::process::id(),
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
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, vec![0u8; 64]);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    let dmx = open(bytes);

    assert_eq!(dmx.vprp_vertical_refresh_rate(0), None);
    assert_eq!(dmx.vprp_h_total_in_t(0), None);
    assert_eq!(dmx.vprp_v_total_in_lines(0), None);
    assert_eq!(dmx.vprp_frame_width_in_pixels(0), None);
    assert_eq!(dmx.vprp_frame_height_in_lines(0), None);
    assert_eq!(dmx.vprp_signal_shape(0), None);
}

/// Out-of-range stream index returns `None` on every accessor.
#[test]
fn out_of_range_stream_returns_none() {
    let stream = magicyuv_stream(720, 480);
    let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::ntsc());
    let dmx = open(write_opendml(&stream, opts));

    assert_eq!(dmx.vprp_vertical_refresh_rate(99), None);
    assert_eq!(dmx.vprp_h_total_in_t(99), None);
    assert_eq!(dmx.vprp_v_total_in_lines(99), None);
    assert_eq!(dmx.vprp_frame_width_in_pixels(99), None);
    assert_eq!(dmx.vprp_frame_height_in_lines(99), None);
    assert_eq!(dmx.vprp_signal_shape(99), None);
}

/// Hand-rolled `vprp` body with a recognised-standard token that leaves
/// `dwVerticalRefreshRate == 0` (the standard implies it). The
/// individual refresh accessor maps `0` to `None`, but the bundle
/// surfaces the `0` verbatim alongside the other populated fields, so a
/// caller can see exactly which DWORDs the writer filled.
#[test]
fn signal_shape_bundle_surfaces_zero_refresh_verbatim() {
    // Build a minimal RIFF AVI carrying a single video strl with a
    // hand-rolled vprp: format=PAL_CCIR_601(2), standard=PAL(1),
    // refresh=0, h_total=864, v_total=625, aspect=4:3,
    // frame_w=720, frame_h=576, nbFieldPerFrame=1, one field desc.
    let mut vprp = Vec::new();
    let dw = |v: u32| v.to_le_bytes();
    vprp.extend_from_slice(&dw(2)); // VideoFormatToken = PAL_CCIR_601
    vprp.extend_from_slice(&dw(1)); // VideoStandard = PAL
    vprp.extend_from_slice(&dw(0)); // dwVerticalRefreshRate = 0 (implied)
    vprp.extend_from_slice(&dw(864)); // dwHTotalInT
    vprp.extend_from_slice(&dw(625)); // dwVTotalInLines
    vprp.extend_from_slice(&dw((4 << 16) | 3)); // dwFrameAspectRatio
    vprp.extend_from_slice(&dw(720)); // dwFrameWidthInPixels
    vprp.extend_from_slice(&dw(576)); // dwFrameHeightInLines
    vprp.extend_from_slice(&dw(1)); // nbFieldPerFrame
                                    // One VIDEO_FIELD_DESC (8 DWORDs).
    for _ in 0..8 {
        vprp.extend_from_slice(&dw(0));
    }

    let bytes = build_minimal_avi_with_vprp(720, 576, &vprp);
    let dmx = open(bytes);

    // Individual accessor folds 0 to None.
    assert_eq!(dmx.vprp_vertical_refresh_rate(0), None);
    // Bundle surfaces 0 verbatim alongside the populated fields.
    assert_eq!(
        dmx.vprp_signal_shape(0),
        Some(VprpSignalShape {
            vertical_refresh_rate: 0,
            h_total_in_t: 864,
            v_total_in_lines: 625,
            frame_width_in_pixels: 720,
            frame_height_in_lines: 576,
        })
    );
    // h_total / v_total / frame_w / frame_h individual accessors are
    // non-zero so they surface.
    assert_eq!(dmx.vprp_h_total_in_t(0), Some(864));
    assert_eq!(dmx.vprp_v_total_in_lines(0), Some(625));
    assert_eq!(dmx.vprp_frame_width_in_pixels(0), Some(720));
    assert_eq!(dmx.vprp_frame_height_in_lines(0), Some(576));
}

/// Build a minimal single-video-stream `RIFF....AVI ` file whose `strl`
/// carries the supplied raw `vprp` body, with a `movi` LIST holding one
/// `00db` chunk. Enough structure for the demuxer to parse the strl +
/// vprp; no index.
fn build_minimal_avi_with_vprp(width: u32, height: u32, vprp_body: &[u8]) -> Vec<u8> {
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }
    let dw = |v: u32| v.to_le_bytes();

    // avih (56 bytes).
    let mut avih = Vec::new();
    avih.extend_from_slice(&dw(40000)); // dwMicroSecPerFrame
    avih.extend_from_slice(&dw(0)); // dwMaxBytesPerSec
    avih.extend_from_slice(&dw(0)); // dwPaddingGranularity
    avih.extend_from_slice(&dw(0x10)); // dwFlags (HASINDEX-ish, unused)
    avih.extend_from_slice(&dw(1)); // dwTotalFrames
    avih.extend_from_slice(&dw(0)); // dwInitialFrames
    avih.extend_from_slice(&dw(1)); // dwStreams
    avih.extend_from_slice(&dw(0)); // dwSuggestedBufferSize
    avih.extend_from_slice(&dw(width)); // dwWidth
    avih.extend_from_slice(&dw(height)); // dwHeight
    for _ in 0..4 {
        avih.extend_from_slice(&dw(0)); // dwReserved[4]
    }

    // strh (56 bytes) for a video stream.
    let mut strh = Vec::new();
    strh.extend_from_slice(b"vids"); // fccType
    strh.extend_from_slice(b"M8RG"); // fccHandler
    strh.extend_from_slice(&dw(0)); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&dw(0)); // dwInitialFrames
    strh.extend_from_slice(&dw(1)); // dwScale
    strh.extend_from_slice(&dw(25)); // dwRate
    strh.extend_from_slice(&dw(0)); // dwStart
    strh.extend_from_slice(&dw(1)); // dwLength
    strh.extend_from_slice(&dw(0)); // dwSuggestedBufferSize
    strh.extend_from_slice(&dw(0xFFFF_FFFF)); // dwQuality
    strh.extend_from_slice(&dw(0)); // dwSampleSize
    for _ in 0..4 {
        strh.extend_from_slice(&0u16.to_le_bytes()); // rcFrame (4 x WORD)
    }

    // strf = BITMAPINFOHEADER (40 bytes).
    let mut strf = Vec::new();
    strf.extend_from_slice(&dw(40)); // biSize
    strf.extend_from_slice(&dw(width)); // biWidth
    strf.extend_from_slice(&dw(height)); // biHeight
    strf.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    strf.extend_from_slice(b"M8RG"); // biCompression
    strf.extend_from_slice(&dw(0)); // biSizeImage
    strf.extend_from_slice(&dw(0)); // biXPelsPerMeter
    strf.extend_from_slice(&dw(0)); // biYPelsPerMeter
    strf.extend_from_slice(&dw(0)); // biClrUsed
    strf.extend_from_slice(&dw(0)); // biClrImportant

    // strl LIST = strh + strf + vprp.
    let mut strl = Vec::new();
    strl.extend_from_slice(b"strl");
    strl.extend_from_slice(&chunk(b"strh", &strh));
    strl.extend_from_slice(&chunk(b"strf", &strf));
    strl.extend_from_slice(&chunk(b"vprp", vprp_body));
    let strl_list = {
        let mut out = Vec::new();
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&(strl.len() as u32).to_le_bytes());
        out.extend_from_slice(&strl);
        out
    };

    // hdrl LIST = avih + strl.
    let mut hdrl = Vec::new();
    hdrl.extend_from_slice(b"hdrl");
    hdrl.extend_from_slice(&chunk(b"avih", &avih));
    hdrl.extend_from_slice(&strl_list);
    let hdrl_list = {
        let mut out = Vec::new();
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&(hdrl.len() as u32).to_le_bytes());
        out.extend_from_slice(&hdrl);
        out
    };

    // movi LIST = one 00db chunk.
    let mut movi = Vec::new();
    movi.extend_from_slice(b"movi");
    movi.extend_from_slice(&chunk(b"00db", &[0u8; 16]));
    let movi_list = {
        let mut out = Vec::new();
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&(movi.len() as u32).to_le_bytes());
        out.extend_from_slice(&movi);
        out
    };

    let mut riff_body = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(&hdrl_list);
    riff_body.extend_from_slice(&movi_list);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);
    out
}
