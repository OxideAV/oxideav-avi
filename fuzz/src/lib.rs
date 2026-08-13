//! Shared helpers for the oxideav-avi fuzz targets.
//!
//! Three pieces live here so the targets stay thin:
//!
//! * [`Recipe`] — a bounded byte reader that turns the fuzz input
//!   into a stream of small integers (never panics, never reads out
//!   of bounds; exhausted input yields zeros so every prefix of a
//!   crashing input is itself a valid recipe).
//! * [`MuxPlan`] / [`MuxPlan::decode`] / [`MuxPlan::run`] — a
//!   structure-aware, valid-by-construction mux recipe: streams x
//!   packets x [`AviKind`] x rec-cluster / index-layout / vprp /
//!   INFO / side-band options, exactly the option surface whose
//!   round-trip identity the muxer + demuxer pair guarantees.
//! * [`open_all_and_exercise`] — the demux-side battery: all three
//!   open front doors, a bounded packet drain, both seek paths, and
//!   every index-adjacent / header-field accessor, mirroring (and
//!   extending) the in-tree round-394 deterministic harness.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer as _, MediaType, Muxer,
    Packet, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::AviDemuxer;
use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit, VprpConfig};
use oxideav_avi::stream_format::RgbQuad;

// ---------------------------------------------------------------------------
// Recipe reader
// ---------------------------------------------------------------------------

/// Bounded byte reader over the fuzz input. Reads past the end yield
/// `0`, so shrinking a crashing input never changes its meaning
/// mid-parse.
pub struct Recipe<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Recipe<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Recipe { data, pos: 0 }
    }
    pub fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }
    pub fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }
    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    /// `true` with probability `num/256`.
    pub fn chance(&mut self, num: u16) -> bool {
        u16::from(self.u8()) < num
    }
    /// Remaining unread bytes (used by the mutation target to derive
    /// mutation positions after the fixture recipe is consumed).
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos.min(self.data.len())..]
    }
}

// ---------------------------------------------------------------------------
// Codec registry + stream shapes
// ---------------------------------------------------------------------------

/// Synthetic codec_id <-> tag mappings, mirroring the crate's own
/// integration tests: the container only needs the registry to
/// translate FourCC / wFormatTag both ways, no real codec factory.
pub fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("mjpeg")).tag(CodecTag::fourcc(b"MJPG")));
    reg
}

pub fn video_stream(index: u32, width: u32, height: u32, fps: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.frame_rate = Some(Rational::new(fps as i64, 1));
    StreamInfo {
        index,
        time_base: TimeBase::new(1, i64::from(fps.max(1))),
        duration: None,
        start_time: Some(0),
        params,
    }
}

pub fn audio_stream(index: u32, sample_rate: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, i64::from(sample_rate.max(1))),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Cheap deterministic payload bytes.
pub fn payload(seed: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed.wrapping_mul(0x9E37_79B9).wrapping_add(17);
    for _ in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// In-memory writer (the muxer consumes its Box<dyn WriteSeek>)
// ---------------------------------------------------------------------------

/// Writer that shares its backing buffer so the finished file bytes
/// can be recovered after the muxer (which consumes the
/// `Box<dyn WriteSeek>`) is dropped.
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Cursor<Vec<u8>>>>);

impl SharedBuf {
    pub fn into_bytes(self) -> Vec<u8> {
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

// ---------------------------------------------------------------------------
// Structure-aware mux plan
// ---------------------------------------------------------------------------

/// One packet the plan will write: `(payload, keyframe)`.
pub type SentPacket = (Vec<u8>, bool);

/// A decoded, valid-by-construction mux recipe.
pub struct MuxPlan {
    pub kind: AviKind,
    pub streams: Vec<StreamInfo>,
    pub is_video: Vec<bool>,
    /// Per-stream packet list, in per-stream order.
    pub sent: Vec<Vec<SentPacket>>,
    // Option knobs (primitives so the plan stays Clone-free and the
    // AviMuxOptions builder is materialised once, at run time).
    rec_cluster_packets: Option<u32>,
    rec_cluster_bytes: Option<u32>,
    strl_std_index: Option<usize>,
    mid_movi_index: Option<u32>,
    synth_idx1: bool,
    super_index_capacity: Option<usize>,
    padding_granularity: Option<u32>,
    top_level_junk: Option<u32>,
    info_name: bool,
    top_level_info: bool,
    cset: bool,
    disp: bool,
    vprp_preset: u8,
    stream_names: bool,
    field2_stream0: bool,
    indexed_video: Option<(u16, usize)>,
    /// After writing video packet `i` on stream 0, emit a palette
    /// change (bit 0) / text chunk (bit 1) when `i` is in the map.
    sideband_every: Option<u32>,
}

impl MuxPlan {
    /// Decode a plan from the recipe bytes. Every decoded plan is
    /// valid by construction: the muxer must accept it and the
    /// demuxer must round-trip it.
    pub fn decode(r: &mut Recipe) -> MuxPlan {
        let kind = match r.u8() % 4 {
            0 => AviKind::Avi10,
            // Tiny ceilings force RIFF AVIX rolls every few packets
            // (RiffSegmentLimit clamps to a 4 KiB minimum itself).
            1 => AviKind::OpenDml(RiffSegmentLimit::Bytes(u64::from(r.u16()) & 0x3FFF)),
            2 => AviKind::OpenDml(RiffSegmentLimit::Bytes(64 * 1024)),
            _ => AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        };
        let opendml = matches!(kind, AviKind::OpenDml(_));

        // 1..=3 streams; stream 0 is video unless the audio-only bit
        // fires, extra streams alternate audio/video.
        let n_streams = 1 + (r.u8() % 3) as usize;
        let audio_only = r.chance(32);
        let mut streams = Vec::with_capacity(n_streams);
        let mut is_video = Vec::with_capacity(n_streams);
        for i in 0..n_streams {
            let video = !audio_only && (i == 0 || r.chance(128));
            if video {
                let w = 16 + u32::from(r.u8() % 64) * 4;
                let h = 16 + u32::from(r.u8() % 64) * 4;
                let fps = 1 + u32::from(r.u8() % 60);
                streams.push(video_stream(i as u32, w, h, fps));
            } else {
                let rates = [8_000u32, 11_025, 22_050, 44_100, 48_000];
                let rate = rates[(r.u8() as usize) % rates.len()];
                streams.push(audio_stream(i as u32, rate));
            }
            is_video.push(video);
        }

        // Packets: 0..=24 per stream. Video payloads are arbitrary
        // (including empty); audio payloads stay block-aligned (PCM
        // S16 stereo => 4-byte blocks) so the open()-time CBR
        // validator's premise holds. Audio packets are always
        // keyframes (PCM: every block is a sync point).
        let mut sent = Vec::with_capacity(n_streams);
        for (i, _) in streams.iter().enumerate() {
            let n_packets = (r.u8() % 25) as usize;
            let mut list: Vec<SentPacket> = Vec::with_capacity(n_packets);
            for p in 0..n_packets {
                if is_video[i] {
                    let len = (r.u8() % 193) as usize;
                    let kf = p == 0 || r.chance(96);
                    list.push((payload((i as u32) << 16 | p as u32, len), kf));
                } else {
                    let len = 4 * (1 + (r.u8() % 48) as usize);
                    list.push((
                        payload(0xA000_0000 | (i as u32) << 16 | p as u32, len),
                        true,
                    ));
                }
            }
            sent.push(list);
        }

        let has_video0 = is_video[0];
        MuxPlan {
            rec_cluster_packets: r.chance(64).then(|| 1 + u32::from(r.u8() % 8)),
            rec_cluster_bytes: r.chance(48).then(|| 256 + u32::from(r.u16()) % 4096),
            strl_std_index: (opendml && r.chance(96)).then(|| 1 + (r.u8() % 64) as usize),
            mid_movi_index: (opendml && r.chance(64)).then(|| 1 + u32::from(r.u8() % 8)),
            synth_idx1: opendml && r.chance(64),
            super_index_capacity: (opendml && r.chance(48)).then(|| 1 + (r.u8() % 16) as usize),
            padding_granularity: r.chance(48).then(|| 1u32 << (r.u8() % 18)),
            top_level_junk: r.chance(32).then(|| u32::from(r.u16()) % 1024),
            info_name: r.chance(64),
            top_level_info: r.chance(128),
            cset: r.chance(32),
            disp: r.chance(32),
            vprp_preset: if has_video0 { r.u8() % 5 } else { 0 },
            stream_names: r.chance(48),
            field2_stream0: has_video0 && r.chance(32),
            indexed_video: (has_video0 && r.chance(32)).then(|| {
                let bits: u16 = [1u16, 4, 8][(r.u8() as usize) % 3];
                let max = 1usize << bits;
                (bits, 1 + (r.u8() as usize) % max)
            }),
            sideband_every: (has_video0 && r.chance(48)).then(|| 1 + u32::from(r.u8() % 6)),
            kind,
            streams,
            is_video,
            sent,
        }
    }

    fn options(&self) -> AviMuxOptions {
        let mut o = AviMuxOptions::new();
        if let Some(n) = self.rec_cluster_packets {
            o = o.with_rec_cluster_packets(n);
        }
        if let Some(n) = self.rec_cluster_bytes {
            o = o.with_rec_cluster_bytes(n);
        }
        if let Some(n) = self.strl_std_index {
            o = o.with_strl_std_index(n);
        }
        if let Some(n) = self.mid_movi_index {
            o = o.with_mid_movi_index(0, n);
        }
        if self.synth_idx1 {
            o = o.synthesise_idx1_from_ix(true);
        }
        if let Some(n) = self.super_index_capacity {
            o = o.with_super_index_capacity(n);
        }
        if let Some(n) = self.padding_granularity {
            // The builder itself rejects out-of-range granularities.
            o = o.with_padding_granularity(n);
        }
        if let Some(n) = self.top_level_junk {
            o = o.with_top_level_junk(n);
        }
        if self.info_name {
            o = o.with_info(*b"INAM", "fuzz plan");
            o = o.with_top_level_info(self.top_level_info);
        }
        if self.cset {
            o = o.with_cset_fields(1252, 1, 9, 1);
        }
        if self.disp {
            o = o.with_disp_chunk(vec![0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB]);
        }
        match self.vprp_preset {
            1 => o = o.with_vprp(0, VprpConfig::ntsc()),
            2 => o = o.with_vprp(0, VprpConfig::pal()),
            3 => o = o.with_vprp(0, VprpConfig::secam()),
            4 => {
                o = o.with_vprp(
                    0,
                    VprpConfig::ntsc()
                        .with_aspect(16, 9)
                        .with_vertical_refresh_rate(60),
                )
            }
            _ => {}
        }
        if self.stream_names {
            for (i, _) in self.streams.iter().enumerate() {
                o = o.with_stream_name(i as u32, format!("fuzz stream {i}"));
            }
        }
        if self.field2_stream0 {
            o = o.with_field2_stream(0);
        }
        if let Some((bits, used)) = &self.indexed_video {
            let palette: Vec<RgbQuad> = (0..*used)
                .map(|k| RgbQuad {
                    blue: k as u8,
                    green: (k as u8).wrapping_mul(3),
                    red: (k as u8).wrapping_mul(7),
                    reserved: 0,
                })
                .collect();
            o = o.with_indexed_video(0, *bits, palette);
        }
        o
    }

    /// Execute the plan: mux every packet (round-robin interleave
    /// across streams) and return the finished file bytes. Every step
    /// must succeed — a decoded plan is valid by construction, so an
    /// `Err` anywhere is a muxer bug and the caller panics on it.
    pub fn run(&self) -> Vec<u8> {
        let buf = SharedBuf::default();
        let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
        let mut mux = oxideav_avi::muxer::open_avi(ws, &self.streams, self.kind, self.options())
            .expect("valid plan: open_avi must accept");
        mux.write_header().expect("valid plan: write_header");

        // Round-robin interleave, PCM audio pts advances by sample
        // blocks (payload_len / block_align), video pts by frames.
        let n = self.streams.len();
        let mut next: Vec<usize> = vec![0; n];
        let mut audio_pts: Vec<i64> = vec![0; n];
        loop {
            let mut wrote = false;
            for s in 0..n {
                let Some((data, kf)) = self.sent[s].get(next[s]) else {
                    continue;
                };
                let mut pkt = Packet::new(s as u32, self.streams[s].time_base, data.clone());
                if self.is_video[s] {
                    pkt.pts = Some(next[s] as i64);
                    pkt.flags.keyframe = *kf;
                    if self.field2_stream0 && s == 0 && data.len() >= 2 {
                        mux.set_field2_offset((data.len() / 2) as u32);
                    }
                } else {
                    pkt.pts = Some(audio_pts[s]);
                    audio_pts[s] += (data.len() / 4) as i64;
                    pkt.flags.keyframe = true;
                }
                mux.write_packet(&pkt).expect("valid plan: write_packet");
                if s == 0 && self.is_video[0] {
                    if let Some(every) = self.sideband_every {
                        if (next[s] as u32).is_multiple_of(every) {
                            // Minimal well-formed AVIPALCHANGE: first
                            // entry 0, 2 entries, flags 0, 2x4 bytes.
                            mux.write_palette_change(
                                0,
                                &[0, 2, 0, 0, 10, 20, 30, 0, 40, 50, 60, 0],
                            )
                            .expect("valid plan: write_palette_change");
                            mux.write_text_chunk(0, b"fuzz overlay")
                                .expect("valid plan: write_text_chunk");
                        }
                    }
                }
                next[s] += 1;
                wrote = true;
            }
            if !wrote {
                break;
            }
        }
        mux.write_trailer().expect("valid plan: write_trailer");
        drop(mux);
        buf.into_bytes()
    }

    /// Total packets across all streams.
    pub fn total_packets(&self) -> usize {
        self.sent.iter().map(Vec::len).sum()
    }
}

// ---------------------------------------------------------------------------
// Demux-side battery
// ---------------------------------------------------------------------------

/// Exercise every index-adjacent / header-field accessor plus a
/// bounded packet walk and both seek paths. Return values are
/// irrelevant — the battery only cares that nothing panics and
/// nothing allocates proportionally to an attacker-claimed size.
pub fn exercise(mut dmx: AviDemuxer) {
    let n_streams = dmx.streams().len() as u32;
    let _ = dmx.metadata().len();
    // File-global cross-checks + avih surfaces.
    let _ = dmx.super_index_target_violations();
    let _ = dmx.super_index_duration_violations();
    let _ = dmx.std_index_base_offset_violations();
    let _ = dmx.std_index_entry_count_violations();
    let _ = dmx.cbr_audio_block_alignment_violations();
    let _ = dmx.palette_change_flag_violations();
    let _ = dmx.declared_vs_actual_stream_count_mismatch();
    let _ = dmx.has_index_flag_violation();
    let _ = dmx.idx1_rec_list_entries().len();
    let _ = dmx.idx1_rec_list_count();
    let _ = dmx.junk_chunks().len();
    let _ = dmx.junk_total_bytes();
    let _ = dmx.movi_segments().len();
    let _ = dmx.movi_segment_count();
    let _ = dmx.dmlh_total_frames();
    let _ = dmx.dmlh_declared_body_size();
    let _ = dmx.dmlh_reserved().map(<[u8]>::len);
    let _ = dmx.avih_flags();
    let _ = dmx.avih_suggested_buffer_size_typed();
    let _ = dmx.avih_total_frames();
    let _ = dmx.avih_declared_stream_count();
    let _ = dmx.avih_movie_rect();
    let _ = dmx.avih_reserved();
    let _ = dmx.micro_sec_per_frame();
    let _ = dmx.max_bytes_per_sec();
    let _ = dmx.initial_frames();
    let _ = dmx.padding_granularity();
    let _ = dmx.digitization_date();
    let _ = dmx.smpte_timecode();
    let _ = dmx.info_for(*b"INAM");
    let _ = dmx.all_info_for("ISFT").len();
    for s in 0..n_streams.min(4) {
        let _ = dmx.super_index_entries(s);
        let _ = dmx.super_index_index_type(s);
        let _ = dmx.super_index_chunk_id(s);
        let _ = dmx.super_index_sub_type(s);
        let _ = dmx.super_index_is_2field(s);
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
        let _ = dmx.idx1_typed_flags_for_packet(s, 0);
        let _ = dmx.field2_offset_for_packet(s, 0);
        let _ = dmx.stream_palette(s).map(<[RgbQuad]>::len);
        let _ = dmx.effective_palette_after_changes(s, u32::MAX);
        let _ = dmx.effective_palette_at(s, 1);
        let _ = dmx.palette_change_packet_positions(s).len();
        let _ = dmx.palette_change_typed(s).len();
        let _ = dmx.palette_change_count(s);
        let _ = dmx.text_chunk_count(s);
        let _ = dmx.text_chunk_typed(s).len();
        let _ = dmx.stream_block_align(s);
        let _ = dmx.audio_is_vbr(s);
        let _ = dmx.stream_audio_strf(s);
        let _ = dmx.stream_channel_layout(s);
        let _ = dmx.stream_subformat(s);
        let _ = dmx.stream_valid_bits_per_sample(s);
        let _ = dmx.stream_bitfields_masks(s);
        let _ = dmx.stream_top_down(s);
        let _ = dmx.stream_size_image(s);
        let _ = dmx.stream_pixels_per_meter(s);
        let _ = dmx.stream_clr_used(s);
        let _ = dmx.stream_clr_important(s);
        let _ = dmx.stream_planes(s);
        let _ = dmx.stream_name(s);
        let _ = dmx.stream_header_data(s).map(<[u8]>::len);
        let _ = dmx.stream_frame_rect(s);
        let _ = dmx.stream_language(s);
        let _ = dmx.stream_initial_frames(s);
        let _ = dmx.stream_quality(s);
        let _ = dmx.stream_priority(s);
        let _ = dmx.stream_start(s);
        let _ = dmx.stream_handler(s);
        let _ = dmx.stream_suggested_buffer_size(s);
        let _ = dmx.stream_sample_size(s);
        let _ = dmx.stream_length(s);
        let _ = dmx.stream_flags_typed(s);
        let _ = dmx.stream_timebase(s);
        let _ = dmx.stream_fcc_type(s);
        let _ = dmx.vprp_field_descs(s).len();
        let _ = dmx.vprp_frame_aspect_ratio(s);
        let _ = dmx.vprp_video_format(s);
        let _ = dmx.vprp_video_standard(s);
        let _ = dmx.vprp_signal_shape(s);
    }
    // Bounded packet walk: mutants can declare absurd chunk chains;
    // 64 packets is plenty to cross every index structure.
    for _ in 0..64 {
        if dmx.next_packet().is_err() {
            break;
        }
    }
    let _ = dmx.seek_to_keyframe_strict_via_std_index(0, 5);
    let _ = dmx.seek_to_first_video_keyframe_after(0, 2);
    let _ = dmx.seek_to(0, 5);
    for _ in 0..8 {
        if dmx.next_packet().is_err() {
            break;
        }
    }
}

/// Run all three demuxer front doors over `bytes`, exercising
/// whatever opens. Whether a given input opens or errors is the
/// file's business — panicking is not.
pub fn open_all_and_exercise(bytes: &[u8]) {
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    if let Ok(dmx) = oxideav_avi::demuxer::open_avi(rs, &reg) {
        exercise(dmx);
    }
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    if let Ok(dmx) = oxideav_avi::demuxer::open_avi_lenient(rs, &reg) {
        exercise(dmx);
    }
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    if let Ok(dmx) = oxideav_avi::demuxer::open_avi_strict(rs, &reg) {
        exercise(dmx);
    }
}
