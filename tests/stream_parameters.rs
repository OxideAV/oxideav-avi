//! Stream parameters an AVI video stream reports before any packet is
//! decoded: picture size, decoded pixel format and frame rate.
//!
//! The `BITMAPINFOHEADER` of a compressed stream says nothing about
//! the decoded layout (ffmpeg writes `biBitCount = 24` for every
//! codec), so the layout comes from the codec's parameter sets —
//! in-band in the first frame for every fixture here.
//!
//! The fixtures under `tests/fixtures/stream_params/` were made with
//! ffmpeg (`testsrc2`, 66x50 — a size that is not macroblock- or
//! CTB-aligned, so the SPS crop is exercised — at 25 fps; AV1 at
//! 64x48 / 24000/1001, the 10-bit VP9 one at 30000/1001; HEVC with
//! `-tag:v HEVC`). The expected values are ffprobe's:
//!
//! ```text
//! ffprobe -v error -count_packets -select_streams v:0 \
//!   -show_entries stream=codec_tag_string,width,height,pix_fmt,r_frame_rate,nb_read_packets FILE
//! ```

use oxideav_core::{Demuxer, Error, NullCodecResolver, PixelFormat, Rational};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stream_params/");

use PixelFormat::*;

/// `(file, size, pixel format, frame rate, packets)`.
type Case = (
    &'static str,
    (u32, u32),
    Option<PixelFormat>,
    (i64, i64),
    usize,
);

const CASES: &[Case] = &[
    ("h264_420p.avi", (66, 50), Some(Yuv420P), (25, 1), 2),
    ("h264_420p10.avi", (66, 50), Some(Yuv420P10Le), (25, 1), 2),
    ("h264_422p.avi", (66, 50), Some(Yuv422P), (25, 1), 2),
    ("h264_444p.avi", (66, 50), Some(Yuv444P), (25, 1), 2),
    ("hevc_420p.avi", (66, 50), Some(Yuv420P), (25, 1), 2),
    ("hevc_422p10.avi", (66, 50), Some(Yuv422P10Le), (25, 1), 2),
    ("vp9_420p.avi", (66, 50), Some(Yuv420P), (25, 1), 2),
    (
        "vp9_420p10.avi",
        (66, 50),
        Some(Yuv420P10Le),
        (30000, 1001),
        3,
    ),
    ("vp9_444p.avi", (66, 50), Some(Yuv444P), (25, 1), 2),
    (
        "av1_420p10.avi",
        (64, 48),
        Some(Yuv420P10Le),
        (24000, 1001),
        3,
    ),
    // ffprobe says yuvj420p, but only by decoding the JPEG: neither the
    // BITMAPINFOHEADER nor the MJPG FourCC states the sampling factors
    // or the colour transform — that is the JPEG decoder's report.
    ("mjpeg_420p.avi", (66, 50), None, (25, 1), 2),
];

fn open_bytes(bytes: Vec<u8>) -> Box<dyn Demuxer> {
    oxideav_avi::demuxer::open(Box::new(std::io::Cursor::new(bytes)), &NullCodecResolver)
        .expect("open")
}

fn read(file: &str) -> Vec<u8> {
    std::fs::read(format!("{DIR}{file}")).expect(file)
}

#[test]
fn every_field_matches_ffprobe() {
    for &(file, size, pf, fr, _) in CASES {
        let d = open_bytes(read(file));
        let p = &d.streams()[0].params;
        assert_eq!((p.width, p.height), (Some(size.0), Some(size.1)), "{file}");
        assert_eq!(p.pixel_format, pf, "{file}");
        assert_eq!(p.frame_rate, Some(Rational::new(fr.0, fr.1)), "{file}");
    }
}

#[test]
fn reading_the_first_frame_keeps_every_packet() {
    for &(file, _, _, _, packets) in CASES {
        let mut d = open_bytes(read(file));
        let mut n = 0;
        loop {
            match d.next_packet() {
                Ok(p) => {
                    if n == 0 {
                        assert!(p.flags.keyframe, "{file}: first packet is the key frame");
                        assert!(!p.data.is_empty(), "{file}");
                    }
                    n += 1;
                }
                Err(Error::Eof) => break,
                Err(e) => panic!("{file}: {e}"),
            }
        }
        assert_eq!(n, packets, "{file}");
    }
}

/// The `BITMAPINFOHEADER` of `bytes`' first stream: offset of `strf`'s
/// body.
fn strf_body(bytes: &[u8]) -> usize {
    bytes.windows(4).position(|w| w == b"strf").expect("strf") + 8
}

#[test]
fn sps_cropping_window_wins_over_a_coded_size_header() {
    // Rewrite biWidth / biHeight to the macroblock-aligned coded size
    // (80x64); the SPS crop still says the decoder emits 66x50.
    let mut bytes = read("h264_420p.avi");
    let b = strf_body(&bytes);
    bytes[b + 4..b + 8].copy_from_slice(&80i32.to_le_bytes());
    bytes[b + 8..b + 12].copy_from_slice(&64i32.to_le_bytes());
    let d = open_bytes(bytes);
    let p = &d.streams()[0].params;
    assert_eq!((p.width, p.height), (Some(66), Some(50)));
    assert_eq!(p.pixel_format, Some(Yuv420P));
}

#[test]
fn a_zero_rate_or_scale_states_no_frame_rate() {
    let mut bytes = read("vp9_420p.avi");
    let strh = bytes.windows(4).position(|w| w == b"strh").expect("strh") + 8;
    // AVISTREAMHEADER: fccType, fccHandler, dwFlags, wPriority +
    // wLanguage, dwInitialFrames, dwScale (offset 20), dwRate (24).
    bytes[strh + 20..strh + 24].copy_from_slice(&0u32.to_le_bytes());
    let d = open_bytes(bytes);
    assert_eq!(d.streams()[0].params.frame_rate, None);
}
