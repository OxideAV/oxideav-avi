//! AVI (RIFF/AVI) muxer.
//!
//! Output layout (AVI 1.0):
//! ```text
//! RIFF(AVI )
//!   LIST(hdrl)
//!     avih                  ← main header
//!     LIST(strl) × N
//!       strh                ← stream header
//!       strf                ← BITMAPINFOHEADER or WAVEFORMATEX
//!       [ indx ]            ← OpenDML 2.0 super-index (AviKind::OpenDml)
//!   LIST(movi)              ← packet chunks: NNdc / NNwb / NNdb
//!   idx1                    ← legacy index (written in write_trailer)
//! ```
//!
//! For [`AviKind::OpenDml`] the muxer rolls additional `RIFF AVIX`
//! segments after the primary `RIFF AVI ` envelope when the running
//! file size approaches the configured [`RiffSegmentLimit`]. Each
//! continuation contains a single `LIST movi` carrying further
//! packet chunks. The primary segment carries an `indx` super-index
//! in the first stream's `strl`; its entries are back-patched in
//! `write_trailer` with each segment's `qwOffset` / `dwSize` /
//! `dwDuration`.
//!
//! - The public `Muxer` API is codec-agnostic. The only codec-aware
//!   call site is `packaging::build_strf`, which errors with
//!   `Unsupported` at `open()` for codecs the supplied
//!   `CodecResolver` can't resolve to a wire FourCC / wFormatTag.
//!   `write_packet` never branches on codec.

use std::io::{Seek, SeekFrom, Write};

use oxideav_core::{Error, Packet, Result, StreamInfo};
use oxideav_core::{Muxer, WriteSeek};

use crate::packaging::{build_strf, StrfEntry};
use crate::riff::{
    begin_list, finish_chunk, write_chunk, write_chunk_header, AVI_FORM, LIST, RIFF,
};

/// Per-RIFF-segment byte ceiling for [`AviKind::OpenDml`] output.
///
/// AVI 1.0 conventionally caps each RIFF at 1 GiB to leave headroom
/// for the legacy index and for tools that scan with 32-bit offsets;
/// OpenDML 2.0 raises the per-segment ceiling but keeps the same
/// per-RIFF accounting. Tests use `Bytes(small_value)` to force
/// segmentation on tiny fixtures.
#[derive(Clone, Copy, Debug)]
pub enum RiffSegmentLimit {
    /// 1 GiB per RIFF (the AVI 1.0 / OpenDML 2.0 convention).
    OneGiB,
    /// Custom byte ceiling. Clamped to a minimum of 4 KiB so the first
    /// segment has room for `hdrl` + at least one frame.
    Bytes(u64),
}

impl RiffSegmentLimit {
    /// Resolved byte ceiling. `Bytes(n)` is clamped to `max(n, 4096)`.
    pub fn bytes(self) -> u64 {
        match self {
            RiffSegmentLimit::OneGiB => 1024 * 1024 * 1024,
            RiffSegmentLimit::Bytes(n) => n.max(4096),
        }
    }
}

/// AVI envelope variant.
///
/// `Avi10` is the legacy single-`RIFF AVI ` form; `OpenDml` is the
/// OpenDML 2.0 multi-`RIFF` form per spec/06 §6.1. The OpenDML
/// envelope's primary RIFF carries the `indx` super-index in the
/// first video stream's `strl`; per-stream `ix##` chunks are
/// intentionally omitted (spec/06 §6.1: "from the codec's POV, the
/// super-index is informational"). Decoders that need per-frame
/// random access inside an OpenDML continuation can fall back to
/// linear walking — the demuxer in this crate already does so.
#[derive(Clone, Copy, Debug, Default)]
pub enum AviKind {
    /// AVI 1.0: single top-level `RIFF AVI ` chunk. Output must stay
    /// below 2 GiB; exceeding that returns `Error::Unsupported` from
    /// `write_packet`.
    #[default]
    Avi10,
    /// OpenDML 2.0: a primary `RIFF AVI ` followed by zero or more
    /// `RIFF AVIX` continuations. Each segment is bounded by the
    /// supplied [`RiffSegmentLimit`].
    OpenDml(RiffSegmentLimit),
}

/// Optional muxer features beyond the core envelope variant.
///
/// All fields default to off / disabled so existing callers (which
/// pass nothing) get the same byte output they did pre-round-3.
#[derive(Clone, Debug, Default)]
pub struct AviMuxOptions {
    /// When `Some(n)`, group every `n` consecutive packet chunks into
    /// a `LIST rec ` cluster inside `movi`. Per OpenDML 2.0 spec/06
    /// §"Stream Data ('movi' List)", `LIST rec ` clusters keep the
    /// per-cluster size manageable for files that grow past 1 GiB.
    /// Default `None` (no clustering). The minimum useful value is
    /// 2; `Some(0)` and `Some(1)` are treated as `None`.
    pub rec_cluster_packets: Option<u32>,
    /// When `Some(n)`, close the current `LIST rec ` cluster as soon
    /// as the packet that just landed pushes the cluster body past
    /// `n` bytes (round-4 P4). May be combined with
    /// [`Self::rec_cluster_packets`] (whichever cap fires first
    /// closes the cluster). `n < 256` is treated as `None`.
    pub rec_cluster_bytes: Option<u32>,
    /// Per-stream `vprp` populator (round-4 P2). Each entry is keyed
    /// by stream index; absent entries fall back to the round-3
    /// `FORMAT_UNKNOWN` / `STANDARD_UNKNOWN` defaults. Only emitted
    /// in `AviKind::OpenDml` mode.
    pub vprp_overrides: Vec<(u32, VprpConfig)>,
    /// Per-stream 2-field interlaced index opt-in (round-4 P1). When
    /// the listed stream is a video stream and the envelope is
    /// `AviKind::OpenDml`, the muxer emits `AVI_INDEX_2FIELD`
    /// super + standard indexes for it (12-byte std-index entries
    /// carrying `dwOffsetField2`). Encoders should call
    /// [`AviMuxer::set_field2_offset`] before each `write_packet`
    /// for an interlaced stream.
    pub field2_streams: Vec<u32>,
}

/// Per-stream override values for the OpenDML 2.0 `vprp` Video
/// Properties Header (round-4 P2). All fields are optional; a zero
/// value falls back to the round-3 default the muxer already emits
/// for that field.
///
/// Per OpenDML 2.0 §5.0 the spec defines four well-known
/// `(VideoFormatToken, VideoStandard)` pairs — the helpers
/// [`VprpConfig::ntsc`] / [`VprpConfig::pal`] / [`VprpConfig::secam`]
/// fill in the well-known refresh rates so callers don't have to
/// remember the table.
#[derive(Clone, Copy, Debug, Default)]
pub struct VprpConfig {
    /// `VideoFormatToken` per OpenDML §5.0 enum. 0 = FORMAT_UNKNOWN.
    pub video_format_token: u32,
    /// `VideoStandard` per OpenDML §5.0 enum. 0 = STANDARD_UNKNOWN.
    pub video_standard: u32,
    /// `dwVerticalRefreshRate` in Hz. 0 = use stream-derived fps.
    pub vertical_refresh_rate: u32,
    /// `dwFrameAspectRatio` packed `(X << 16) | Y`. 0 = 4:3 default.
    pub frame_aspect_ratio: u32,
    /// `nbFieldPerFrame` — 1 progressive, 2 interlaced. 0 = 1.
    pub nb_field_per_frame: u32,
}

/// OpenDML 2.0 §5.0 video-format token: `FORMAT_UNKNOWN`.
pub const VIDEO_FORMAT_UNKNOWN: u32 = 0;
/// OpenDML 2.0 §5.0 video-format token: PAL square-pixel.
pub const VIDEO_FORMAT_PAL_SQUARE: u32 = 1;
/// OpenDML 2.0 §5.0 video-format token: PAL CCIR 601.
pub const VIDEO_FORMAT_PAL_CCIR_601: u32 = 2;
/// OpenDML 2.0 §5.0 video-format token: NTSC square-pixel.
pub const VIDEO_FORMAT_NTSC_SQUARE: u32 = 3;
/// OpenDML 2.0 §5.0 video-format token: NTSC CCIR 601.
pub const VIDEO_FORMAT_NTSC_CCIR_601: u32 = 4;

/// OpenDML 2.0 §5.0 video-standard: `STANDARD_UNKNOWN`.
pub const VIDEO_STANDARD_UNKNOWN: u32 = 0;
/// OpenDML 2.0 §5.0 video-standard: PAL.
pub const VIDEO_STANDARD_PAL: u32 = 1;
/// OpenDML 2.0 §5.0 video-standard: NTSC.
pub const VIDEO_STANDARD_NTSC: u32 = 2;
/// OpenDML 2.0 §5.0 video-standard: SECAM.
pub const VIDEO_STANDARD_SECAM: u32 = 3;

impl VprpConfig {
    /// NTSC CCIR-601 preset: 60 Hz, interlaced, 4:3.
    pub fn ntsc() -> Self {
        Self {
            video_format_token: VIDEO_FORMAT_NTSC_CCIR_601,
            video_standard: VIDEO_STANDARD_NTSC,
            vertical_refresh_rate: 60,
            frame_aspect_ratio: (4u32 << 16) | 3,
            nb_field_per_frame: 2,
        }
    }
    /// PAL CCIR-601 preset: 50 Hz, interlaced, 4:3.
    pub fn pal() -> Self {
        Self {
            video_format_token: VIDEO_FORMAT_PAL_CCIR_601,
            video_standard: VIDEO_STANDARD_PAL,
            vertical_refresh_rate: 50,
            frame_aspect_ratio: (4u32 << 16) | 3,
            nb_field_per_frame: 2,
        }
    }
    /// SECAM preset: 50 Hz, interlaced, 4:3 (no SECAM token in §5.0).
    pub fn secam() -> Self {
        Self {
            video_format_token: VIDEO_FORMAT_UNKNOWN,
            video_standard: VIDEO_STANDARD_SECAM,
            vertical_refresh_rate: 50,
            frame_aspect_ratio: (4u32 << 16) | 3,
            nb_field_per_frame: 2,
        }
    }
    /// Builder: pin `nbFieldPerFrame`.
    pub fn with_nb_field_per_frame(mut self, n: u32) -> Self {
        self.nb_field_per_frame = n;
        self
    }
    /// Builder: pin `dwFrameAspectRatio` packed as `(X << 16) | Y`.
    pub fn with_frame_aspect_ratio(mut self, packed: u32) -> Self {
        self.frame_aspect_ratio = packed;
        self
    }
    /// Builder: pin `dwFrameAspectRatio` from `(X, Y)`.
    pub fn with_aspect(mut self, x: u32, y: u32) -> Self {
        self.frame_aspect_ratio = ((x & 0xFFFF) << 16) | (y & 0xFFFF);
        self
    }
}

impl AviMuxOptions {
    /// Convenience: build a default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder helper: enable `LIST rec ` clustering with `n` packets
    /// per cluster (`n` must be ≥ 2 to take effect).
    pub fn with_rec_cluster_packets(mut self, n: u32) -> Self {
        self.rec_cluster_packets = if n >= 2 { Some(n) } else { None };
        self
    }

    /// Builder helper: enable byte-budget `LIST rec ` clustering
    /// (round-4 P4) — close the cluster as soon as the body exceeds
    /// `n` bytes. May be combined with
    /// [`Self::with_rec_cluster_packets`]. `n < 256` is treated as
    /// no clustering.
    pub fn with_rec_cluster_bytes(mut self, n: u32) -> Self {
        self.rec_cluster_bytes = if n >= 256 { Some(n) } else { None };
        self
    }

    /// Builder helper: register a `vprp` override for `stream_index`
    /// (round-4 P2). Replaces any prior override for the same index.
    pub fn with_vprp(mut self, stream_index: u32, config: VprpConfig) -> Self {
        self.vprp_overrides.retain(|(i, _)| *i != stream_index);
        self.vprp_overrides.push((stream_index, config));
        self
    }

    /// Builder helper: mark `stream_index` as a 2-field interlaced
    /// stream (round-4 P1). Encoders must call
    /// [`AviMuxer::set_field2_offset`] before each `write_packet`
    /// for the stream so `dwOffsetField2` lands on the std-index
    /// entry.
    pub fn with_field2_stream(mut self, stream_index: u32) -> Self {
        if !self.field2_streams.contains(&stream_index) {
            self.field2_streams.push(stream_index);
        }
        self
    }
}

/// Bookkeeping for a single idx1 entry (legacy AVI 1.0 index).
#[derive(Clone, Copy, Debug)]
struct IndexEntry {
    ckid: [u8; 4],
    flags: u32,
    /// Offset from the start of the `movi` list body (see `idx1` format note).
    offset: u32,
    size: u32,
}

struct TrackState {
    stream: StreamInfo,
    entry: StrfEntry,
    /// 4-byte chunk FourCC used in movi for this stream (e.g. b"00dc").
    packet_fourcc: [u8; 4],
    /// Running packet count (used for avih.TotalFrames for the first video
    /// stream and length fields).
    packet_count: u32,
    /// Running total sample count for audio (frames for PCM, packets for VBR).
    sample_count: u64,
    /// Max chunk size seen so far (for strh.dwSuggestedBufferSize).
    max_chunk_size: u32,
    /// Max output bytes per packet (used for ffmpeg compatibility).
    total_bytes: u64,
    /// Per-segment OpenDML standard-index entries (one per packet in
    /// the current segment). Flushed into an `ix##` chunk at segment
    /// close. Always populated so the OpenDML emit path can decide
    /// whether to write `ix##` regardless of stream type.
    ix_entries: Vec<IxStdEntry>,
}

/// One AVISTDINDEX_ENTRY-shaped record for a packet inside the current
/// OpenDML segment's `ix##` chunk. Offsets are relative to the
/// enclosing `movi` LIST's first chunk header (`qwBaseOffset` in the
/// std-index header), which is what AVI 2.0 §"AVI Index Locations"
/// describes as the canonical reference point.
#[derive(Clone, Copy, Debug)]
struct IxStdEntry {
    /// Byte offset of the chunk **data** (just past its 8-byte header)
    /// from the segment's `qwBaseOffset`.
    dw_offset: u32,
    /// Payload size + keyframe-bit clear ⇒ keyframe; high bit set ⇒
    /// non-keyframe (delta).
    dw_size_with_flag: u32,
    /// `dwOffsetField2` per OpenDML 2.0 §3.0 "AVI Field Index Chunk"
    /// — `qwBaseOffset`-relative byte offset of the second field's
    /// first byte. Zero for default progressive entries; only used
    /// when the parent index has `bIndexSubType == AVI_INDEX_2FIELD`
    /// (round-4 P1).
    dw_offset_field2: u32,
}

/// Open an AVI muxer with the legacy single-`RIFF AVI ` envelope.
///
/// Per-stream wire tags come from each stream's
/// [`oxideav_core::CodecParameters::tag`] field — the demuxer sets
/// it from the source container at read-time, encoders set it via
/// `output_params()`. For codecs that haven't migrated yet the
/// muxer also accepts a printable FourCC hint in
/// `params.extradata[0..4]` and synthesises wFormatTag for the PCM
/// families directly from the codec id. Everything else returns
/// `Error::Unsupported` from `open()`.
///
/// To select the OpenDML 2.0 envelope, use [`open_with_kind`].
pub fn open(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    open_with_kind(output, streams, AviKind::Avi10)
}

/// Open an AVI muxer with an explicit envelope variant. See [`open`]
/// for the wire-tag resolution rules.
pub fn open_with_kind(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    kind: AviKind,
) -> Result<Box<dyn Muxer>> {
    open_with_options(output, streams, kind, AviMuxOptions::default())
}

/// Open an AVI muxer with full control over envelope variant and
/// per-feature options. See [`open`] for the wire-tag resolution
/// rules and [`AviMuxOptions`] for available toggles. Returns a
/// trait object — callers that need the concrete type to access
/// AVI-specific hooks (e.g. [`AviMuxer::set_field2_offset`]) should
/// use [`open_avi`] instead.
pub fn open_with_options(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    kind: AviKind,
    options: AviMuxOptions,
) -> Result<Box<dyn Muxer>> {
    let m = open_avi(output, streams, kind, options)?;
    Ok(Box::new(m))
}

/// Open an AVI muxer and return the concrete [`AviMuxer`] so callers
/// can access AVI-specific hooks like [`AviMuxer::set_field2_offset`]
/// (round-4 P1/P3) before invoking the standard
/// [`oxideav_core::Muxer`] methods.
pub fn open_avi(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    kind: AviKind,
    options: AviMuxOptions,
) -> Result<AviMuxer> {
    if streams.is_empty() {
        return Err(Error::invalid("avi muxer: need at least one stream"));
    }
    if streams.len() > 99 {
        // We use 2 ASCII *decimal* digits 00..99 for the chunk index.
        return Err(Error::unsupported(
            "avi muxer: > 99 streams not supported in legacy index",
        ));
    }
    let mut tracks = Vec::with_capacity(streams.len());
    for (i, s) in streams.iter().enumerate() {
        let entry = build_strf(&s.params)?;
        let packet_fourcc = packet_fourcc_for(i as u32, entry.chunk_suffix);
        tracks.push(TrackState {
            stream: s.clone(),
            entry,
            packet_fourcc,
            packet_count: 0,
            sample_count: 0,
            max_chunk_size: 0,
            total_bytes: 0,
            ix_entries: Vec::new(),
        });
    }
    Ok(AviMuxer {
        output,
        tracks,
        kind,
        options,
        riff_size_off: 0,
        movi_size_off: 0,
        movi_start_off: 0,
        index: Vec::new(),
        indx_entries_count_off: None,
        indx_entries_start_off: None,
        indx_entries_capacity: 0,
        indx_for_2field: false,
        dmlh_total_frames_off: None,
        segments: Vec::new(),
        current_segment_packets: 0,
        rec_open_size_off: None,
        rec_packets_in_cluster: 0,
        rec_bytes_in_cluster: 0,
        pending_field2_offset: None,
        header_written: false,
        trailer_written: false,
    })
}

fn packet_fourcc_for(index: u32, suffix: [u8; 2]) -> [u8; 4] {
    // 00dc-style: two ASCII decimal digits.
    let tens = (index / 10) as u8 + b'0';
    let ones = (index % 10) as u8 + b'0';
    [tens, ones, suffix[0], suffix[1]]
}

/// One closed-out RIFF segment in OpenDML mode. Used to back-patch
/// the `indx` super-index in `write_trailer`.
#[derive(Clone, Copy, Debug)]
struct SegmentRecord {
    /// File-absolute offset of this segment's `RIFF` 4-CC.
    riff_offset: u64,
    /// Total byte length of the segment (= 8 + dwSize, including any
    /// even-pad byte). Stored as the value to write into `dwSize` of
    /// the indx entry per spec/06 §6.1 ("dwSize is the byte count of
    /// the entire RIFF chunk including the 8-byte RIFF header").
    total_size: u64,
    /// Number of packets (frames) carried in this segment's `movi`
    /// LIST. Becomes `dwDuration` in the indx entry. We sum across
    /// all streams; for single-stream MagicYUV / video files this is
    /// the per-stream frame count, which matches OpenDML's intent.
    packet_count: u32,
}

/// Concrete AVI muxer. Returned by [`open_avi`] for callers that
/// need direct access to AVI-specific hooks like
/// [`AviMuxer::set_field2_offset`] (round-4 P1/P3). Implements
/// [`oxideav_core::Muxer`] for the usual write-header /
/// write-packet / write-trailer flow.
pub struct AviMuxer {
    output: Box<dyn WriteSeek>,
    tracks: Vec<TrackState>,
    kind: AviKind,
    options: AviMuxOptions,
    /// Offset of the current RIFF chunk's size field.
    riff_size_off: u64,
    /// Offset of the current movi LIST size field.
    movi_size_off: u64,
    /// Start offset of the current movi list body (i.e. of the
    /// `"movi"` form-type word). idx1 entries are offsets from this
    /// byte (specifically, from 4 bytes *before* the first chunk
    /// header, which lands on the `movi` form-type four-cc).
    movi_start_off: u64,
    /// Per-packet idx1 entries for the primary segment. Emitted in
    /// `write_trailer` regardless of `kind` so legacy AVI-1.0 readers
    /// can still seek inside the first segment.
    index: Vec<IndexEntry>,
    /// File offset of the `nEntriesInUse` field within the OpenDML
    /// `indx` super-index. `None` for `AviKind::Avi10`.
    indx_entries_count_off: Option<u64>,
    /// File offset of the first super-index entry slot. `None` for
    /// `AviKind::Avi10`.
    indx_entries_start_off: Option<u64>,
    /// Number of super-index slots reserved at header time. We can't
    /// pre-determine the actual segment count, so we reserve a
    /// generous fixed capacity; back-patching only writes
    /// `min(actual_segments, capacity)` slots.
    indx_entries_capacity: usize,
    /// True iff the `indx` super-index was stamped with
    /// `bIndexSubType = AVI_INDEX_2FIELD` per OpenDML 2.0 §3.0
    /// "Super Index Chunk" (round-4 P1).
    indx_for_2field: bool,
    /// File offset of the `dwTotalFrames` DWORD inside the
    /// `LIST odml dmlh` chunk. Back-patched in `write_trailer` once
    /// every packet has been written. `None` for `AviKind::Avi10`.
    dmlh_total_frames_off: Option<u64>,
    /// All closed-out segments in OpenDML mode. The primary segment
    /// is appended to this list when it's closed (i.e. when the next
    /// `write_packet` would push past the limit, or in
    /// `write_trailer`). Always empty for `AviKind::Avi10`.
    segments: Vec<SegmentRecord>,
    /// Number of packets written into the current open segment's
    /// `movi` LIST. Reset when a new segment is opened.
    current_segment_packets: u32,
    /// File offset of the `LIST rec ` size field for the currently
    /// open cluster (when [`AviMuxOptions::rec_cluster_packets`] is
    /// `Some`). `None` between clusters.
    rec_open_size_off: Option<u64>,
    /// Number of packets written into the currently-open `LIST rec `
    /// cluster. Reset to zero each time a new cluster is opened or
    /// the previous one is closed. Unused when no clustering is set.
    rec_packets_in_cluster: u32,
    /// Bytes (chunk header + body + pad) written into the currently
    /// open `LIST rec ` cluster body, used to enforce
    /// [`AviMuxOptions::rec_cluster_bytes`] (round-4 P4).
    rec_bytes_in_cluster: u64,
    /// Pending `dwOffsetField2` for the next `write_packet` call
    /// (round-4 P1/P3). Set via [`AviMuxer::set_field2_offset`]
    /// and consumed by the next `write_packet`.
    pending_field2_offset: Option<u32>,
    header_written: bool,
    trailer_written: bool,
}

/// Reserved slots in the OpenDML `indx` super-index. 256 slots is
/// 4 KiB of payload and lets a 1-GiB-segment OpenDML file index up
/// to 256 GiB, which covers everything users need without forcing
/// up-front segment-count knowledge. Files with more than 256
/// segments still mux correctly — the trailing entries simply don't
/// land in the super-index (the demuxer falls back to walking
/// `RIFF AVIX` continuations).
const OPENDML_SUPER_INDEX_CAPACITY: usize = 256;

impl Muxer for AviMuxer {
    fn format_name(&self) -> &str {
        "avi"
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Err(Error::other("avi muxer: write_header called twice"));
        }
        // Start outer RIFF list.
        self.riff_size_off = begin_list(self.output.as_mut(), &RIFF, &AVI_FORM)?;

        // hdrl LIST with avih + strl*.
        let hdrl_size_off = begin_list(self.output.as_mut(), &LIST, b"hdrl")?;
        let avih = build_avih(&self.tracks);
        write_chunk(self.output.as_mut(), b"avih", &avih)?;
        // For OpenDML, embed the super-index in the FIRST stream's strl
        // (typically video). For Avi10, no super-index.
        let want_indx = matches!(self.kind, AviKind::OpenDml(_));
        let want_vprp = want_indx; // emit `vprp` for video streams in OpenDML mode
                                   // Round-4 P1: when stream 0 is registered as 2-field, the
                                   // super-index for stream 0 must carry
                                   // `bIndexSubType = AVI_INDEX_2FIELD` per OpenDML 2.0 §3.0.
        let indx_is_2field = want_indx && self.options.field2_streams.contains(&0);
        self.indx_for_2field = indx_is_2field;
        for (i, t) in self.tracks.iter().enumerate() {
            let with_indx = want_indx && i == 0;
            let with_vprp = want_vprp && &t.entry.strh_type == b"vids";
            let vprp_override = self
                .options
                .vprp_overrides
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, c)| *c);
            let indx_2field_here = indx_is_2field && with_indx;
            let (indx_count_off, indx_entries_off) = write_strl(
                self.output.as_mut(),
                i as u32,
                t,
                with_indx,
                with_vprp,
                vprp_override,
                indx_2field_here,
            )?;
            if with_indx {
                self.indx_entries_count_off = indx_count_off;
                self.indx_entries_start_off = indx_entries_off;
                self.indx_entries_capacity = OPENDML_SUPER_INDEX_CAPACITY;
            }
        }
        // OpenDML 2.0 §5.0 "Source and Header Information Storage":
        // emit `LIST odml` carrying the `dmlh` extended header inside
        // `hdrl`. The single DWORD `dwTotalFrames` is back-patched in
        // `write_trailer` once we know the cross-segment frame count.
        if want_indx {
            let odml_size_off = begin_list(self.output.as_mut(), &LIST, b"odml")?;
            // dmlh body: a single DWORD dwTotalFrames (placeholder = 0;
            // back-patched in write_trailer).
            crate::riff::write_chunk_header(self.output.as_mut(), b"dmlh", 4)?;
            let dmlh_off = self.output.stream_position()?;
            self.output.write_all(&0u32.to_le_bytes())?;
            // dmlh body length is even (4) so no pad byte required.
            self.dmlh_total_frames_off = Some(dmlh_off);
            finish_chunk(self.output.as_mut(), odml_size_off)?;
        }
        finish_chunk(self.output.as_mut(), hdrl_size_off)?;

        // movi LIST — size patched in write_trailer (or when this segment
        // is closed in OpenDML mode).
        self.movi_size_off = begin_list(self.output.as_mut(), &LIST, b"movi")?;
        // movi_start_off points at the "movi" form-type FourCC — i.e. 4 bytes
        // after the size field. idx1 offsets are relative to this byte (+ 4 =
        // first chunk header). Per the AVI 1.0 spec, idx1 offsets may be
        // relative to either the start of the file OR the start of the movi
        // LIST body (the 'movi' FourCC). Most decoders heuristically detect
        // which — by convention, we make them relative to 'movi'.
        self.movi_start_off = self.movi_size_off + 4; // skip past size → 'movi' fourcc
        self.current_segment_packets = 0;
        self.rec_open_size_off = None;
        self.rec_packets_in_cluster = 0;
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("avi muxer: write_header not called"));
        }
        let idx = packet.stream_index as usize;
        if idx >= self.tracks.len() {
            return Err(Error::invalid(format!(
                "avi muxer: unknown stream index {idx}"
            )));
        }
        if packet.data.len() > u32::MAX as usize {
            return Err(Error::invalid("avi muxer: packet larger than 4 GiB"));
        }

        // OpenDML: roll a new RIFF AVIX segment if this packet would
        // push the current segment past the configured byte ceiling.
        // The check fires only after the segment already has at least
        // one packet — every segment must carry at least one frame.
        if let AviKind::OpenDml(limit) = self.kind {
            let projected = self.output.stream_position()?
                + 8 // chunk header
                + packet.data.len() as u64
                + (packet.data.len() & 1) as u64
                + 16 /* idx1 entry, only relevant in primary segment */;
            // Bytes already used in this segment, measured from RIFF start.
            let segment_start = self.riff_size_off - 4;
            let segment_used = projected.saturating_sub(segment_start);
            if self.current_segment_packets > 0 && segment_used > limit.bytes() {
                self.close_current_segment()?;
                self.open_avix_segment()?;
            }
        }

        // Optional `LIST rec ` clustering (OpenDML 2.0 spec/06 §"Stream
        // Data ('movi' List)"). Open a new cluster when we don't have
        // one; close+reopen when the current one has reached either
        // its packet-count cap or — when set — its byte budget. The
        // caps are independent: whichever fires first closes the
        // cluster (round-4 P4).
        let want_clustering =
            self.options.rec_cluster_packets.is_some() || self.options.rec_cluster_bytes.is_some();
        if want_clustering {
            // Bytes this packet would add: chunk header (8) + payload
            // + even-pad (the pad lives inside the LIST rec body too).
            let projected_packet_bytes =
                8u64 + packet.data.len() as u64 + (packet.data.len() & 1) as u64;
            let needs_close_for_packets = self
                .options
                .rec_cluster_packets
                .map(|n| self.rec_packets_in_cluster >= n)
                .unwrap_or(false);
            let needs_close_for_bytes = self
                .options
                .rec_cluster_bytes
                .map(|n| {
                    self.rec_packets_in_cluster > 0
                        && self.rec_bytes_in_cluster + projected_packet_bytes > n as u64
                })
                .unwrap_or(false);
            if self.rec_open_size_off.is_none() {
                self.open_rec_cluster()?;
            } else if needs_close_for_packets || needs_close_for_bytes {
                self.close_rec_cluster()?;
                self.open_rec_cluster()?;
            }
        }

        let fourcc = self.tracks[idx].packet_fourcc;
        // Record offset (relative to 'movi' fourcc) BEFORE writing the chunk.
        let chunk_off = self.output.stream_position()?;
        let rel_off_opt = chunk_off.checked_sub(self.movi_start_off);
        let size = packet.data.len() as u32;
        let flags = if packet.flags.keyframe {
            0x10 // AVIIF_KEYFRAME
        } else {
            0
        };

        // Round-4 P1/P3: consume any pending field-2 offset signalled
        // by `set_field2_offset`. Always consume so a stray hook on a
        // non-2-field stream can't leak onto the next packet.
        let pending_field2 = self.pending_field2_offset.take();
        let stream_is_2field = self.options.field2_streams.contains(&(idx as u32));

        // Stamp an `AVISTDINDEX_ENTRY`-shaped record for this packet
        // before the chunk is actually written: `dw_offset` is from the
        // segment's `qwBaseOffset` (= `movi_start_off + 4`, the first
        // chunk header inside the LIST) to the chunk *data* (= just
        // past its 8-byte header). We accumulate per-track and flush
        // them into an `ix##` chunk at segment close (`flush_ix_chunks`).
        if matches!(self.kind, AviKind::OpenDml(_)) {
            // qwBaseOffset = first chunk header inside movi
            //              = movi_start_off + 4 ('movi' fourcc width).
            let qw_base = self.movi_start_off + 4;
            // chunk-data offset = chunk_header + 8.
            let data_off = chunk_off + 8;
            if let Some(d) = data_off.checked_sub(qw_base) {
                if d <= u32::MAX as u64 {
                    let dw_size_with_flag = if packet.flags.keyframe {
                        size
                    } else {
                        size | 0x8000_0000
                    };
                    // dwOffsetField2 is qwBaseOffset-relative (per
                    // OpenDML 2.0 §3.0); the caller's value is
                    // payload-relative. Convert by adding `d`.
                    let dw_offset_field2 = if stream_is_2field {
                        match pending_field2 {
                            Some(payload_off) => {
                                let abs = d + payload_off as u64;
                                if abs <= u32::MAX as u64 {
                                    abs as u32
                                } else {
                                    0
                                }
                            }
                            None => 0,
                        }
                    } else {
                        0
                    };
                    self.tracks[idx].ix_entries.push(IxStdEntry {
                        dw_offset: d as u32,
                        dw_size_with_flag,
                        dw_offset_field2,
                    });
                }
            }
        }

        write_chunk(self.output.as_mut(), &fourcc, &packet.data)?;

        let t = &mut self.tracks[idx];
        t.packet_count += 1;
        if size > t.max_chunk_size {
            t.max_chunk_size = size;
        }
        t.total_bytes += size as u64;
        // Sample count: for audio with block_align, add the frame count;
        // otherwise one sample per packet.
        t.sample_count += sample_count_of_packet(&t.stream, &t.entry, size);

        self.current_segment_packets += 1;
        if self.options.rec_cluster_packets.is_some() || self.options.rec_cluster_bytes.is_some() {
            self.rec_packets_in_cluster += 1;
            // Track on-disk bytes added to the cluster body: 8-byte
            // chunk header + payload + even-pad.
            self.rec_bytes_in_cluster +=
                8u64 + packet.data.len() as u64 + (packet.data.len() & 1) as u64;
        }

        // idx1 entry — only meaningful for the primary segment in
        // OpenDML mode (idx1 offsets are 32-bit and relative to the
        // primary `movi` LIST, so chunks in `RIFF AVIX` continuations
        // can't be indexed by idx1). For Avi10 mode we always record.
        let in_primary_segment = self.segments.is_empty();
        if in_primary_segment {
            if let Some(rel_off) = rel_off_opt {
                if rel_off <= u32::MAX as u64 {
                    self.index.push(IndexEntry {
                        ckid: fourcc,
                        flags,
                        offset: rel_off as u32,
                        size,
                    });
                }
            }
        }

        // Enforce the 2 GiB ceiling for AVI 1.0 mode only — OpenDML
        // can grow arbitrarily large because it segments.
        if matches!(self.kind, AviKind::Avi10) {
            let cur = self.output.stream_position()?;
            if cur > (2 * 1024 * 1024 * 1024) - 1024 {
                return Err(Error::unsupported(
                    "avi muxer: file would exceed 2 GiB; use AviKind::OpenDml",
                ));
            }
        }

        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        if !self.header_written {
            return Err(Error::other("avi muxer: write_trailer before write_header"));
        }

        let in_primary_segment = self.segments.is_empty();

        // Close any open `LIST rec ` cluster so ix## (OpenDML) and idx1
        // (legacy) chunks land at the tail of `movi`, not nested inside
        // a cluster.
        if self.rec_open_size_off.is_some() {
            self.close_rec_cluster()?;
        }
        // OpenDML: flush `ix##` chunks at the tail of the current
        // segment's movi LIST before closing it. Mirrors the
        // close_current_segment path for the trailing partial segment.
        if matches!(self.kind, AviKind::OpenDml(_)) {
            self.flush_ix_chunks()?;
        }
        // Close movi LIST (patch its size).
        finish_chunk(self.output.as_mut(), self.movi_size_off)?;

        if in_primary_segment {
            // The whole file is one RIFF — write idx1 inside it (legacy
            // AVI 1.0 layout). This holds for both AviKind::Avi10 and
            // AviKind::OpenDml when only the primary segment was used.
            let idx_body = self.serialize_idx1();
            write_chunk(self.output.as_mut(), b"idx1", &idx_body)?;
            // Close outer RIFF.
            finish_chunk(self.output.as_mut(), self.riff_size_off)?;
            // Record the primary segment if we're in OpenDML mode so the
            // super-index back-patch can include it.
            if matches!(self.kind, AviKind::OpenDml(_)) {
                let total_size = self.output.stream_position()?;
                let riff_start = self.riff_size_off - 4;
                self.segments.push(SegmentRecord {
                    riff_offset: riff_start,
                    total_size: total_size - riff_start,
                    packet_count: self.current_segment_packets,
                });
            }
        } else {
            // OpenDML continuation: just close the AVIX RIFF. The
            // primary segment's idx1 was already written when its RIFF
            // was closed in `close_current_segment`.
            finish_chunk(self.output.as_mut(), self.riff_size_off)?;
            let total_size = self.output.stream_position()?;
            let riff_start = self.riff_size_off - 4;
            self.segments.push(SegmentRecord {
                riff_offset: riff_start,
                total_size: total_size - riff_start,
                packet_count: self.current_segment_packets,
            });
        }

        // Optionally patch avih.dwTotalFrames and strh.dwLength now that we
        // know the packet counts. These are located at well-known offsets
        // relative to the RIFF start.
        self.patch_post_counts()?;

        // For OpenDML, back-patch the indx super-index entries with each
        // segment's qwOffset / dwSize / dwDuration.
        if matches!(self.kind, AviKind::OpenDml(_)) {
            self.patch_super_index()?;
        }

        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

impl AviMuxer {
    /// Patch avih/strh length fields after the trailer is written. We know
    /// the exact offsets because we laid out the header deterministically.
    fn patch_post_counts(&mut self) -> Result<()> {
        // avih total_frames = max video stream packet_count (or first
        // stream if no video). strh dwLength = per-stream packet_count for
        // video, sample_count for audio.
        //
        // Layout we wrote (offsets are within the primary RIFF):
        //   "RIFF"(4) + size(4) + "AVI "(4)         — offset 0..12
        //   "LIST"(4) + size(4) + "hdrl"(4)         — offset 12..24
        //   "avih"(4) + size(4) + body(56)          — offset 24..88
        //     body[16] = TotalFrames                — file offset 48..52
        //   For each stream i:
        //     "LIST"(4) + size(4) + "strl"(4)       — strl LIST opener
        //     "strh"(4) + size(4) + body(56)        — strh
        //       body[32] = dwLength                 — strl_off + 20 + 32
        //       body[36] = dwSuggestedBufferSize    — strl_off + 20 + 36
        //     "strf"(4) + size(4) + body(N)
        //     [ "indx"(4) + size(4) + payload ]     — only if OpenDML & i==0
        let total_video_frames = self
            .tracks
            .iter()
            .find(|t| &t.entry.strh_type == b"vids")
            .map(|t| t.packet_count)
            .unwrap_or_else(|| self.tracks.first().map(|t| t.packet_count).unwrap_or(0));

        let end_pos = self.output.stream_position()?;

        // avih.dwTotalFrames file offset:
        //   12 (RIFF preamble) + 12 (LIST hdrl preamble) + 8 ("avih" + size)
        //   + 16 (TotalFrames body offset) = 48.
        self.output.seek(SeekFrom::Start(48))?;
        self.output.write_all(&total_video_frames.to_le_bytes())?;

        // First strl LIST starts at the file offset right after the avih
        // chunk: 12 + 12 + 8 + 56 = 88 ... wait, but the avih body is
        // 56 B → avih chunk = 64 B → first strl LIST starts at
        //   12 (RIFF preamble) + 12 (hdrl LIST preamble) + 64 (avih chunk)
        // = 88.  However for the OpenDML envelope the first stream's
        // strl ALSO contains an indx chunk after strf, so the second
        // stream's strl starts an extra
        //   (8 + 24 + 16*OPENDML_SUPER_INDEX_CAPACITY) bytes later.
        let mut strl_off: u64 = 88;
        let opendml = matches!(self.kind, AviKind::OpenDml(_));
        for (i, t) in self.tracks.iter().enumerate() {
            let strh_body_off = strl_off + 20;
            // strh.dwLength is at body offset 32 → file offset strh_body_off + 32.
            let length = if &t.entry.strh_type == b"auds" {
                // For PCM we store sample_count (frames). For VBR we'd
                // normally use packet count, but we don't support VBR audio
                // in the mux yet.
                t.sample_count as u32
            } else {
                t.packet_count
            };
            self.output.seek(SeekFrom::Start(strh_body_off + 32))?;
            self.output.write_all(&length.to_le_bytes())?;

            // Also patch strh.dwSuggestedBufferSize at body offset 36.
            self.output.seek(SeekFrom::Start(strh_body_off + 36))?;
            self.output.write_all(&t.max_chunk_size.to_le_bytes())?;

            // Advance strl_off by the size of this strl LIST (8 header +
            // body). Body = 4 (form) + 64 (strh) + 8 + strf.len() + pad
            // [+ 8 + indx_payload_padded if i == 0 and opendml]
            // [+ 8 + vprp_payload_padded if video stream and opendml].
            let strf_padded = t.entry.strf.len() + (t.entry.strf.len() & 1);
            let mut strl_body = 4 + 64 + 8 + strf_padded;
            if opendml && i == 0 {
                let indx_payload = 24 + 16 * OPENDML_SUPER_INDEX_CAPACITY;
                let indx_padded = indx_payload + (indx_payload & 1);
                strl_body += 8 + indx_padded;
            }
            // vprp emission matches `write_strl(.., with_vprp = opendml &&
            // strh_type == "vids")`. Payload is fixed-size (68 B → even,
            // no pad).
            if opendml && &t.entry.strh_type == b"vids" {
                strl_body += 8 + 68;
            }
            strl_off += 8 + strl_body as u64;
        }

        // Restore writer position.
        self.output.seek(SeekFrom::Start(end_pos))?;

        // OpenDML 2.0 §5.0: back-patch dmlh.dwTotalFrames with the
        // cross-segment total (= total_video_frames; AVIX continuation
        // frames are already summed into the primary video stream's
        // packet_count via TrackState::packet_count by the time
        // write_trailer runs).
        if let Some(off) = self.dmlh_total_frames_off {
            self.output.seek(SeekFrom::Start(off))?;
            self.output.write_all(&total_video_frames.to_le_bytes())?;
            self.output.seek(SeekFrom::Start(end_pos))?;
        }
        Ok(())
    }

    /// Serialize the idx1 body for the primary segment.
    fn serialize_idx1(&self) -> Vec<u8> {
        let mut idx_body = Vec::with_capacity(self.index.len() * 16);
        for e in &self.index {
            idx_body.extend_from_slice(&e.ckid);
            idx_body.extend_from_slice(&e.flags.to_le_bytes());
            idx_body.extend_from_slice(&e.offset.to_le_bytes());
            idx_body.extend_from_slice(&e.size.to_le_bytes());
        }
        idx_body
    }

    /// Close the current `RIFF` segment in OpenDML mode. Flushes
    /// per-stream `ix##` chunks at the tail of the movi LIST, finishes
    /// the movi LIST, writes `idx1` if this is the primary segment,
    /// then finishes the outer RIFF and records the segment's
    /// `(offset, total_size, packet_count)`.
    fn close_current_segment(&mut self) -> Result<()> {
        let in_primary = self.segments.is_empty();
        // Close any open `LIST rec ` cluster first so ix## lands at
        // the tail of movi, not nested inside the cluster.
        if self.rec_open_size_off.is_some() {
            self.close_rec_cluster()?;
        }
        // Flush `ix##` AVISTDINDEX chunks at the tail of the current
        // segment's movi LIST. One per track with at least one packet
        // recorded in this segment. Per OpenDML 2.0 §"Index Locations",
        // these live INSIDE the movi LIST so consumers walking movi
        // see them while scanning forward.
        self.flush_ix_chunks()?;
        // Close movi LIST.
        finish_chunk(self.output.as_mut(), self.movi_size_off)?;
        // idx1 only in primary RIFF (offsets are 32-bit, can't span
        // continuation segments anyway).
        if in_primary {
            let idx_body = self.serialize_idx1();
            write_chunk(self.output.as_mut(), b"idx1", &idx_body)?;
        }
        // Close outer RIFF and snapshot total size.
        finish_chunk(self.output.as_mut(), self.riff_size_off)?;
        let after_riff = self.output.stream_position()?;
        let riff_start = self.riff_size_off - 4;
        self.segments.push(SegmentRecord {
            riff_offset: riff_start,
            total_size: after_riff - riff_start,
            packet_count: self.current_segment_packets,
        });
        Ok(())
    }

    /// Write one `ix##` AVISTDINDEX chunk per track that has packets
    /// recorded in the current segment. After flushing, the per-track
    /// `ix_entries` lists are cleared so the next segment starts
    /// fresh. The `qwBaseOffset` we serialise is `movi_start_off + 4`
    /// — i.e. the offset of the first chunk header inside the
    /// segment's movi LIST, which matches what the demuxer's
    /// `parse_ix_chunk` uses as the base for `dw_offset` resolution.
    fn flush_ix_chunks(&mut self) -> Result<()> {
        let qw_base = self.movi_start_off + 4;
        // Take ownership so we can mutate per-track state and then
        // borrow `self.output` mutably alongside.
        let ix_entries_per_track: Vec<Vec<IxStdEntry>> = self
            .tracks
            .iter_mut()
            .map(|t| std::mem::take(&mut t.ix_entries))
            .collect();
        for (track_idx, entries) in ix_entries_per_track.iter().enumerate() {
            if entries.is_empty() {
                continue;
            }
            let stream_is_2field = self.options.field2_streams.contains(&(track_idx as u32));
            // FourCC is "ix" + the two-ASCII-decimal-digit stream index
            // — per OpenDML 2.0 §"Index Locations": "the corresponding
            // index chunks are marked with 'ix##' in the 'movi' data."
            let stream_digits = packet_fourcc_for(track_idx as u32, *b"xx");
            let ix_id = [b'i', b'x', stream_digits[0], stream_digits[1]];
            // wLongsPerEntry: 2 = default 8-B entries; 3 = AVI Field
            // Index Chunk (round-4 P1) with 12-B entries that carry
            // dwOffsetField2 per OpenDML 2.0 §3.0.
            let (w_longs, sub_type, entry_size) = if stream_is_2field {
                (3u16, 0x01u8, 12usize)
            } else {
                (2u16, 0u8, 8usize)
            };
            let mut payload = Vec::with_capacity(32 + entries.len() * entry_size);
            payload.extend_from_slice(&w_longs.to_le_bytes());
            payload.push(sub_type);
            // bIndexType = AVI_INDEX_OF_CHUNKS (0x01).
            payload.push(0x01);
            // nEntriesInUse.
            payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            // dwChunkId.
            payload.extend_from_slice(&self.tracks[track_idx].packet_fourcc);
            // qwBaseOffset.
            payload.extend_from_slice(&qw_base.to_le_bytes());
            // dwReserved3.
            payload.extend_from_slice(&0u32.to_le_bytes());
            // Entries.
            for e in entries.iter() {
                payload.extend_from_slice(&e.dw_offset.to_le_bytes());
                payload.extend_from_slice(&e.dw_size_with_flag.to_le_bytes());
                if stream_is_2field {
                    payload.extend_from_slice(&e.dw_offset_field2.to_le_bytes());
                }
            }
            write_chunk(self.output.as_mut(), &ix_id, &payload)?;
        }
        Ok(())
    }

    /// Open a new `RIFF AVIX` continuation segment. Writes the RIFF
    /// header, `AVIX` form-type, and a fresh `LIST movi`. Resets the
    /// per-segment cursors but does NOT touch idx1 (continuation
    /// segments don't carry idx1).
    fn open_avix_segment(&mut self) -> Result<()> {
        // Begin a new RIFF AVIX list.
        self.riff_size_off = begin_list(self.output.as_mut(), &RIFF, b"AVIX")?;
        // Begin movi LIST.
        self.movi_size_off = begin_list(self.output.as_mut(), &LIST, b"movi")?;
        self.movi_start_off = self.movi_size_off + 4;
        self.current_segment_packets = 0;
        self.rec_open_size_off = None;
        self.rec_packets_in_cluster = 0;
        self.rec_bytes_in_cluster = 0;
        Ok(())
    }

    /// Open a new `LIST rec ` cluster inside the current `movi` LIST.
    /// Writes the `LIST` header + `rec ` form-type and reserves a 4-byte
    /// size placeholder; the size is patched in [`close_rec_cluster`]
    /// when the cluster fills its packet quota or `movi` closes.
    fn open_rec_cluster(&mut self) -> Result<()> {
        let off = begin_list(self.output.as_mut(), &LIST, b"rec ")?;
        self.rec_open_size_off = Some(off);
        self.rec_packets_in_cluster = 0;
        self.rec_bytes_in_cluster = 0;
        Ok(())
    }

    /// Close the open `LIST rec ` cluster (no-op if none is open). Patches
    /// the cluster's size field so a later scan walks past it cleanly.
    fn close_rec_cluster(&mut self) -> Result<()> {
        if let Some(off) = self.rec_open_size_off.take() {
            finish_chunk(self.output.as_mut(), off)?;
            self.rec_packets_in_cluster = 0;
            self.rec_bytes_in_cluster = 0;
        }
        Ok(())
    }

    /// Stamp `payload_offset` onto the next [`oxideav_core::Muxer::write_packet`]
    /// call so the corresponding `ix##` `AVISTDINDEX_ENTRY.dwOffsetField2`
    /// (per OpenDML 2.0 §3.0 "AVI Field Index Chunk") points at the
    /// second field's first byte. `payload_offset` is measured from
    /// the first byte of the packet's payload.
    ///
    /// Round-4 P1/P3 hook. One-shot — consumed by the next
    /// `write_packet` (regardless of stream) and then cleared. For
    /// streams not in [`AviMuxOptions::field2_streams`] the value is
    /// dropped at `write_packet` time and the std-index entry stays
    /// 8 bytes wide.
    pub fn set_field2_offset(&mut self, payload_offset: u32) {
        self.pending_field2_offset = Some(payload_offset);
    }

    /// Back-patch the OpenDML `indx` super-index entries with each
    /// segment's `(qwOffset, dwSize, dwDuration)` triple, and write
    /// the final `nEntriesInUse`.
    fn patch_super_index(&mut self) -> Result<()> {
        let (Some(n_off), Some(start_off)) =
            (self.indx_entries_count_off, self.indx_entries_start_off)
        else {
            return Ok(());
        };
        let end_pos = self.output.stream_position()?;
        let n_to_write = self.segments.len().min(self.indx_entries_capacity);
        // nEntriesInUse.
        self.output.seek(SeekFrom::Start(n_off))?;
        self.output.write_all(&(n_to_write as u32).to_le_bytes())?;
        // Per-entry slots.
        for (i, seg) in self.segments.iter().take(n_to_write).enumerate() {
            let slot = start_off + (i as u64) * 16;
            self.output.seek(SeekFrom::Start(slot))?;
            self.output.write_all(&seg.riff_offset.to_le_bytes())?;
            // dwSize: total RIFF byte length per spec/06 §6.1.
            let dw_size = seg.total_size.min(u32::MAX as u64) as u32;
            self.output.write_all(&dw_size.to_le_bytes())?;
            // dwDuration: per-segment frame count.
            self.output.write_all(&seg.packet_count.to_le_bytes())?;
        }
        self.output.seek(SeekFrom::Start(end_pos))?;
        Ok(())
    }
}

/// AVIMAINHEADER (56 bytes): dwMicroSecPerFrame, dwMaxBytesPerSec,
/// dwPaddingGranularity, dwFlags, dwTotalFrames, dwInitialFrames, dwStreams,
/// dwSuggestedBufferSize, dwWidth, dwHeight, dwReserved[4].
fn build_avih(tracks: &[TrackState]) -> Vec<u8> {
    let (video_micro_per_frame, width, height) = tracks
        .iter()
        .find(|t| &t.entry.strh_type == b"vids")
        .map(|t| {
            // scale/rate = seconds per frame; micro_per_frame = 1_000_000 * scale/rate.
            let scale = t.entry.scale.max(1) as u64;
            let rate = t.entry.rate.max(1) as u64;
            let upf = (1_000_000u64 * scale / rate) as u32;
            let w = t.stream.params.width.unwrap_or(0);
            let h = t.stream.params.height.unwrap_or(0);
            (upf, w, h)
        })
        .unwrap_or((0, 0, 0));
    let flags: u32 = 0x00000810; // AVIF_ISINTERLEAVED | AVIF_HASINDEX
    let total_frames: u32 = 0; // patched post-hoc
    let streams = tracks.len() as u32;

    let mut body = Vec::with_capacity(56);
    body.extend_from_slice(&video_micro_per_frame.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // MaxBytesPerSec
    body.extend_from_slice(&0u32.to_le_bytes()); // PaddingGranularity
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&total_frames.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // InitialFrames
    body.extend_from_slice(&streams.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // SuggestedBufferSize
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.extend_from_slice(&[0u8; 16]); // reserved[4]
    body
}

/// Build and write a `strl` LIST (strh + strf [+ indx] [+ vprp]).
///
/// Returns `(indx_n_entries_off, indx_entries_start_off)` when
/// `with_indx` is set, otherwise `(None, None)`. The two offsets let
/// the muxer back-patch the OpenDML super-index in `write_trailer`
/// once each segment's RIFF position is known.
///
/// When `with_vprp` is set, an OpenDML 2.0 §5.0 `vprp` chunk is
/// appended to the `strl` after `strf` (and after `indx` if both are
/// present). The default values match the spec's "FORMAT_UNKNOWN /
/// STANDARD_UNKNOWN, single-field, 4:3 aspect" hint for callers that
/// don't carry signal-shape metadata; the chunk lets a downstream
/// tool detect the file as OpenDML 2.0-aware regardless.
#[allow(clippy::too_many_arguments)]
fn write_strl<W: Write + Seek + ?Sized>(
    w: &mut W,
    _index: u32,
    t: &TrackState,
    with_indx: bool,
    with_vprp: bool,
    vprp_override: Option<VprpConfig>,
    indx_2field: bool,
) -> Result<(Option<u64>, Option<u64>)> {
    let strl_off = begin_list(w, &LIST, b"strl")?;

    // strh body (56 bytes).
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(&t.entry.strh_type); // fccType
    strh.extend_from_slice(&t.entry.handler_fourcc); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // flags
    strh.extend_from_slice(&0u16.to_le_bytes()); // priority
    strh.extend_from_slice(&0u16.to_le_bytes()); // language
    strh.extend_from_slice(&0u32.to_le_bytes()); // initial_frames
    strh.extend_from_slice(&t.entry.scale.to_le_bytes());
    strh.extend_from_slice(&t.entry.rate.to_le_bytes());
    strh.extend_from_slice(&0u32.to_le_bytes()); // start
    strh.extend_from_slice(&0u32.to_le_bytes()); // length (patched)
    strh.extend_from_slice(&0u32.to_le_bytes()); // suggested_buffer_size (patched)
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // quality = -1 (default)
    strh.extend_from_slice(&t.entry.sample_size.to_le_bytes());
    // rcFrame: left, top, right, bottom (i16 each). Use 0,0,width,height
    // for video; zeros for audio.
    if &t.entry.strh_type == b"vids" {
        let w_val = t.stream.params.width.unwrap_or(0) as i16;
        let h_val = t.stream.params.height.unwrap_or(0) as i16;
        strh.extend_from_slice(&0i16.to_le_bytes());
        strh.extend_from_slice(&0i16.to_le_bytes());
        strh.extend_from_slice(&w_val.to_le_bytes());
        strh.extend_from_slice(&h_val.to_le_bytes());
    } else {
        strh.extend_from_slice(&[0u8; 8]);
    }
    write_chunk(w, b"strh", &strh)?;

    // strf chunk.
    write_chunk(w, b"strf", &t.entry.strf)?;

    let mut indx_n_entries_off: Option<u64> = None;
    let mut indx_entries_start_off: Option<u64> = None;
    if with_indx {
        // OpenDML 2.0 super-index. Layout per spec/06 §6.1:
        //   WORD  wLongsPerEntry  = 4
        //   BYTE  bIndexSubType   = 0
        //   BYTE  bIndexType      = 0x00 (AVI_INDEX_OF_INDEXES)
        //   DWORD nEntriesInUse   (back-patched)
        //   DWORD dwChunkId       (e.g. b"00dc")
        //   DWORD dwReserved[3]
        //   <16-byte entries> × OPENDML_SUPER_INDEX_CAPACITY
        // Entries are zero-initialised so a partial back-patch leaves
        // a clean tail of zeros that demuxers tolerate.
        let chunk_id = packet_fourcc_for(0, t.entry.chunk_suffix);
        let entries_bytes = OPENDML_SUPER_INDEX_CAPACITY * 16;
        let payload_len = 24 + entries_bytes;
        // Write chunk header by hand so we can compute file offsets.
        write_chunk_header(w, b"indx", payload_len as u32)?;
        let payload_off = w.stream_position()?;
        // Pre-fill with zeros and overwrite the preamble bytes.
        let mut buf = vec![0u8; payload_len];
        buf[0..2].copy_from_slice(&4u16.to_le_bytes());
        // bIndexSubType: 0 (default) or AVI_INDEX_2FIELD per the
        // OpenDML 2.0 §3.0 "Super Index Chunk" rule that the super
        // index inherits the subtype of its child indexes (round-4 P1).
        if indx_2field {
            buf[2] = 0x01; // AVI_INDEX_SUB_2FIELD
        }
        // bIndexType already zero (AVI_INDEX_OF_INDEXES).
        // nEntriesInUse: zero, will be back-patched.
        buf[8..12].copy_from_slice(&chunk_id);
        // dwReserved[3] already zero.
        w.write_all(&buf)?;
        // Even-pad if odd length.
        if payload_len & 1 == 1 {
            w.write_all(&[0])?;
        }
        indx_n_entries_off = Some(payload_off + 4);
        indx_entries_start_off = Some(payload_off + 24);
    }

    // OpenDML 2.0 §5.0 "Video Properties Header" — emit one `vprp`
    // per video stream when requested. We use the spec's
    // `FORMAT_UNKNOWN` / `STANDARD_UNKNOWN` defaults plus the muxer's
    // own width/height + frame-rate so a re-mux's vprp doesn't lie
    // about the resolution. nbFieldPerFrame=1 (progressive) — the
    // muxer doesn't currently emit interlaced indexes, so single
    // field is correct.
    if with_vprp {
        let body = build_vprp_body(t, vprp_override);
        write_chunk(w, b"vprp", &body)?;
    }

    finish_chunk(w, strl_off)?;
    Ok((indx_n_entries_off, indx_entries_start_off))
}

/// Build a `vprp` body for a video track. 9 fixed DWORDs followed by
/// a single `VIDEO_FIELD_DESC` (8 DWORDs) describing the lone
/// progressive field. Total length = 9*4 + 1*32 = 68 bytes.
///
/// `override_cfg`, when supplied, replaces the per-field defaults
/// with caller-chosen values per OpenDML 2.0 §5.0 (round-4 P2). A
/// zero override field falls back to the default so callers can
/// override only what they care about.
fn build_vprp_body(t: &TrackState, override_cfg: Option<VprpConfig>) -> Vec<u8> {
    let width = t.stream.params.width.unwrap_or(0);
    let height = t.stream.params.height.unwrap_or(0);
    // Vertical refresh rate in Hz: rate / scale (samples per second).
    // For video this is conventionally fps (e.g. 25, 30000/1001).
    let stream_refresh_rate = if t.entry.scale > 0 {
        ((t.entry.rate as u64 + (t.entry.scale as u64 / 2)) / t.entry.scale as u64) as u32
    } else {
        0
    };
    let cfg = override_cfg.unwrap_or_default();
    let video_format_token = cfg.video_format_token; // 0 stays FORMAT_UNKNOWN
    let video_standard = cfg.video_standard; // 0 stays STANDARD_UNKNOWN
    let refresh_rate = if cfg.vertical_refresh_rate > 0 {
        cfg.vertical_refresh_rate
    } else {
        stream_refresh_rate
    };
    let frame_aspect_ratio = if cfg.frame_aspect_ratio > 0 {
        cfg.frame_aspect_ratio
    } else {
        (4u32 << 16) | 3u32
    };
    let nb_field_per_frame = if cfg.nb_field_per_frame > 0 {
        cfg.nb_field_per_frame
    } else {
        1
    };
    let mut body = Vec::with_capacity(68);
    body.extend_from_slice(&video_format_token.to_le_bytes());
    body.extend_from_slice(&video_standard.to_le_bytes());
    body.extend_from_slice(&refresh_rate.to_le_bytes()); // dwVerticalRefreshRate
    body.extend_from_slice(&width.to_le_bytes()); // dwHTotalInT (unknown — fall back to width)
    body.extend_from_slice(&height.to_le_bytes()); // dwVTotalInLines
    body.extend_from_slice(&frame_aspect_ratio.to_le_bytes()); // dwFrameAspectRatio
    body.extend_from_slice(&width.to_le_bytes()); // dwFrameWidthInPixels
    body.extend_from_slice(&height.to_le_bytes()); // dwFrameHeightInLines
    body.extend_from_slice(&nb_field_per_frame.to_le_bytes());
    // VIDEO_FIELD_DESC[0]: full-frame valid bitmap. (We always emit
    // a single descriptor; downstream tolerates short tails.)
    body.extend_from_slice(&height.to_le_bytes()); // CompressedBMHeight
    body.extend_from_slice(&width.to_le_bytes()); // CompressedBMWidth
    body.extend_from_slice(&height.to_le_bytes()); // ValidBMHeight
    body.extend_from_slice(&width.to_le_bytes()); // ValidBMWidth
    body.extend_from_slice(&0u32.to_le_bytes()); // ValidBMXOffset
    body.extend_from_slice(&0u32.to_le_bytes()); // ValidBMYOffset
    body.extend_from_slice(&0u32.to_le_bytes()); // VideoXOffsetInT
    body.extend_from_slice(&0u32.to_le_bytes()); // VideoYValidStartLine
    body
}

fn sample_count_of_packet(stream: &StreamInfo, entry: &StrfEntry, size: u32) -> u64 {
    if &entry.strh_type == b"auds" && entry.sample_size > 0 {
        (size as u64) / (entry.sample_size as u64)
    } else {
        let _ = stream;
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    #[test]
    fn packet_fourcc_layout() {
        assert_eq!(packet_fourcc_for(0, *b"dc"), *b"00dc");
        assert_eq!(packet_fourcc_for(1, *b"wb"), *b"01wb");
        assert_eq!(packet_fourcc_for(12, *b"db"), *b"12db");
    }

    #[test]
    fn unsupported_codec_errors_at_open() {
        use oxideav_core::WriteSeek;
        use std::io::Cursor;
        let mut params = CodecParameters::audio(CodecId::new("opus"));
        params.channels = Some(2);
        params.sample_rate = Some(48_000);
        let stream = StreamInfo {
            index: 0,
            time_base: oxideav_core::TimeBase::new(1, 48_000),
            duration: None,
            start_time: Some(0),
            params,
        };
        let cursor: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
        match open(cursor, &[stream]) {
            Err(Error::Unsupported(_)) => {}
            Err(other) => panic!("expected Unsupported, got {other:?}"),
            Ok(_) => panic!("expected Unsupported"),
        }
    }
}
