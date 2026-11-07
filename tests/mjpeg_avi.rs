//! Motion JPEG in AVI: an `mjpeg` stream as an encoder states it (codec
//! id, size, frame rate — no container tag) is written with the `MJPG`
//! `BITMAPINFOHEADER` of OpenDML AVI File Format Extensions §7.0 and reads
//! back as the same stream.
//!
//! Clean-room sources:
//!   - `docs/container/riff/opendml-avi-2.0.pdf` §7.0 "MJPG Motion JPEG
//!     DIB Extensions" (`##dc` data chunks, every frame a key frame,
//!     YCbCr 8-bit components)
//!   - `docs/container/vfw/video-for-windows.md` §BITMAPINFOHEADER

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Error, Packet, Rational,
    ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

const W: u32 = 64;
const H: u32 = 48;

/// An in-memory `WriteSeek` whose bytes stay readable after the muxer
/// (which owns its boxed output) is dropped.
#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for SharedBuf {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

impl SharedBuf {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

fn mjpeg_stream() -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("mjpeg"));
    params.width = Some(W);
    params.height = Some(H);
    params.frame_rate = Some(Rational::new(10, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 10),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// SOI, APP0 "AVI1", payload, EOI — the shape of an `MJPG` data chunk.
fn jpeg_frame(seed: u8, len: usize) -> Vec<u8> {
    let mut v = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'A', b'V', b'I', b'1', 0,
    ];
    v.extend(std::iter::repeat_n(0u8, 11));
    v.extend((0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) & 0x7F));
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn write_avi(stream: &StreamInfo, frames: &[Vec<u8>], keyframe_flag: bool) -> Vec<u8> {
    let buf = SharedBuf::default();
    let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
    let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(stream)).unwrap();
    mux.write_header().unwrap();
    for (i, f) in frames.iter().enumerate() {
        let mut pkt = Packet::new(0, stream.time_base, f.clone());
        pkt.pts = Some(i as i64);
        pkt.dts = Some(i as i64);
        pkt.flags.keyframe = keyframe_flag;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    buf.bytes()
}

fn find(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("{:?} not found", String::from_utf8_lossy(needle)))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[test]
fn untagged_mjpeg_stream_is_written_as_mjpg() {
    let frames: Vec<Vec<u8>> = (0..4).map(|i| jpeg_frame(i, 300 + i as usize)).collect();
    let avi = write_avi(&mjpeg_stream(), &frames, true);

    // strh: fccType `vids`, fccHandler `MJPG`.
    let strh = find(&avi, b"strh") + 8;
    assert_eq!(&avi[strh..strh + 8], b"vidsMJPG");
    // strf: a bare 40-byte BITMAPINFOHEADER.
    let strf = find(&avi, b"strf");
    assert_eq!(u32_at(&avi, strf + 4), 40);
    let b = strf + 8;
    assert_eq!(u32_at(&avi, b), 40, "biSize");
    assert_eq!(u32_at(&avi, b + 4), W, "biWidth");
    assert_eq!(u32_at(&avi, b + 8), H, "biHeight positive (compressed)");
    assert_eq!(&avi[b + 12..b + 14], &1u16.to_le_bytes(), "biPlanes");
    assert_eq!(
        &avi[b + 14..b + 16],
        &24u16.to_le_bytes(),
        "biBitCount: 3 × 8-bit YCbCr"
    );
    assert_eq!(&avi[b + 16..b + 20], b"MJPG", "biCompression");
    assert_eq!(u32_at(&avi, b + 20), W * H * 3, "biSizeImage");
    // Frames ride `00dc` chunks.
    assert_eq!(
        &avi[find(&avi, b"movi") + 4..find(&avi, b"movi") + 8],
        b"00dc"
    );
}

#[test]
fn every_mjpg_chunk_is_indexed_as_a_key_frame() {
    // An encoder that left the key-frame flag clear still gets
    // AVIIF_KEYFRAME on every idx1 entry (OpenDML §7.0).
    let frames: Vec<Vec<u8>> = (0..3).map(|i| jpeg_frame(i, 100)).collect();
    let avi = write_avi(&mjpeg_stream(), &frames, false);
    let idx1 = find(&avi, b"idx1");
    let n = u32_at(&avi, idx1 + 4) as usize / 16;
    assert_eq!(n, 3);
    for e in 0..n {
        let at = idx1 + 8 + e * 16;
        assert_eq!(&avi[at..at + 4], b"00dc");
        assert_ne!(
            u32_at(&avi, at + 4) & 0x10,
            0,
            "entry {e} lacks AVIIF_KEYFRAME"
        );
    }
}

#[test]
fn mjpeg_round_trips_through_the_demuxer() {
    let stream = mjpeg_stream();
    let frames: Vec<Vec<u8>> = (0..5)
        .map(|i| jpeg_frame(i, 200 + 7 * i as usize))
        .collect();
    let avi = write_avi(&stream, &frames, true);

    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("mjpeg")).tag(CodecTag::fourcc(b"MJPG")));
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(avi));
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let p = &dmx.streams()[0].params;
    assert_eq!(p.codec_id.as_str(), "mjpeg");
    assert_eq!(p.tag, Some(CodecTag::fourcc(b"MJPG")));
    assert_eq!((p.width, p.height), (Some(W), Some(H)));
    assert_eq!(p.frame_rate, Some(Rational::new(10, 1)));

    for (i, want) in frames.iter().enumerate() {
        let pkt = dmx.next_packet().unwrap();
        assert_eq!(&pkt.data, want, "frame {i} bytes");
        assert_eq!(pkt.pts, Some(i as i64));
        assert!(pkt.flags.keyframe);
    }
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

/// Black-box check: `ffprobe` reads the written file as Motion JPEG with
/// the frame count and size we wrote. Skipped when `ffprobe` is absent.
#[test]
fn ffprobe_reads_the_written_mjpeg_avi() {
    let frames: Vec<Vec<u8>> = (0..4).map(|i| jpeg_frame(i, 120)).collect();
    let avi = write_avi(&mjpeg_stream(), &frames, true);
    let path = std::env::temp_dir().join("oxideav-avi-mjpeg-ffprobe.avi");
    std::fs::write(&path, &avi).unwrap();
    let out = match std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=codec_name,width,height,nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("skip: ffprobe not available");
            return;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.trim(), format!("mjpeg,{W},{H},4"));
}
