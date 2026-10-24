//! Uncompressed AVI video surfaces as the fleet's `rawvideo` codec with
//! an explicit pixel format, and its packets are always the packed
//! top-down rows the `rawvideo` decoder takes — whatever the on-wire
//! DIB orientation (`biHeight` sign) and DWORD row stride (VfW
//! `BITMAPINFOHEADER` §biHeight / "Calculating Surface Stride").

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_avi::demuxer::open_avi as demux_open;
use oxideav_avi::muxer::{AviKind, AviMuxOptions, open_avi};
use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, Muxer, Packet, PixelFormat,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);
impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

fn stream(codec: &str, fmt: Option<PixelFormat>, w: u32, h: u32) -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new(codec));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = fmt;
    p.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn mux(s: &StreamInfo, opts: AviMuxOptions, frames: &[Vec<u8>]) -> Vec<u8> {
    let sink = Sink::default();
    let ws: Box<dyn WriteSeek> = Box::new(sink.clone());
    let mut m = open_avi(ws, std::slice::from_ref(s), AviKind::Avi10, opts).unwrap();
    m.write_header().unwrap();
    for (i, f) in frames.iter().enumerate() {
        let mut p = Packet::new(0, s.time_base, f.clone());
        p.pts = Some(i as i64);
        p.flags.keyframe = true;
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    sink.0.lock().unwrap().get_ref().clone()
}

fn demux(bytes: Vec<u8>) -> (StreamInfo, Vec<Vec<u8>>) {
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = demux_open(rs, &reg).unwrap();
    let s = d.streams()[0].clone();
    let mut out = Vec::new();
    while let Ok(p) = d.next_packet() {
        out.push(p.data);
    }
    (s, out)
}

/// `biHeight` / `biBitCount` / `biCompression` of the first `strf`.
fn strf_fields(bytes: &[u8]) -> (i32, u16, [u8; 4]) {
    let i = bytes.windows(4).position(|w| w == b"strf").unwrap() + 8;
    let h = i32::from_le_bytes(bytes[i + 8..i + 12].try_into().unwrap());
    let bc = u16::from_le_bytes(bytes[i + 14..i + 16].try_into().unwrap());
    (h, bc, bytes[i + 16..i + 20].try_into().unwrap())
}

#[test]
fn bottom_up_padded_bi_rgb_becomes_packed_top_down_bgr24() {
    // The legacy `rgb24` mux path writes packets verbatim with a
    // positive biHeight: a bottom-up DIB. 5×3 at 24 bpp has 15 visible
    // bytes per row and a 16-byte DWORD stride.
    let (w, h) = (5u32, 3u32);
    let mut wire = Vec::new();
    for row in 0..h {
        // Row 0 on the wire is the BOTTOM picture row.
        wire.extend(std::iter::repeat_n(0x10 + row as u8, 15));
        wire.push(0); // stride padding
    }
    let bytes = mux(&stream("rgb24", None, w, h), AviMuxOptions::new(), &[wire]);
    assert_eq!(strf_fields(&bytes), (3, 24, [0; 4]));
    let (s, pkts) = demux(bytes);
    assert_eq!(s.params.codec_id.as_str(), "rawvideo");
    assert_eq!(s.params.pixel_format, Some(PixelFormat::Bgr24));
    let mut want = Vec::new();
    for row in (0..h).rev() {
        want.extend(std::iter::repeat_n(0x10 + row as u8, 15));
    }
    assert_eq!(pkts, vec![want]);
}

#[test]
fn rawvideo_mux_writes_top_down_dib_and_round_trips() {
    let (w, h) = (5u32, 3u32);
    let frame: Vec<u8> = (0..(w * h * 3) as u8).collect();
    let s = stream("rawvideo", Some(PixelFormat::Bgr24), w, h);
    let bytes = mux(&s, AviMuxOptions::new(), std::slice::from_ref(&frame));
    assert_eq!(strf_fields(&bytes), (-3, 24, [0; 4]));
    // On-wire chunk is DWORD-padded: 3 rows × 16 bytes.
    let movi = bytes.windows(4).position(|x| x == b"00db").unwrap();
    let n = u32::from_le_bytes(bytes[movi + 4..movi + 8].try_into().unwrap());
    assert_eq!(n, 48);
    let (d, pkts) = demux(bytes);
    assert_eq!(d.params.pixel_format, Some(PixelFormat::Bgr24));
    assert_eq!(pkts, vec![frame]);
}

#[test]
fn every_mapped_pixel_format_round_trips() {
    let (w, h) = (3u32, 2u32);
    for (fmt, bpp, bits, comp) in [
        (PixelFormat::Bgr24, 3, 24, [0u8; 4]),
        (PixelFormat::Bgrx, 4, 32, [0; 4]),
        (PixelFormat::Rgb555Le, 2, 16, [0; 4]),
        (PixelFormat::Rgb565Le, 2, 16, [3, 0, 0, 0]),
        (PixelFormat::Bgr565Le, 2, 16, [3, 0, 0, 0]),
        (PixelFormat::Rgbx, 4, 32, [3, 0, 0, 0]),
        (PixelFormat::Xrgb, 4, 32, [3, 0, 0, 0]),
        (PixelFormat::Rgba, 4, 32, *b"RGBA"),
    ] {
        let frame: Vec<u8> = (0..(w * h) as usize * bpp)
            .map(|i| i as u8 ^ 0x5A)
            .collect();
        let s = stream("rawvideo", Some(fmt), w, h);
        let bytes = mux(&s, AviMuxOptions::new(), std::slice::from_ref(&frame));
        let (hf, bc, c) = strf_fields(&bytes);
        assert_eq!((bc, c), (bits, comp), "{fmt:?}");
        // DIBs are written top-down; the FourCC keeps a positive height.
        assert_eq!(hf, if comp == *b"RGBA" { 2 } else { -2 }, "{fmt:?}");
        let (d, pkts) = demux(bytes);
        assert_eq!(d.params.codec_id.as_str(), "rawvideo", "{fmt:?}");
        assert_eq!(d.params.pixel_format, Some(fmt));
        assert_eq!(d.params.tag, Some(CodecTag::fourcc(&comp)));
        assert_eq!(pkts, vec![frame], "{fmt:?}");
    }
}

#[test]
fn rawvideo_without_a_mappable_format_is_refused() {
    let s = stream("rawvideo", Some(PixelFormat::Yuv420P), 4, 4);
    let ws: Box<dyn WriteSeek> = Box::new(Sink::default());
    assert!(
        open_avi(
            ws,
            std::slice::from_ref(&s),
            AviKind::Avi10,
            AviMuxOptions::new()
        )
        .is_err()
    );
    let s = stream("rawvideo", None, 4, 4);
    let ws: Box<dyn WriteSeek> = Box::new(Sink::default());
    assert!(
        open_avi(
            ws,
            std::slice::from_ref(&s),
            AviKind::Avi10,
            AviMuxOptions::new()
        )
        .is_err()
    );
}
