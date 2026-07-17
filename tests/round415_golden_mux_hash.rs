//! Round-415 golden-hash pins for the muxer write path.
//!
//! The perf round's muxer optimizations must keep the emitted bytes
//! IDENTICAL — these tests mux a fixed synthetic two-stream fixture
//! (MJPG video + PCM S16 audio, deterministic LCG payloads) through
//! both envelope variants and pin an FNV-1a-64 hash of the finished
//! file. Any byte drift in header suite, chunk layout, index build,
//! or finalize patching changes the hash and fails the pin.
//!
//! If a FUTURE feature round deliberately changes the default output
//! shape, re-pin the constants in the same commit and say so in the
//! commit message — this gate is for accidental drift, not a freeze.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, CodecTag, MediaType, Packet, PixelFormat, Rational, SampleFormat,
    StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{open_with_kind, AviKind, RiffSegmentLimit};

/// FNV-1a 64-bit over the whole file image.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Writer that shares its backing buffer so the finished bytes can be
/// recovered after the muxer (which consumes the `Box<dyn WriteSeek>`)
/// is dropped.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Cursor<Vec<u8>>>>);

impl SharedBuf {
    fn into_bytes(self) -> Vec<u8> {
        Arc::try_unwrap(self.0)
            .expect("muxer dropped; sole owner")
            .into_inner()
            .expect("lock poisoned")
            .into_inner()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl Seek for SharedBuf {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(pos)
    }
}

fn fixture_streams() -> [StreamInfo; 2] {
    let mut vparams =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    vparams.media_type = MediaType::Video;
    vparams.width = Some(640);
    vparams.height = Some(480);
    vparams.pixel_format = Some(PixelFormat::Yuv420P);
    vparams.frame_rate = Some(Rational::new(25, 1));
    let video = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params: vparams,
    };

    let mut aparams = CodecParameters::audio(CodecId::new("pcm_s16le"));
    aparams.channels = Some(2);
    aparams.sample_rate = Some(48_000);
    aparams.sample_format = Some(SampleFormat::S16);
    let audio = StreamInfo {
        index: 1,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: aparams,
    };

    [video, audio]
}

/// Deterministic pseudo-random payload (LCG), `len` bytes.
fn payload(seed: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut s = seed.wrapping_mul(0x9E37_79B9).wrapping_add(0x85EB_CA6B);
    for _ in 0..len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

/// Mux 64 interleaved video/audio frame pairs (video keyframe every
/// 8th; one odd-length video payload to pin pad-byte handling) and
/// return the finished file bytes.
fn mux_fixture(kind: AviKind) -> Vec<u8> {
    let streams = fixture_streams();
    let shared = SharedBuf::default();
    let ws: Box<dyn WriteSeek> = Box::new(shared.clone());
    let mut mux = open_with_kind(ws, &streams, kind).expect("mux open");
    mux.write_header().expect("write_header");
    for i in 0..64u32 {
        // Frame 13 gets an odd payload length so the idx1 offsets and
        // pad-byte emission stay pinned too.
        let vlen = if i == 13 { 2047 } else { 2048 };
        let mut v = Packet::new(0, streams[0].time_base, payload(i, vlen));
        v.pts = Some(i as i64);
        v.flags.keyframe = i % 8 == 0;
        mux.write_packet(&v).expect("video packet");

        let mut a = Packet::new(1, streams[1].time_base, payload(0x8000_0000 | i, 1920));
        a.pts = Some(i as i64 * 480);
        a.flags.keyframe = true;
        mux.write_packet(&a).expect("audio packet");
    }
    mux.write_trailer().expect("write_trailer");
    drop(mux);
    shared.into_bytes()
}

#[test]
fn golden_avi10_mux_bytes_pinned() {
    let bytes = mux_fixture(AviKind::Avi10);
    assert_eq!(
        format!("{:016x}", fnv1a64(&bytes)),
        "3fcb61241ffea949", // re-pin deliberately, never to paper over drift
        "AVI 1.0 muxer output drifted (len={})",
        bytes.len()
    );
}

#[test]
fn golden_opendml_mux_bytes_pinned() {
    // 64 KiB segment ceiling → several RIFF AVIX continuations, so the
    // segment-roll + ix## flush + super-index patch paths are pinned.
    let bytes = mux_fixture(AviKind::OpenDml(RiffSegmentLimit::Bytes(64 * 1024)));
    assert_eq!(
        format!("{:016x}", fnv1a64(&bytes)),
        "ffbb2711f93620c0", // re-pin deliberately, never to paper over drift
        "OpenDML muxer output drifted (len={})",
        bytes.len()
    );
}
