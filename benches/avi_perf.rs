//! Criterion benchmarks for the AVI container hot paths (round 415).
//!
//! Everything runs on synthetic in-memory fixtures (`Cursor<Vec<u8>>`)
//! so results are deterministic, allocation-bound rather than
//! disk-bound, and the suite is committable with no fixture files.
//!
//! Covered paths:
//!   * `mux/…`   — full muxer write path: `write_header` + N packet
//!     appends + index build + `write_trailer` finalize, for both the
//!     AVI 1.0 envelope and the OpenDML 2.0 multi-`RIFF` envelope
//!     (1 MiB segment ceiling → several `RIFF AVIX` continuations).
//!   * `open/…`  — demuxer `open()`: header-suite parse, `idx1` /
//!     `indx`+`ix##` index ingestion, keyframe-map build.
//!   * `walk/…`  — the `next_packet` chunk walk draining every packet,
//!     over the `idx1` (AVI 1.0) and OpenDML multi-segment layouts.
//!   * `seek/…`  — keyframe seek: the `idx1`-backed `seek_to` path and
//!     the OpenDML `ix##` std-index path (`idx1` FourCC blanked so the
//!     demuxer must use the std-index tables).

use std::hint::black_box;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer as _, MediaType, Packet,
    PixelFormat, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::AviDemuxer;
use oxideav_avi::muxer::{open_with_kind, AviKind, RiffSegmentLimit};

/// Frames per fixture. 1500 video + 1500 audio packets = 3000 packet
/// chunks per file — enough for index-build and walk costs to dominate
/// constant overhead while keeping a full Criterion pass bounded.
const FRAMES: usize = 1500;
/// Synthetic video payload bytes per frame (even → no pad byte).
const VIDEO_PAYLOAD: usize = 2048;
/// Synthetic audio payload bytes per packet: 480 stereo S16 samples.
const AUDIO_PAYLOAD: usize = 1920;
/// Audio samples per packet (payload / block_align = 1920 / 4).
const AUDIO_SAMPLES_PER_PKT: i64 = 480;
/// Video keyframe cadence: every 8th frame is flagged as a keyframe.
const KEYFRAME_EVERY: usize = 8;

/// Writer that shares its backing buffer so the finished file bytes
/// can be recovered after the muxer (which consumes the
/// `Box<dyn WriteSeek>`) is dropped. Fixture building only — the
/// timed mux loops write into a plain `Cursor` to keep the measured
/// path free of lock overhead.
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

/// Two-stream fixture shape: MJPG video (stream 0) + PCM S16 stereo
/// audio (stream 1) — the classic capture-hardware AVI layout.
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

/// Registry that resolves `MJPG` ↔ `"mjpeg"` for the demuxer's
/// forward `resolve_tag` direction (synthetic — no codec involved).
fn fixture_registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("mjpeg")).tag(CodecTag::fourcc(b"MJPG")));
    reg
}

/// Deterministic pseudo-random payload (LCG), `len` bytes, seeded per
/// packet so every chunk body is distinct.
fn payload(seed: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut s = seed.wrapping_mul(0x9E37_79B9).wrapping_add(0x85EB_CA6B);
    for _ in 0..len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

/// Pre-built packet load shared by every mux iteration: interleaved
/// video/audio, video keyframe every [`KEYFRAME_EVERY`] frames.
fn build_packets(streams: &[StreamInfo; 2]) -> Vec<Packet> {
    let mut pkts = Vec::with_capacity(FRAMES * 2);
    for i in 0..FRAMES {
        let mut v = Packet::new(0, streams[0].time_base, payload(i as u32, VIDEO_PAYLOAD));
        v.pts = Some(i as i64);
        v.flags.keyframe = i % KEYFRAME_EVERY == 0;
        pkts.push(v);

        let mut a = Packet::new(
            1,
            streams[1].time_base,
            payload(0x8000_0000 | i as u32, AUDIO_PAYLOAD),
        );
        a.pts = Some(i as i64 * AUDIO_SAMPLES_PER_PKT);
        a.flags.keyframe = true;
        pkts.push(a);
    }
    pkts
}

/// Byte budget for the output buffer preallocation.
fn out_capacity() -> usize {
    FRAMES * (VIDEO_PAYLOAD + AUDIO_PAYLOAD + 64) + 65536
}

/// Run the full mux write path into the given writer.
fn mux_into(ws: Box<dyn WriteSeek>, kind: AviKind, streams: &[StreamInfo; 2], packets: &[Packet]) {
    let mut mux = open_with_kind(ws, streams, kind).expect("mux open");
    mux.write_header().expect("write_header");
    for p in packets {
        mux.write_packet(p).expect("write_packet");
    }
    mux.write_trailer().expect("write_trailer");
}

/// Build finished file bytes for the demux-side benches (setup only).
fn mux_fixture_bytes(kind: AviKind, streams: &[StreamInfo; 2], packets: &[Packet]) -> Vec<u8> {
    let shared = SharedBuf::default();
    mux_into(Box::new(shared.clone()), kind, streams, packets);
    shared.into_bytes()
}

/// Open the concrete demuxer over the given file bytes.
fn open_demuxer(bytes: Vec<u8>, reg: &CodecRegistry) -> AviDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_avi::demuxer::open_avi(rs, reg).expect("demux open")
}

/// Blank the `idx1` FourCC in-place so the demuxer treats the chunk as
/// unknown and seeks must use the OpenDML `ix##` std-index tables.
fn strip_idx1(bytes: &mut [u8]) {
    let pos = bytes
        .windows(4)
        .position(|w| w == b"idx1")
        .expect("idx1 present in fixture");
    bytes[pos..pos + 4].copy_from_slice(b"JUNK");
}

const OPENDML_1MIB: AviKind = AviKind::OpenDml(RiffSegmentLimit::Bytes(1024 * 1024));

fn bench_mux(c: &mut Criterion) {
    let streams = fixture_streams();
    let packets = build_packets(&streams);
    let total_bytes: u64 = packets.iter().map(|p| p.data.len() as u64).sum();

    let mut g = c.benchmark_group("mux");
    g.sample_size(20);
    g.throughput(Throughput::Bytes(total_bytes));
    g.bench_function("avi10_3000pkts", |b| {
        b.iter(|| {
            let ws: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::with_capacity(out_capacity())));
            mux_into(ws, AviKind::Avi10, &streams, &packets);
        });
    });
    g.bench_function("opendml_1mib_segments_3000pkts", |b| {
        b.iter(|| {
            let ws: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::with_capacity(out_capacity())));
            mux_into(ws, OPENDML_1MIB, &streams, &packets);
        });
    });
    g.finish();
}

fn bench_open(c: &mut Criterion) {
    let streams = fixture_streams();
    let packets = build_packets(&streams);
    let reg = fixture_registry();
    let avi10 = mux_fixture_bytes(AviKind::Avi10, &streams, &packets);
    let opendml = mux_fixture_bytes(OPENDML_1MIB, &streams, &packets);

    let mut g = c.benchmark_group("open");
    g.sample_size(30);
    g.bench_function("avi10", |b| {
        b.iter_batched(
            || avi10.clone(),
            |bytes| black_box(open_demuxer(bytes, &reg)),
            BatchSize::LargeInput,
        );
    });
    g.bench_function("opendml", |b| {
        b.iter_batched(
            || opendml.clone(),
            |bytes| black_box(open_demuxer(bytes, &reg)),
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

fn bench_walk(c: &mut Criterion) {
    let streams = fixture_streams();
    let packets = build_packets(&streams);
    let reg = fixture_registry();
    let avi10 = mux_fixture_bytes(AviKind::Avi10, &streams, &packets);
    let opendml = mux_fixture_bytes(OPENDML_1MIB, &streams, &packets);
    let total_bytes: u64 = packets.iter().map(|p| p.data.len() as u64).sum();

    let drain = |mut dmx: AviDemuxer| {
        let mut n = 0usize;
        let mut bytes = 0usize;
        loop {
            match dmx.next_packet() {
                Ok(p) => {
                    n += 1;
                    bytes += p.data.len();
                }
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("walk error: {e}"),
            }
        }
        assert_eq!(n, FRAMES * 2);
        bytes
    };

    let mut g = c.benchmark_group("walk");
    g.sample_size(20);
    g.throughput(Throughput::Bytes(total_bytes));
    g.bench_function("avi10_3000pkts", |b| {
        b.iter_batched(
            || open_demuxer(avi10.clone(), &reg),
            |dmx| black_box(drain(dmx)),
            BatchSize::LargeInput,
        );
    });
    g.bench_function("opendml_3000pkts", |b| {
        b.iter_batched(
            || open_demuxer(opendml.clone(), &reg),
            |dmx| black_box(drain(dmx)),
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

fn bench_seek(c: &mut Criterion) {
    let streams = fixture_streams();
    let packets = build_packets(&streams);
    let reg = fixture_registry();
    let avi10 = mux_fixture_bytes(AviKind::Avi10, &streams, &packets);
    let mut opendml = mux_fixture_bytes(OPENDML_1MIB, &streams, &packets);
    // Force the std-index path: without idx1 the demuxer must walk the
    // OpenDML ix## tables.
    strip_idx1(&mut opendml);

    // 32 video-stream seek targets spread across the whole file.
    let targets: Vec<i64> = (0..32).map(|j| (j * 47) % FRAMES as i64).collect();

    let mut g = c.benchmark_group("seek");
    g.sample_size(30);

    let mut dmx_idx1 = open_demuxer(avi10, &reg);
    g.bench_function("idx1_32_seeks", |b| {
        b.iter(|| {
            let mut landed_sum = 0i64;
            for &t in &targets {
                landed_sum += dmx_idx1.seek_to(0, t).expect("idx1 seek");
            }
            black_box(landed_sum)
        });
    });

    let mut dmx_std = open_demuxer(opendml, &reg);
    g.bench_function("stdindex_32_seeks", |b| {
        b.iter(|| {
            let mut landed_sum = 0i64;
            for &t in &targets {
                landed_sum += dmx_std.seek_to(0, t).expect("std-index seek");
            }
            black_box(landed_sum)
        });
    });
    g.finish();
}

criterion_group!(benches, bench_mux, bench_open, bench_walk, bench_seek);
criterion_main!(benches);
