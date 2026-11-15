//! The container does its own block I/O: what it asks of the caller's
//! reader / writer is a handful of large transfers, not several small
//! ones per chunk — and what it writes is unchanged by that.

use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use oxideav_avi::demuxer::open_avi;
use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};
use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, Muxer, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

/// An in-memory device that counts the calls made on it.
#[derive(Clone, Default)]
struct Device {
    bytes: Arc<Mutex<Cursor<Vec<u8>>>>,
    /// (reads, writes, seeks)
    calls: Arc<Mutex<(usize, usize, usize)>>,
}

impl Device {
    fn calls(&self) -> (usize, usize, usize) {
        *self.calls.lock().unwrap()
    }
    fn len(&self) -> usize {
        self.bytes.lock().unwrap().get_ref().len()
    }
    fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().get_ref().clone()
    }
}

impl Read for Device {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.calls.lock().unwrap().0 += 1;
        self.bytes.lock().unwrap().read(b)
    }
}
impl Write for Device {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.calls.lock().unwrap().1 += 1;
        self.bytes.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Seek for Device {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.calls.lock().unwrap().2 += 1;
        self.bytes.lock().unwrap().seek(p)
    }
}

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg
}

fn stream() -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.width = Some(64);
    params.height = Some(48);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn payload(i: usize, len: usize) -> Vec<u8> {
    (0..len).map(|b| (b * 31 + i * 7) as u8).collect()
}

/// Mux `sizes.len()` frames (frame `i` is `sizes[i]` bytes) into `dev`.
fn mux(dev: &Device, kind: AviKind, sizes: &[usize]) {
    let s = stream();
    let ws: Box<dyn WriteSeek> = Box::new(dev.clone());
    let mut m =
        oxideav_avi::muxer::open_avi(ws, std::slice::from_ref(&s), kind, AviMuxOptions::new())
            .unwrap();
    m.write_header().unwrap();
    for (i, &len) in sizes.iter().enumerate() {
        let mut pkt = Packet::new(0, s.time_base, payload(i, len));
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = i % 10 == 0;
        m.write_packet(&pkt).unwrap();
    }
    m.write_trailer().unwrap();
}

fn drain(dev: &Device) -> Vec<(i64, usize, bool)> {
    let rs: Box<dyn ReadSeek> = Box::new(dev.clone());
    let mut d = open_avi(rs, &registry()).unwrap();
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => {
                assert_eq!(p.data, payload(out.len(), p.data.len()));
                out.push((p.pts.unwrap(), p.data.len(), p.flags.keyframe));
            }
            Err(Error::Eof) => return out,
            Err(e) => panic!("{e}"),
        }
    }
}

const KINDS: [AviKind; 2] = [
    AviKind::Avi10,
    AviKind::OpenDml(RiffSegmentLimit::Bytes(256 * 1024)),
];

#[test]
fn small_chunks_cost_block_transfers_not_calls_per_chunk() {
    // 20 000 frames of 40..=103 bytes: ~1.6 MiB with headers and index.
    let sizes: Vec<usize> = (0..20_000).map(|i| 40 + i % 64).collect();
    for kind in KINDS {
        let dev = Device::default();
        mux(&dev, kind, &sizes);
        let blocks = dev.len() / (64 * 1024) + 1;
        let (_, writes, seeks) = dev.calls();
        // Block writes, plus a few per segment for the size patches.
        assert!(
            writes < 4 * blocks + 200,
            "{writes} device writes for {blocks} blocks ({kind:?})"
        );
        assert!(seeks < 400, "{seeks} device seeks ({kind:?})");

        dev.bytes.lock().unwrap().set_position(0);
        let before = dev.calls();
        let got = drain(&dev);
        assert_eq!(got.len(), sizes.len());
        for (i, &(pts, len, key)) in got.iter().enumerate() {
            assert_eq!((pts, len, key), (i as i64, sizes[i], i % 10 == 0));
        }
        let (reads, _, seeks) = dev.calls();
        let (reads, seeks) = (reads - before.0, seeks - before.2);
        // open() (header walk, index, the OpenDML ix## scan of movi)
        // and the whole packet walk together.
        assert!(
            reads < 6 * blocks + 100,
            "{reads} device reads for {blocks} blocks ({kind:?})"
        );
        assert!(seeks < 6 * blocks + 100, "{seeks} device seeks ({kind:?})");
    }
}

#[test]
fn large_frames_are_transferred_once_and_directly() {
    // 60 frames of 150 000 bytes: 9 MB.
    let sizes = vec![150_000usize; 60];
    for kind in KINDS {
        let dev = Device::default();
        mux(&dev, kind, &sizes);
        let (_, writes, _) = dev.calls();
        // One gathered write per frame (header + body); the test device
        // has no vectored write, so the header goes separately: two.
        assert!(writes < 2 * sizes.len() + 200, "{writes} device writes");

        // Count the bytes the device hands out while reading it back.
        struct Meter(Device, Arc<Mutex<usize>>);
        impl Read for Meter {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.read(b)?;
                *self.1.lock().unwrap() += n;
                Ok(n)
            }
        }
        impl Seek for Meter {
            fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
                self.0.seek(p)
            }
        }
        dev.bytes.lock().unwrap().set_position(0);
        let fetched = Arc::new(Mutex::new(0usize));
        let rs: Box<dyn ReadSeek> = Box::new(Meter(dev.clone(), fetched.clone()));
        let d = open_avi(rs, &registry()).unwrap();
        // open() does not read the frames: headers, index, and for
        // OpenDML a few kilobytes at each chunk header of the ix## scan.
        let at_open = *fetched.lock().unwrap();
        assert!(at_open < dev.len() / 10, "open() fetched {at_open} bytes");
        drop(d);

        dev.bytes.lock().unwrap().set_position(0);
        let before = dev.calls().0;
        assert_eq!(drain(&dev).len(), sizes.len());
        let reads = dev.calls().0 - before;
        // A frame body is read in a few growing transfers straight
        // into the packet (8, 16, 32 … KiB, as `read_to_end` sizes
        // them), never through the block.
        assert!(reads < 12 * sizes.len() + 200, "{reads} device reads");
    }
}

#[test]
fn bytes_reach_the_output_at_header_flush_trailer_and_drop() {
    let s = stream();
    let dev = Device::default();
    let ws: Box<dyn WriteSeek> = Box::new(dev.clone());
    let mut m = oxideav_avi::muxer::open_avi(
        ws,
        std::slice::from_ref(&s),
        AviKind::Avi10,
        AviMuxOptions::new(),
    )
    .unwrap();
    m.write_header().unwrap();
    let after_header = dev.len();
    assert!(after_header > 0, "the header is on the device");
    let mut written = 0;
    for i in 0..10 {
        let mut pkt = Packet::new(0, s.time_base, payload(i, 100));
        pkt.pts = Some(i as i64);
        m.write_packet(&pkt).unwrap();
        written += 108;
    }
    // Ten small chunks are still held in the block …
    assert_eq!(dev.len(), after_header);
    // … until the caller asks for them.
    m.flush().unwrap();
    assert_eq!(dev.len(), after_header + written);
    let tail = dev.snapshot();
    assert_eq!(&tail[after_header..after_header + 4], b"00dc");

    // Dropping without a trailer loses nothing that was written.
    let mut pkt = Packet::new(0, s.time_base, payload(10, 100));
    pkt.pts = Some(10);
    m.write_packet(&pkt).unwrap();
    drop(m);
    assert_eq!(dev.len(), after_header + written + 108);
}
