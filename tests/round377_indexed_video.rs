//! Round-377: indexed (palettised) video DIB write side — the
//! write-side complement of the round-355 baseline-DIB color-table read
//! surface.
//!
//! Per the RIFF MCI reference §"Interpreting the Color Table", a DIB
//! with `biBitCount` of 1/4/8 is indexed and carries an `RGBQUAD` color
//! table after the 40-byte BITMAPINFOHEADER. The demuxer parses that
//! table via `stream_palette`. Before round-377 the muxer always
//! advertised 24-bpp (no table), so an indexed strf could only be
//! produced by a hand-built fixture. `AviMuxOptions::with_indexed_video`
//! closes the round-trip: the muxer emits `biBitCount`, `biClrUsed =
//! palette.len()`, and the color table verbatim, and the demuxer reads
//! the palette back entry-for-entry.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_with_options, AviKind, AviMuxOptions};
use oxideav_avi::stream_format::RgbQuad;

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    // rgb24 → the BI_RGB all-zero FourCC the muxer uses for indexed DIBs.
    reg.register(CodecInfo::new(CodecId::new("rgb24")));
    reg
}

fn rgb_stream(width: u32, height: u32) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("rgb24"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.frame_rate = Some(Rational::new(25, 1));
    params.tag = Some(CodecTag::fourcc(&[0, 0, 0, 0]));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn mux(path: &std::path::Path, stream: &StreamInfo, frames: usize, opts: AviMuxOptions) {
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux =
        open_with_options(ws, std::slice::from_ref(stream), AviKind::Avi10, opts).unwrap();
    mux.write_header().unwrap();
    for i in 0..frames {
        let mut pkt = Packet::new(0, stream.time_base, vec![(i as u8).wrapping_add(1); 64]);
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
}

fn synth_palette(n: usize) -> Vec<RgbQuad> {
    (0..n)
        .map(|i| RgbQuad {
            blue: (i * 3) as u8,
            green: (i * 5 + 1) as u8,
            red: (i * 7 + 2) as u8,
            reserved: 0,
        })
        .collect()
}

#[test]
fn indexed_8bpp_palette_roundtrips() {
    let stream = rgb_stream(32, 32);
    let pal = synth_palette(256);
    let opts = AviMuxOptions::new().with_indexed_video(0, 8, pal.clone());

    let tmp = std::env::temp_dir().join("oxideav-avi-r377-idx8.avi");
    mux(&tmp, &stream, 3, opts);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &registry()).unwrap();

    let got = dmx.stream_palette(0).expect("palette present");
    assert_eq!(got.len(), 256);
    assert_eq!(
        got,
        pal.as_slice(),
        "8-bpp palette round-trips entry-for-entry"
    );

    // Metadata key surfaces the entry count.
    let meta = dmx.metadata().to_vec();
    let entries = meta
        .iter()
        .find(|(k, _)| k == "avi:vids.0.palette_entries")
        .map(|(_, v)| v.clone());
    assert_eq!(entries.as_deref(), Some("256"));

    // Frames still demux out of the movi list.
    let mut n = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => n += 1,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(n, 3);
}

#[test]
fn indexed_4bpp_partial_palette_roundtrips() {
    let stream = rgb_stream(16, 16);
    // 4-bpp max is 16 entries; use a partial 10-entry table (biClrUsed
    // = 10, not the depth maximum).
    let pal = synth_palette(10);
    let opts = AviMuxOptions::new().with_indexed_video(0, 4, pal.clone());

    let tmp = std::env::temp_dir().join("oxideav-avi-r377-idx4.avi");
    mux(&tmp, &stream, 2, opts);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &registry()).unwrap();

    let got = dmx.stream_palette(0).expect("palette present");
    assert_eq!(got.len(), 10, "partial palette honoured via biClrUsed");
    assert_eq!(got, pal.as_slice());
}

#[test]
fn indexed_1bpp_two_color_roundtrips() {
    let stream = rgb_stream(8, 8);
    let pal = vec![
        RgbQuad {
            blue: 0,
            green: 0,
            red: 0,
            reserved: 0,
        },
        RgbQuad {
            blue: 255,
            green: 255,
            red: 255,
            reserved: 0,
        },
    ];
    let opts = AviMuxOptions::new().with_indexed_video(0, 1, pal.clone());

    let tmp = std::env::temp_dir().join("oxideav-avi-r377-idx1.avi");
    mux(&tmp, &stream, 1, opts);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &registry()).unwrap();

    let got = dmx.stream_palette(0).expect("palette present");
    assert_eq!(got.len(), 2);
    assert_eq!(got, pal.as_slice());
}

#[test]
fn default_video_has_no_palette() {
    // Without the override the muxer advertises 24-bpp → no color table.
    let stream = rgb_stream(32, 32);
    let tmp = std::env::temp_dir().join("oxideav-avi-r377-idx-none.avi");
    mux(&tmp, &stream, 2, AviMuxOptions::new());

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &registry()).unwrap();
    assert!(dmx.stream_palette(0).is_none());
}

#[test]
fn last_indexed_call_per_stream_wins() {
    let stream = rgb_stream(16, 16);
    let pal_a = synth_palette(4);
    let pal_b = synth_palette(8);
    // Two calls on stream 0: the second replaces the first.
    let opts = AviMuxOptions::new()
        .with_indexed_video(0, 8, pal_a)
        .with_indexed_video(0, 8, pal_b.clone());

    let tmp = std::env::temp_dir().join("oxideav-avi-r377-idx-lastwins.avi");
    mux(&tmp, &stream, 1, opts);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &registry()).unwrap();
    let got = dmx.stream_palette(0).expect("palette present");
    assert_eq!(got, pal_b.as_slice());
}
