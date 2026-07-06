//! Round 394 — deterministic fuzz hardening for the index walkers.
//!
//! Not a coverage-guided fuzzer: a reproducible in-tree mutation
//! harness that hammers `open_avi` / `open_avi_lenient`, the packet
//! walker, the seek paths, and the whole index-accessor battery with
//! (a) random byte flips, (b) truncations, and (c) targeted
//! corruption of the `idx1` / `indx` / `ix##` structures on three
//! writer-shaped fixtures (legacy AVI 1.0, multi-segment OpenDML,
//! compact in-`strl` standard index). Every mutant must be handled
//! without a panic and without unbounded allocation; whether it opens
//! or errors cleanly is the file's business.
//!
//! The harness already earns its keep: the pre-round-394
//! `read_body_bounded` committed a `vec![0; cb]` allocation for the
//! DECLARED chunk size — a mutated `cb` near `0xFFFFFFFF` requested
//! ~4 GiB before the read could fail (now grown in 64 KiB steps).

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer as _, MediaType,
    Muxer as _, Packet, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::AviDemuxer;
use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG"));
    reg.register(info);
    reg
}

fn video_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(64);
    params.height = Some(64);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn audio_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn payload(seed: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed.wrapping_mul(0x9E37_79B9).wrapping_add(17);
    for _ in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 24) as u8);
    }
    out
}

/// Deterministic xorshift64* PRNG so every failure reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mux_fixture(tag: &str, kind: AviKind, opts: AviMuxOptions, frames: usize) -> Vec<u8> {
    // Unique per call: the three #[test]s build the same fixture set
    // concurrently, so a shared filename races create/read/remove.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let unique = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let streams = [video_stream(0), audio_stream(1)];
    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r394-fuzz-{tag}-{pid}-{unique}.avi"));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(ws, &streams, kind, opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..frames {
            let mut v = Packet::new(0, streams[0].time_base, payload(i as u32, 220));
            v.pts = Some(i as i64);
            v.flags.keyframe = i % 3 == 0;
            mux.write_packet(&v).unwrap();
            if i == 1 {
                // Side-band records so the sideband scanners get
                // mutated coverage too.
                let _ = mux.write_palette_change(0, &[0, 2, 0, 0, 1, 2, 3, 0, 4, 5, 6, 0]);
                let _ = mux.write_text_chunk(0, b"overlay");
            }
            let mut a = Packet::new(1, streams[1].time_base, payload(0xB000 + i as u32, 96));
            a.pts = Some(i as i64 * 24);
            a.flags.keyframe = true;
            mux.write_packet(&a).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn fixtures() -> Vec<Vec<u8>> {
    vec![
        // Legacy AVI 1.0 with idx1 + side-band chunks + strn/strd.
        mux_fixture(
            "avi10",
            AviKind::Avi10,
            AviMuxOptions::default()
                .with_stream_name(0, "fuzz video")
                .with_stream_header_data(0, [0x5A; 7]),
            10,
        ),
        // Multi-segment OpenDML: indx super-indexes + per-segment ix##.
        mux_fixture(
            "odml",
            AviKind::OpenDml(RiffSegmentLimit::Bytes(2 * 1024)),
            AviMuxOptions::default(),
            10,
        ),
        // Compact in-strl standard index.
        mux_fixture(
            "sstd",
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::default().with_strl_std_index(32),
            10,
        ),
    ]
}

/// Exercise every index-adjacent accessor + a bounded packet walk +
/// one strict std-index seek attempt. Return value is irrelevant —
/// the harness only cares that nothing panics.
fn exercise(mut dmx: AviDemuxer) {
    let n_streams = dmx.streams().len() as u32;
    let _ = dmx.metadata().len();
    let _ = dmx.super_index_target_violations();
    let _ = dmx.super_index_duration_violations();
    let _ = dmx.std_index_base_offset_violations();
    let _ = dmx.std_index_entry_count_violations();
    let _ = dmx.cbr_audio_block_alignment_violations();
    let _ = dmx.palette_change_flag_violations();
    let _ = dmx.declared_vs_actual_stream_count_mismatch();
    let _ = dmx.has_index_flag_violation();
    let _ = dmx.idx1_rec_list_entries().len();
    let _ = dmx.junk_chunk_count();
    let _ = dmx.movi_segments().len();
    let _ = dmx.dmlh_total_frames();
    for s in 0..n_streams.min(4) {
        let _ = dmx.super_index_entries(s);
        let _ = dmx.super_index_index_type(s);
        let _ = dmx.super_index_chunk_id(s);
        let _ = dmx.super_index_sub_type(s);
        let _ = dmx.super_index_longs_per_entry(s);
        let _ = dmx.super_index_reserved(s);
        let _ = dmx.super_index_segment_durations(s);
        let _ = dmx.std_index_base_offsets(s);
        let _ = dmx.std_index_chunk_ids(s);
        let _ = dmx.std_index_index_types(s);
        let _ = dmx.std_index_declared_entry_counts(s);
        let _ = dmx.std_index_reserved(s);
        let _ = dmx.keyframe_indexed_packet_count(s);
        let _ = dmx.packet_is_keyframe(s, 0);
        let _ = dmx.packet_is_keyframe(s, 3);
        let _ = dmx.field2_offset_for_packet(s, 0);
        let _ = dmx.stream_palette(s);
        let _ = dmx.effective_palette_after_changes(s, u32::MAX);
        let _ = dmx.effective_palette_at(s, 1);
        let _ = dmx.palette_change_packet_positions(s);
        let _ = dmx.text_chunk_count(s);
    }
    // Bounded packet walk: mutants can declare absurd chunk chains;
    // 64 packets is plenty to cross every index structure.
    for _ in 0..64 {
        match dmx.next_packet() {
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = dmx.seek_to_keyframe_strict_via_std_index(0, 5);
    let _ = dmx.seek_to(0, 5);
    for _ in 0..8 {
        if dmx.next_packet().is_err() {
            break;
        }
    }
}

fn open_and_exercise(bytes: &[u8]) {
    let reg = registry();
    // Strict open first (must not panic; error is fine)...
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.to_vec()));
    if let Ok(dmx) = oxideav_avi::demuxer::open_avi(rs, &reg) {
        exercise(dmx);
    }
    // ...then lenient (reaches further into malformed headers).
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.to_vec()));
    if let Ok(dmx) = oxideav_avi::demuxer::open_avi_lenient(rs, &reg) {
        exercise(dmx);
    }
}

#[test]
fn random_byte_flips_never_panic() {
    let fixtures = fixtures();
    let mut rng = Rng(0x0DDB_1A5E_5BAD_C0DE);
    for round in 0..240 {
        let base = &fixtures[round % fixtures.len()];
        let mut mutant = base.clone();
        let flips = 1 + rng.below(8);
        for _ in 0..flips {
            let pos = rng.below(mutant.len());
            let val = (rng.next() & 0xFF) as u8;
            mutant[pos] ^= val.max(1);
        }
        open_and_exercise(&mutant);
    }
}

#[test]
fn truncations_never_panic() {
    for base in fixtures() {
        // ~40 evenly spaced truncation points per fixture, plus the
        // pathological head lengths.
        let step = (base.len() / 40).max(1);
        let mut cuts: Vec<usize> = (0..base.len()).step_by(step).collect();
        cuts.extend([0, 1, 7, 8, 11, 12, 13, 24, 87, 88, 89]);
        for cut in cuts {
            if cut > base.len() {
                continue;
            }
            open_and_exercise(&base[..cut]);
        }
    }
}

#[test]
fn targeted_index_corruption_never_panics() {
    // Overwrite the bytes right after each idx1 / indx / ix## header
    // with hostile patterns: huge counts, all-ones sizes, zeroed
    // strides — the exact fields the index walkers arithmetic on.
    let patterns: [&[u8]; 4] = [
        &[0xFF; 16],
        &[0x00; 16],
        &[0xFF, 0xFF, 0xFF, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF],
        &[0x02, 0x00, 0x01, 0x01, 0xFF, 0xFF, 0xFF, 0xFF],
    ];
    for base in fixtures() {
        let mut targets: Vec<usize> = Vec::new();
        for k in 0..base.len().saturating_sub(4) {
            let tag = &base[k..k + 4];
            if tag == b"idx1" || tag == b"indx" || (tag[0] == b'i' && tag[1] == b'x') {
                targets.push(k);
            }
        }
        for &t in &targets {
            for pat in patterns {
                let mut mutant = base.clone();
                // Corrupt the size field (t+4) and the structure head.
                let start = t + 4;
                let end = (start + pat.len()).min(mutant.len());
                mutant[start..end].copy_from_slice(&pat[..end - start]);
                open_and_exercise(&mutant);
                // Also corrupt the structure body just past the header.
                let mut mutant = base.clone();
                let start = (t + 8).min(mutant.len());
                let end = (start + pat.len()).min(mutant.len());
                if start < end {
                    mutant[start..end].copy_from_slice(&pat[..end - start]);
                }
                open_and_exercise(&mutant);
            }
        }
    }
}
