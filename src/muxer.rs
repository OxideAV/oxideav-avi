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

/// `AVIIF_KEYFRAME` per vfw.h — set on idx1 entries that point at a
/// self-contained keyframe.
pub const AVIIF_KEYFRAME: u32 = 0x0000_0010;

/// `AVIIF_FIRSTPART` per vfw.h — set on idx1 entries that contain the
/// first part of a multi-part frame. Round-6 candidate 1: the AVI
/// muxer sets `AVIIF_FIRSTPART | AVIIF_LASTPART` (= 0x60) on every
/// idx1 entry for a 2-field interlaced stream, so readers walking
/// idx1 alone can detect the 2-field carriage even without an `ix##`
/// AVISTDINDEX. Both bits together mean "this entry is the only
/// part of the frame", which is exactly how the muxer carries
/// 2-field video (one packet per frame, fields concatenated).
pub const AVIIF_FIRSTPART: u32 = 0x0000_0020;

/// `AVIIF_LASTPART` per vfw.h — set on idx1 entries that contain the
/// last part of a multi-part frame. See [`AVIIF_FIRSTPART`].
pub const AVIIF_LASTPART: u32 = 0x0000_0040;

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
    /// Optional override for the OpenDML super-index slot reservation
    /// (round-6 candidate 3). `None` keeps the default
    /// [`OPENDML_SUPER_INDEX_DEFAULT_CAPACITY`] (256 slots = 4 KiB
    /// payload, indexing up to 256 GiB at 1 GiB/segment). `Some(n)`
    /// raises the reserve to `n` slots so very long files can index
    /// every continuation. Clamped to a minimum of 16 slots; values
    /// below that fall back to the default.
    pub super_index_capacity: Option<usize>,
    /// Optional `LIST INFO` metadata payload (round-6 candidate 2).
    /// Each entry is `(chunk_id, value)` where `chunk_id` is a
    /// 4-byte `INFO` sub-chunk FourCC such as `*b"INAM"` (title),
    /// `*b"IART"` (artist), `*b"ICMT"` (comment), `*b"ICRD"` (date),
    /// `*b"IPRD"` (album), `*b"ICOP"` (copyright), `*b"ISFT"`
    /// (encoder), etc. The muxer NUL-terminates each value and
    /// emits a top-level `LIST INFO` chunk between `LIST hdrl` and
    /// `LIST movi` per the AVI 1.0 spec's `INFO` sibling layout.
    /// Empty list = no `LIST INFO` is written.
    pub info_entries: Vec<([u8; 4], String)>,
    /// Per-stream mid-`movi` `ix##` index emit (round-7 candidate 1).
    /// Each entry is `(stream_index, packets_per_flush)`: when the
    /// stream's accumulated `ix_entries` count reaches
    /// `packets_per_flush`, the muxer flushes an inline `ix##` chunk
    /// (e.g. `02ix` for stream 2) right after the current packet,
    /// inside the open `movi` LIST. Per OpenDML 2.0 §"Index Locations
    /// in RIFF File": "the corresponding index chunks are marked with
    /// 'ix##' in the 'movi' data" — the spec's RIFFWALK example shows
    /// a timecode stream's `02ix` mid-`movi` rather than only at
    /// segment tail. Entries already flushed inline are cleared from
    /// the per-track buffer, so the remaining tail (if any) still
    /// flushes via [`AviMuxer::flush_ix_chunks`] at segment close.
    /// `packets_per_flush` < 1 disables the periodic flush
    /// (entries land at segment close like every other stream).
    /// Only meaningful for [`AviKind::OpenDml`]; ignored for
    /// `AviKind::Avi10`.
    pub mid_movi_index_streams: Vec<(u32, u32)>,
    /// Place `LIST INFO` as a sibling of `LIST hdrl` (top-level
    /// child of the outer `RIFF AVI ` form) instead of nesting it
    /// inside `LIST hdrl` (round-11 candidate 1). The AVI 1.0 spec
    /// permits both placements: most legacy writers nest INFO inside
    /// hdrl (the round-6 default), but several tools — notably
    /// Microsoft's own Multimedia File Reference recommended layout
    /// — emit `LIST INFO` between hdrl and movi as a sibling of
    /// hdrl. The demuxer accepts both placements; this flag picks
    /// which one the muxer emits when [`Self::info_entries`] is
    /// non-empty. Default `false` (nested in hdrl) preserves the
    /// round-6 byte layout for existing callers.
    pub info_top_level: bool,
    /// Optional `AVIMAINHEADER.dwFlags` override (round-12 candidate
    /// 2). `None` keeps the round-6 default
    /// `AVIF_HASINDEX | AVIF_TRUSTCKTYPE` (`0x0000_0810`); `Some(n)`
    /// stamps `n` verbatim into the `avih.dwFlags` DWORD per
    /// Microsoft's `vfw.h` `AVIF_*` constants. Pairs with the round-10
    /// C3 demuxer accessor [`crate::demuxer::AviDemuxer::avih_flags`]
    /// so a builder→writer→demuxer round-trip can preserve flag bits
    /// like `AVIF_ISINTERLEAVED` (0x0100), `AVIF_WASCAPTUREFILE`
    /// (0x0001_0000), `AVIF_COPYRIGHTED` (0x0002_0000), and
    /// `AVIF_MUSTUSEINDEX` (0x0020) that the legacy default omits.
    /// Use [`Self::with_avih_flags`] / [`Self::with_avih_flag_bit`] to
    /// construct without remembering the constants.
    pub avih_flags_override: Option<u32>,
    /// Optional `avih.dwSuggestedBufferSize` override (round-13
    /// candidate 2). `None` (the default) lets the muxer compute the
    /// hint itself in `write_trailer`: the maximum chunk-body size
    /// observed across every stream, rounded up to the next 4-byte
    /// boundary. `Some(n)` stamps `n` verbatim — useful for capture
    /// tools that already know their per-stream peak allocation
    /// budget. Per AVI 1.0 §3.1 the field is the largest single chunk
    /// a player should expect to read in one shot, i.e. the
    /// recommended read-ahead allocation hint.
    pub suggested_buffer_size_override: Option<u32>,
    /// Optional `avih.dwMaxBytesPerSec` override (round-14 candidate
    /// 1). `None` (the default) lets the muxer compute the value
    /// itself in `write_trailer`: total bytes across every stream's
    /// `movi` payloads divided by the file's nominal duration in
    /// seconds (`avih.dwTotalFrames * avih.dwMicroSecPerFrame /
    /// 1_000_000`). `Some(n)` stamps `n` verbatim. Per AVI 1.0 §3.1
    /// the field is the approximate maximum data rate the file
    /// requires; capture-card players use it to size their disk-read
    /// pacing budget. Pre-round-14 the field was hard-coded to 0,
    /// which forced players to fall back to a worst-case heuristic.
    pub max_bytes_per_sec_override: Option<u32>,
    /// Synthesise the primary segment's `idx1` body from each stream's
    /// `ix##` standard-index entries instead of from the muxer's own
    /// running `index` collection (round-16 candidate 1). Default
    /// `false` keeps the round-3 behaviour: every `write_packet` in the
    /// primary segment appends one `IndexEntry` and `serialize_idx1`
    /// emits them in file order.
    ///
    /// When set to `true` AND the file is in [`AviKind::OpenDml`]
    /// mode, [`AviMuxer::write_trailer`] / [`AviMuxer::close_current_segment`]
    /// instead walks every primary-segment `ix##` entry that was
    /// recorded for any stream (snapshot taken before
    /// [`AviMuxer::flush_ix_for_track`] clears the per-track buffer)
    /// and emits one 16-B `idx1` entry per packet. Per AVI 1.0 + OpenDML
    /// 2.0 §"Index Locations": AVI 1.0-only readers (Windows Media
    /// Player on XP, ffplay's strict AVI 1.0 path) honour `idx1`
    /// alone — they don't walk OpenDML `ix##` super-indexes. When a
    /// file is OpenDML-muxed without `idx1`, those readers can't
    /// seek. This option closes that compat gap by guaranteeing the
    /// idx1 covers every primary-segment packet even if the muxer's
    /// own `index` collection were ever bypassed (e.g. a future
    /// "OpenDML-only" code path) — and serves as a self-consistency
    /// check that the two index views agree.
    ///
    /// Result is one entry per packet × per primary-segment stream;
    /// AVIX continuation packets are NOT included (idx1 offsets are
    /// 32-bit and relative to the primary `movi` LIST). For
    /// [`AviKind::Avi10`] files this option is a no-op (Avi10 has
    /// no `ix##` chunks to walk).
    pub synthesise_idx1_from_ix: bool,
    /// Per-stream `dwMaxBytesPerSec` cap (round-18 candidate 1). Each
    /// `(stream_index, bytes_per_sec)` entry sets a per-track ceiling
    /// the muxer compares against the observed
    /// `total_bytes / file_duration_seconds` per stream at
    /// `write_trailer` time. Streams not listed have no per-stream
    /// cap (the file-wide [`Self::max_bytes_per_sec_override`] still
    /// applies). The first entry per `stream_index` wins; later
    /// builder calls for the same index replace the prior cap.
    ///
    /// Per AVI 1.0 §3.1 the file-wide `avih.dwMaxBytesPerSec` is the
    /// approximate maximum data rate the FILE requires. For VBR
    /// streams with strict per-track playback budgets (an AC-3
    /// stream that must stay under 384 kbit/s for a downstream
    /// hardware decoder; a Motion-JPEG video stream stamped with a
    /// per-track recording allowance) the file-wide value is
    /// insufficient — a player needs to know which track exceeded
    /// its cap, not just that the sum is too large. The muxer
    /// surfaces every breach via [`AviMuxer::over_budget_streams`]
    /// (a `(stream_idx, observed_bps, cap)` triple) so callers can
    /// log / display / re-encode the offending track. With
    /// [`Self::strict_per_stream_budget`] set the breach instead
    /// fails [`AviMuxer::write_trailer`] with [`Error::InvalidData`].
    pub per_stream_max_bytes_per_sec: Vec<(u32, u32)>,
    /// Promote per-stream budget breaches to a hard error in
    /// [`AviMuxer::write_trailer`] (round-18 candidate 1). Default
    /// `false` keeps the lenient behaviour:
    /// [`AviMuxer::over_budget_streams`] surfaces the breaches as
    /// metadata. `true` makes the trailer fail with
    /// [`Error::InvalidData`] on the first breach (other breaches
    /// still land in `over_budget_streams` so a caller catching the
    /// error can still inspect the full set). Only meaningful when
    /// at least one [`Self::per_stream_max_bytes_per_sec`] entry was
    /// registered.
    pub strict_per_stream_budget: bool,
    /// Per-stream top-down DIB flag (round-19 candidate 1). Stream
    /// indexes listed here are emitted with a **negative `biHeight`**
    /// in their BMIH `strf` payload, signalling a top-down DIB
    /// (origin upper-left) per VfW `wingdi.h` §"biHeight sign
    /// rules". Only semantically meaningful for uncompressed RGB
    /// streams (`BI_RGB` and `BI_BITFIELDS`) — YUV bitmaps are
    /// always top-down regardless of sign, and the spec REQUIRES a
    /// positive `biHeight` for compressed FourCCs, so the muxer
    /// silently drops the flag for any stream whose `params.tag`
    /// resolves to a printable FourCC (rgb24's all-zero
    /// `[0,0,0,0]` sentinel is the one stream that's actually
    /// uncompressed). Pairs with the round-19 C1 demuxer accessor
    /// [`crate::demuxer::AviDemuxer::stream_top_down`] so a parsed
    /// top-down stream can round-trip its orientation. Duplicate
    /// entries for the same stream are deduplicated; empty list
    /// (the default) preserves the round-3 byte layout (positive
    /// `biHeight`) for existing callers.
    pub top_down_video_streams: Vec<u32>,
    /// Per-stream WAVEFORMATEXTENSIBLE emit (round-75). Each entry is
    /// `(stream_index, channel_mask, valid_bits_per_sample,
    /// subformat_guid)`: when an `auds` stream is listed here, the
    /// muxer emits a 40-byte `WAVE_FORMAT_EXTENSIBLE` (`0xFFFE`)
    /// `strf` per Microsoft `mmreg.h` § "WAVEFORMATEXTENSIBLE" instead
    /// of the legacy 18-byte `WAVEFORMATEX`. The 22-byte extension
    /// carries the channel-mask bitmap, valid bits per sample, and
    /// SubFormat GUID (i.e. the canonical codec identifier when the
    /// legacy `wFormatTag` escape hatch is in use). Duplicate calls
    /// for the same `stream_index` replace the prior entry.
    ///
    /// Required for any audio stream that needs to carry one of:
    /// - More than 2 channels with an explicit speaker assignment
    ///   (5.1, 7.1, …) — the channel mask drives byte order;
    /// - 24-bit precision in a 32-bit container — `valid_bits_per_sample`
    ///   captures the precision while WAVEFORMATEX-side
    ///   `bits_per_sample` holds the container size;
    /// - Identification via SubFormat GUID — multi-codec dispatch
    ///   beyond the 16-bit `wFormatTag` registry.
    ///
    /// See [`Self::with_extensible_audio`].
    pub extensible_audio_streams: Vec<(u32, u32, u16, crate::stream_format::Guid)>,
    /// Per-stream human-readable names emitted as `strn` chunks
    /// inside each stream's `strl` LIST per AVI 1.0 §"AVI Stream
    /// Headers" (round-80). Each entry is `(stream_index, name)`;
    /// the muxer writes `name` followed by a single NUL terminator
    /// as the chunk body, even-padded with one zero byte when the
    /// resulting length is odd (per RIFF §"data is always padded to
    /// nearest WORD boundary"). Duplicate calls for the same stream
    /// index replace the prior entry — see
    /// [`Self::with_stream_name`]. Empty list (the default) preserves
    /// pre-round-80 byte layout (no `strn` chunk emitted).
    pub stream_names: Vec<(u32, String)>,
    /// Per-stream opaque codec-driver configuration blobs emitted as
    /// `strd` chunks inside each stream's `strl` LIST per AVI 1.0
    /// §"AVI Stream Headers" (round-89). Each entry is `(stream_index,
    /// bytes)`; the muxer writes the bytes verbatim as the chunk
    /// body, even-padded with one zero byte when the length is odd
    /// per RIFF §"data is always padded to nearest WORD boundary".
    /// The spec defines this body as opaque codec-driver data — see
    /// [`Self::with_stream_header_data`]. Duplicate calls for the
    /// same stream index replace the prior entry. Empty list (the
    /// default) preserves pre-round-89 byte layout (no `strd` chunk
    /// emitted).
    pub stream_header_data: Vec<(u32, Vec<u8>)>,
    /// Per-stream `strh.rcFrame` destination-rectangle overrides
    /// (round-115). Each entry is `(stream_index, [left, top, right,
    /// bottom])` and replaces the muxer's default `rcFrame` for that
    /// stream in the 56-byte AVISTREAMHEADER per AVI 1.0
    /// §"AVISTREAMHEADER". The default rect is `0,0,width,height` for
    /// video streams and all-zero for non-video streams; an override lets
    /// a caller place a text or video stream at an arbitrary sub-rectangle
    /// inside the movie rectangle (`avih.dwWidth` × `dwHeight`) — e.g. a
    /// picture-in-picture second video stream, or a subtitle overlay box.
    /// Duplicate calls for the same stream index replace the prior entry —
    /// see [`Self::with_stream_frame_rect`]. Empty list (the default)
    /// preserves the pre-round-115 byte layout.
    pub stream_frame_rects: Vec<(u32, [i16; 4])>,
    /// Per-stream `strh.wLanguage` LANGID overrides (round-119). Each
    /// entry is `(stream_index, langid)` and stamps the given 16-bit
    /// value into byte offset 14 of that stream's 56-byte
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`wLanguage` row
    /// in `docs/container/riff/avi-riff-file-reference.md`: *"Language
    /// tag (BCP 47 / RFC 1766 / similar; AVI does not normatively pin
    /// a registry)."*). Without an override the muxer writes `0`
    /// ("LANG_NEUTRAL / SUBLANG_NEUTRAL", the writer-skips-it default
    /// that the demuxer maps back to `None`); this builder lets a
    /// caller stamp a non-zero LANGID — Microsoft writers conventionally
    /// pack a Win32 LANGID (low 10 bits = `LANG_*` primary, upper 6
    /// bits = `SUBLANG_*` dialect; e.g. `0x0409` = `LANG_ENGLISH /
    /// SUBLANG_ENGLISH_US`, `0x0411` = `LANG_JAPANESE /
    /// SUBLANG_DEFAULT`), but the muxer writes whatever 16-bit value
    /// the caller supplies verbatim and does not validate against any
    /// registry. Duplicate calls for the same stream index replace the
    /// prior entry — see [`Self::with_stream_language`]. Empty list
    /// (the default) preserves the pre-round-119 byte layout
    /// (`wLanguage = 0`).
    pub stream_languages: Vec<(u32, u16)>,
    /// Per-stream `strh.dwInitialFrames` overrides (round-153). Each
    /// entry is `(stream_index, initial_frames)` and stamps the given
    /// 32-bit value into byte offset 16 of that stream's 56-byte
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    /// (`dwInitialFrames` row in
    /// `docs/container/riff/avi-riff-file-reference.md`: *"How far
    /// audio data is skewed ahead of the video frames in interleaved
    /// files. Typically, this is about 0.75 seconds. If creating
    /// interleaved files, set the value of this member to the number
    /// of frames in the file prior to the initial frame of the AVI
    /// sequence."*). Without an override the muxer writes `0`
    /// ("noninterleaved file" per AVIMAINHEADER §`dwInitialFrames`:
    /// *"Noninterleaved files should specify zero"* — the default the
    /// demuxer maps back to `None`); this builder lets a caller stamp
    /// a non-zero skew on a per-stream basis — typical use is a
    /// captured interleaved file where audio leads video by ~0.75
    /// seconds, recorded by stamping the audio stream's leading-frame
    /// count here. The muxer writes whatever 32-bit value the caller
    /// supplies verbatim and does not validate it against the per-stream
    /// `dwLength`. Duplicate calls for the same stream index replace
    /// the prior entry — see [`Self::with_stream_initial_frames`].
    /// Empty list (the default) preserves the pre-round-153 byte layout
    /// (`dwInitialFrames = 0` on every stream).
    pub stream_initial_frames: Vec<(u32, u32)>,
    /// Per-stream `strh.dwQuality` overrides (round-176). Each entry is
    /// `(stream_index, quality)` and stamps the given 32-bit value into
    /// byte offset 40 of that stream's 56-byte AVISTREAMHEADER per AVI
    /// 1.0 §"AVISTREAMHEADER" (`dwQuality` row in
    /// `docs/container/riff/avi-riff-file-reference.md` line 246:
    /// *"Indicator of the quality of the data in the stream. Quality
    /// is represented as a number between 0 and 10,000. For compressed
    /// data, this typically represents the value of the quality
    /// parameter passed to the compression software. If set to -1,
    /// drivers use the default quality value."*). Without an override
    /// the muxer writes `0xFFFF_FFFF` (= `-1` as i32, the documented
    /// "use default driver quality" sentinel — the muxer's own default
    /// since round-3, which the demuxer maps back to `None`); this
    /// builder lets a caller stamp the quality parameter the encoder
    /// was driven with so it round-trips through re-mux. The muxer
    /// writes whatever 32-bit value the caller supplies verbatim and
    /// does not clamp to the documented `[0, 10_000]` range — anomalous
    /// out-of-spec writers (capture drivers stamping full-precision
    /// quality scores etc.) round-trip exactly. Duplicate calls for the
    /// same stream index replace the prior entry — see
    /// [`Self::with_stream_quality`]. Empty list (the default)
    /// preserves the pre-round-176 byte layout (`dwQuality = -1` on
    /// every stream).
    pub stream_qualities: Vec<(u32, u32)>,
    /// Per-stream `strh.wPriority` overrides (round-182). Each entry is
    /// `(stream_index, priority)` and stamps the given 16-bit DWORD
    /// into byte offset 12 of that stream's 56-byte AVISTREAMHEADER per
    /// AVI 1.0 §"AVISTREAMHEADER" (`wPriority` row in
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix B line
    /// 238: *"Priority of a stream type. For example, in a file with
    /// multiple audio streams, the one with the highest priority might
    /// be the default stream."*). Without an override the muxer writes
    /// `0` (the muxer's own default since round-3, which the demuxer
    /// maps back to `None`); this builder lets a caller stamp a
    /// selection hint among same-`fccType` streams so it round-trips
    /// through re-mux. The muxer writes whatever 16-bit value the
    /// caller supplies verbatim and does not validate it — the spec
    /// does not normatively pin a value range or a tie-break rule, so
    /// any anomalous out-of-spec writer that uses the field for
    /// application-specific tagging round-trips exactly. Duplicate
    /// calls for the same stream index replace the prior entry — see
    /// [`Self::with_stream_priority`]. Empty list (the default)
    /// preserves the pre-round-182 byte layout (`wPriority = 0` on
    /// every stream).
    pub stream_priorities: Vec<(u32, u16)>,
    /// Per-stream `strh.dwStart` overrides (round-203). Each entry is
    /// `(stream_index, start)` and stamps the given 32-bit DWORD into
    /// byte offset 28 of that stream's 56-byte AVISTREAMHEADER per AVI
    /// 1.0 §"AVISTREAMHEADER" (`dwStart` row in
    /// `docs/container/riff/avi-riff-file-reference.md` line 243:
    /// *"Starting time for this stream. The units are defined by the
    /// dwRate and dwScale members in the main file header. Usually,
    /// this is zero, but it can specify a delay time for a stream that
    /// does not start concurrently with the file."*). Without an
    /// override the muxer writes `0` (the muxer's own default since
    /// round-3, the spec-documented "starts concurrently with the
    /// file" value the demuxer maps back to `None`); this builder lets
    /// a caller stamp a non-zero start offset on a per-stream basis so
    /// a re-mux of a file whose audio is delayed relative to the video
    /// (or whose video starts late relative to a longer audio track)
    /// round-trips exactly. The muxer writes whatever 32-bit value the
    /// caller supplies verbatim and does not validate it against the
    /// per-stream `dwLength` — the spec phrases the field as a stream-
    /// local tick count in `(dwRate / dwScale)` units, not a global
    /// constraint. Duplicate calls for the same stream index replace
    /// the prior entry — see [`Self::with_stream_start`]. Empty list
    /// (the default) preserves the pre-round-203 byte layout
    /// (`dwStart = 0` on every stream).
    pub stream_starts: Vec<(u32, u32)>,
    /// Per-stream `strh.fccHandler` driver-handler FourCC override
    /// (round-210). Each entry is a tuple of
    /// `(stream_index, fourcc_bytes)` and stamps the given 4 bytes
    /// into byte offset 4 of that stream's 56-byte AVISTREAMHEADER
    /// per AVI 1.0 §"AVISTREAMHEADER" (`fccHandler` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, Appendix B
    /// line 236: *"An optional FOURCC that identifies a specific
    /// data handler. The data handler is the preferred handler for
    /// the stream. For audio and video streams, this specifies the
    /// codec for decoding the stream."*).
    ///
    /// Without an override the muxer writes the packaging-derived
    /// default: for video streams that's the per-codec FourCC
    /// (`MJPG` / `XVID` / etc., mirroring what most writers in the
    /// wild do — `fccHandler == biCompression`); for audio streams
    /// that's the all-zero `\0\0\0\0` "no preferred handler"
    /// default (which the demuxer maps back to `None`). This builder
    /// lets a caller stamp a different FourCC — useful for
    /// re-muxing a capture where the source file's fccHandler does
    /// not equal biCompression (some legacy VfW drivers store a
    /// driver-suite identifier here that differs from the
    /// per-stream codec tag), for stamping a per-stream driver
    /// hint on an audio stream, or for explicitly zeroing the
    /// field on a video stream whose original writer left
    /// fccHandler empty.
    ///
    /// The muxer writes the 4 bytes verbatim and does not validate
    /// printability (the spec's *optional FOURCC* phrasing does not
    /// normatively pin it). Stamping `[0, 0, 0, 0]` is equivalent
    /// to omitting the override for audio streams (the audio
    /// default is also all-zero); on video streams stamping
    /// `[0, 0, 0, 0]` explicitly zeroes the field, overriding the
    /// `biCompression`-mirror default.
    ///
    /// Duplicate calls for the same stream index replace the prior
    /// entry — see [`Self::with_stream_handler`]. Empty list (the
    /// default) preserves the pre-round-210 byte layout
    /// (packaging-derived `fccHandler` on every stream).
    pub stream_handlers: Vec<(u32, [u8; 4])>,
    /// Per-stream `strh.fccType` overrides (round-253).
    ///
    /// Each entry is `(stream_index, fcc_type)` and stamps the given
    /// 4-byte FOURCC at byte offset 0 of that stream's 56-byte
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    /// (`docs/container/riff/avi-riff-file-reference.md`, Appendix B
    /// `fccType` row at line 235 + the `fcc` row at line 234 which
    /// documents the standard `auds` / `mids` / `txts` / `vids`
    /// values: *"A FOURCC code that specifies the type of data
    /// contained in the stream."*).
    ///
    /// Without an override the muxer keeps its packaging-derived
    /// default (`t.entry.strh_type` — `vids` for video streams,
    /// `auds` for audio streams). The override is useful for
    /// re-muxing a stream that should be carried under a non-default
    /// FOURCC (e.g. emitting an `auds`-typed PCM payload under the
    /// `mids` MIDI type FOURCC for a self-consistent MIDI-stream
    /// round-trip), or for stamping a non-standard / vendor FOURCC
    /// that downstream tooling expects to see.
    ///
    /// The muxer writes the 4 bytes verbatim and does NOT validate
    /// printability or membership in the spec-documented
    /// `{auds, mids, txts, vids}` set — the spec does not pin a
    /// closed registry, and non-standard FOURCCs surface verbatim
    /// for a downstream caller to interpret. Stamping `[0, 0, 0, 0]`
    /// is equivalent to omitting the override since the demuxer
    /// maps the all-zero sentinel back to `None`.
    ///
    /// Stamping a `txts` type on a stream that's actually carrying
    /// PCM audio is internally inconsistent on purpose — the
    /// long-standing convention that side-band byte stamps are
    /// byte-stamp-only.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry — see [`Self::with_stream_fcc_type`]. Empty list (the
    /// default) preserves the pre-round-253 byte layout
    /// (packaging-derived `fccType` on every stream).
    pub stream_fcc_types: Vec<(u32, [u8; 4])>,
    /// Per-stream `strh.dwSuggestedBufferSize` overrides (round-217).
    /// Each entry is `(stream_index, suggested_buffer_size)` and stamps
    /// the given 32-bit DWORD into byte offset 36 of that stream's
    /// 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    /// (`dwSuggestedBufferSize` row in
    /// `docs/container/riff/avi-riff-file-reference.md` line 245: *"How
    /// large a buffer should be used to read this stream. Typically,
    /// this contains a value corresponding to the largest chunk present
    /// in the stream. Using the correct buffer size makes playback more
    /// efficient. Use zero if you do not know the correct buffer size."*).
    ///
    /// Without an override the muxer keeps its long-standing
    /// auto-derived default: `t.max_chunk_size` (the largest body it
    /// observed on that stream during `write_packet` calls), patched
    /// into the strh at the end of `write_trailer`. This builder lets
    /// a caller stamp an explicit hint — useful for round-tripping a
    /// file whose original writer over-declared a peak the actual
    /// largest chunk doesn't match (some capture tools preallocate a
    /// fixed-size readback buffer and stamp it verbatim), for forcing
    /// the `0` "do not know" sentinel back (so the demuxer round-trips
    /// the absent-hint case), or for matching a downstream player's
    /// read-ahead budget exactly.
    ///
    /// The muxer writes whatever 32-bit value the caller supplies
    /// verbatim and does NOT validate it against the actual largest
    /// chunk observed in `movi`. Duplicate calls for the same stream
    /// index replace the prior entry — see
    /// [`Self::with_stream_suggested_buffer_size`]. Empty list (the
    /// default) preserves the pre-round-217 byte layout (auto-derived
    /// `t.max_chunk_size` on every stream).
    pub stream_suggested_buffer_sizes: Vec<(u32, u32)>,
    /// Per-stream `strh.dwSampleSize` overrides (round-222). Each entry
    /// is `(stream_index, sample_size)` and stamps the given 32-bit
    /// DWORD into byte offset 44 of that stream's 56-byte
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    /// (`dwSampleSize` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 247:
    /// *"The size of a single sample of data. This is set to zero if
    /// the samples can vary in size. If this number is nonzero, then
    /// multiple samples of data can be grouped into a single chunk
    /// within the file. If it is zero, each sample of data (such as a
    /// video frame) must be in a separate chunk. For video streams,
    /// this number is typically zero, although it can be nonzero if
    /// all video frames are the same size. For audio streams, this
    /// number should be the same as the nBlockAlign member of the
    /// WAVEFORMATEX structure describing the audio."*).
    ///
    /// Without an override the muxer keeps its long-standing
    /// packaging-derived default: audio streams get `nBlockAlign` for
    /// PCM / CBR audio and `0` for VBR audio (MP3 / AAC / MPEG); video
    /// streams get `0` (one frame per chunk). The override stamps the
    /// 32-bit byte value verbatim at offset 44 of the strh and does
    /// NOT change the muxer's own `dwLength` derivation (which keeps
    /// using the packaging-derived `entry.sample_size` for the audio
    /// `size / sample_size` formula); a caller that stamps a
    /// dwSampleSize incompatible with their packet stream is creating
    /// an internally-inconsistent file on purpose (e.g. to round-trip
    /// a fixed-frame-size legacy video capture, to force a `0`
    /// "samples can vary" sentinel onto a CBR audio stream that the
    /// caller does not want subject to multi-sample chunking, or to
    /// reproduce a pathological writer for fuzz / regression
    /// purposes).
    ///
    /// Note that the demuxer's round-14 C2 audio sample-size
    /// invariant (the VBR/CBR consistency check at `open` time) will
    /// reject mismatched files; callers that intentionally produce
    /// such a file must read it back via
    /// [`crate::demuxer::open_avi_lenient`].
    ///
    /// Passing `0` stamps the spec-documented "samples can vary in
    /// size" sentinel — the demuxer maps that back to `None`,
    /// mirroring the round-217 `dwSuggestedBufferSize` / round-210
    /// `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
    /// round-176 `dwQuality` / round-153 `dwInitialFrames` /
    /// round-119 `wLanguage` / round-115 `rcFrame` "default == absent"
    /// convention.
    ///
    /// Duplicate calls for the same stream index replace the prior
    /// entry — see [`Self::with_stream_sample_size`]. Empty list (the
    /// default) preserves the pre-round-222 byte layout
    /// (packaging-derived `dwSampleSize` on every stream).
    pub stream_sample_sizes: Vec<(u32, u32)>,
    /// Per-stream `strh.dwLength` overrides (round-229). Each entry is
    /// `(stream_index, length)` and stamps the given 32-bit DWORD into
    /// byte offset 32 of that stream's 56-byte AVISTREAMHEADER per AVI
    /// 1.0 §"AVISTREAMHEADER" (`dwLength` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 244:
    /// *"Length of this stream. The units are defined by the dwRate
    /// and dwScale members of the stream's header."*).
    ///
    /// Without an override the muxer keeps its long-standing
    /// auto-derived default in
    /// [`AviMuxer::write_trailer`] / `patch_post_counts`:
    /// `packet_count` for video streams and PCM / CBR audio's
    /// `sample_count` (the running total derived from each packet's
    /// declared `Packet.duration` per the muxer's `size /
    /// sample_size` formula); the override replaces that derived
    /// value at the patch site. The 32-bit byte stamp at offset 32
    /// is the only change — the muxer does NOT touch
    /// `avih.dwTotalFrames` (the per-stream length and the file-global
    /// total are spec-independent fields), and does NOT alter any
    /// downstream `idx1` / `ix##` / `dmlh` derivation. Callers can
    /// therefore reproduce legacy writers whose `dwLength` stamp
    /// disagrees with their actual chunk count (some half-written
    /// capture dumps, fixed-budget streamers that round to a known
    /// playlist boundary, or fuzz / regression fixtures); the
    /// resulting file is internally inconsistent on purpose.
    ///
    /// Passing `0` stamps the de-facto "no length declared" value —
    /// the demuxer maps that back to `None`, mirroring the round-222
    /// `dwSampleSize` / round-217 `dwSuggestedBufferSize` / round-210
    /// `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
    /// round-176 `dwQuality` / round-153 `dwInitialFrames` /
    /// round-119 `wLanguage` / round-115 `rcFrame` "default ==
    /// absent" convention.
    ///
    /// Duplicate calls for the same stream index replace the prior
    /// entry — see [`Self::with_stream_length`]. Empty list (the
    /// default) preserves the pre-round-229 byte layout
    /// (auto-derived `dwLength` on every stream).
    pub stream_lengths: Vec<(u32, u32)>,
    /// Per-stream `strh.dwFlags` overrides (round-247). Each entry is
    /// `(stream_index, flags)` and stamps the given 32-bit DWORD into
    /// byte offset 8 of that stream's 56-byte AVISTREAMHEADER per AVI
    /// 1.0 §"AVISTREAMHEADER" (`dwFlags` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 237 +
    /// the *dwFlags values* table at lines 252–255 carrying
    /// `AVISF_DISABLED` (`0x0000_0001`) and `AVISF_VIDEO_PALCHANGES`
    /// (`0x0001_0000`)).
    ///
    /// Without an override the muxer keeps its pre-round-247 default
    /// of `0` (no flags set) — the legacy writer behaviour the muxer
    /// has used since round-3 and the dominant value in the wild
    /// (disabled-by-default streams and palette-animating video are
    /// the minority). The override is byte-stamp-only — it does NOT
    /// validate the supplied bits against the spec's two documented
    /// constants (so a caller can round-trip undocumented vendor /
    /// driver bits in the upper half-DWORD), does NOT touch
    /// `avih.dwFlags` (the file-global `AVIF_*` flags handled
    /// independently via `with_avih_flags` and friends), and does NOT
    /// cross-validate against other strh fields (e.g. stamping
    /// `AVISF_VIDEO_PALCHANGES` on an audio stream is internally
    /// inconsistent on purpose).
    ///
    /// Passing `0` stamps the legacy "no flags set" value — the
    /// demuxer maps that back to `None`, mirroring the round-229
    /// `dwLength` / round-222 `dwSampleSize` / round-217
    /// `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    /// `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    /// round-153 `dwInitialFrames` / round-119 `wLanguage` /
    /// round-115 `rcFrame` "default == absent" convention.
    ///
    /// Duplicate calls for the same stream index replace the prior
    /// entry — see [`Self::with_stream_flags`]. Empty list (the
    /// default) preserves the pre-round-247 byte layout (zero
    /// `dwFlags` on every stream).
    pub stream_flags: Vec<(u32, u32)>,
    /// Per-stream `(strh.dwScale, strh.dwRate)` timebase overrides
    /// (round-249). Each entry is `(stream_index, scale, rate)` and
    /// stamps the supplied DWORD pair into byte offsets 20 and 24 of
    /// that stream's 56-byte AVISTREAMHEADER per AVI 1.0
    /// §"AVISTREAMHEADER" (`dwScale` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 241 +
    /// the `dwRate` row line 242: *"Used with dwRate to specify the
    /// time scale that this stream will use. Dividing dwRate by
    /// dwScale gives the number of samples per second. For video
    /// streams, this is the frame rate. For audio streams, this rate
    /// corresponds to the time needed to play nBlockAlign bytes of
    /// audio, which for PCM audio is the just the sample rate."*).
    ///
    /// Without an override the muxer keeps its packaging-derived
    /// default (video: the per-stream `frame_rate` pair, audio: the
    /// `sample_rate / 1` pair, mirroring the `time_base` the framework
    /// exposes via [`oxideav_core::StreamInfo`]). The override replaces
    /// both DWORDs verbatim at the byte-stamp site; it does NOT alter
    /// the muxer's `(scale, rate)`-derived `dwLength` computation for
    /// audio streams (which still uses the packaging-derived
    /// `t.entry.{scale,rate}` to convert the running sample count into
    /// `dwLength` units), does NOT touch `avih.dwMicroSecPerFrame` (the
    /// file-global frame-rate hint, which the muxer derives
    /// independently from the first video stream's packaging pair),
    /// and does NOT cross-validate against the per-stream `dwLength`
    /// or `dwStart` (stamping an audio sample rate on a video stream
    /// is internally inconsistent on purpose). A `0` in either DWORD
    /// stamps the writer-skips-it / mathematically-undefined sentinel
    /// the demuxer maps back to `None`.
    ///
    /// Duplicate calls for the same stream index replace the prior
    /// entry — see [`Self::with_stream_timebase`]. Empty list (the
    /// default) preserves the pre-round-249 byte layout
    /// (packaging-derived `dwScale` / `dwRate` on every stream).
    pub stream_timebases: Vec<(u32, u32, u32)>,
    /// `avih.dwPaddingGranularity` for stream-aligned remuxes (round-92).
    ///
    /// Per AVI 1.0 §"AVIMAINHEADER" (docs/container/riff/
    /// avi-riff-file-reference.md line 197): *"Alignment for data, in
    /// bytes. Pad the data to multiples of this value."* The spec
    /// pairs this field with §"Other Data Chunks" line 179: *"Data
    /// can be aligned in an AVI file by inserting 'JUNK' chunks as
    /// needed. Applications should ignore the contents of a 'JUNK'
    /// chunk."*
    ///
    /// When `Some(n)` the muxer:
    ///
    /// 1. Stamps `avih.dwPaddingGranularity = n` verbatim (instead of
    ///    the legacy 0 sentinel meaning "no alignment guarantee").
    /// 2. Before each packet chunk in `movi`, emits a `JUNK` chunk
    ///    whose body length is the minimum value that makes the
    ///    upcoming packet chunk's 8-byte header start at the next
    ///    file-absolute offset divisible by `n`. The JUNK body itself
    ///    is filled with zero bytes; the AVI spec defines its content
    ///    as ignored by readers. The JUNK chunk header is 8 bytes
    ///    (FourCC + size DWORD); if the natural slack is fewer than
    ///    8 bytes the muxer rolls forward to the *next* multiple of
    ///    `n` so the JUNK chunk itself always fits.
    /// 3. Emits the packet chunk normally; its 8-byte header lands at
    ///    a file-absolute offset divisible by `n`, so a media player
    ///    doing stream-aligned reads (e.g. against a 4 KiB filesystem
    ///    block size, or a 2 KiB CD-ROM sector) can read each chunk
    ///    in a single aligned syscall.
    ///
    /// `n` must be a power of two ≥ 2 and ≤ 65536 to take effect;
    /// other values fall back to `None` (no alignment), matching the
    /// legacy `dwPaddingGranularity = 0` behaviour. Use
    /// [`Self::with_padding_granularity`].
    ///
    /// Alignment is best-effort per packet: the muxer measures the
    /// current file-absolute offset right before the packet header
    /// and inserts JUNK to reach the target alignment. The same
    /// alignment applies to every packet across every stream in
    /// `movi`. Sideband chunks (`xxpc` palette change, `xxtx` text)
    /// are not pre-aligned — they're outside the per-frame stream
    /// budget and players don't seek to them via the index.
    pub padding_granularity: Option<u32>,
    /// Optional `avih.dwInitialFrames` override (round-157). `None`
    /// (the default) keeps the legacy `0` value the muxer has
    /// emitted since round-3, which AVI 1.0 §"AVIMAINHEADER" line
    /// 200 documents as the "noninterleaved file" sentinel:
    /// *"Initial frame for interleaved files. Noninterleaved files
    /// should specify zero. If creating interleaved files, specify
    /// the number of frames in the file prior to the initial frame
    /// of the AVI sequence."* `Some(n)` stamps `n` verbatim into the
    /// 32-bit DWORD at byte offset 16 of the 56-byte AVIMAINHEADER
    /// body (i.e. byte 24 of the `avih` chunk).
    ///
    /// File-global counterpart of the per-stream
    /// [`Self::stream_initial_frames`] override (round-153, at byte
    /// offset 16 of each AVISTREAMHEADER). The two fields are
    /// independent — Microsoft writers typically stamp the
    /// per-stream value with the leading-frame count and leave the
    /// file-global one at `0`, but the spec allows either to carry
    /// the skew so the muxer exposes both. The muxer writes whatever
    /// 32-bit value the caller supplies verbatim and performs no
    /// validation against any per-stream `dwLength` / `dwRate`.
    ///
    /// Pairs with the round-157 demuxer accessor
    /// [`crate::demuxer::AviDemuxer::initial_frames`] (and the
    /// `avi:initial_frames` metadata key) for a builder→writer→
    /// demuxer round-trip. Use [`Self::with_initial_frames`].
    pub initial_frames: Option<u32>,
    /// Digitization-date text emitted as an `IDIT` chunk inside
    /// `LIST hdrl` (round-107).
    ///
    /// `IDIT` is a member of the RIFF *Hdrl Tags* namespace
    /// (`DateTimeOriginal`) per
    /// `docs/container/riff/metadata/exiftool-riff-tags.html` §"RIFF
    /// Hdrl Tags" — capture hardware records the capture / digitization
    /// timestamp there. When `Some(s)` the muxer writes an `IDIT` chunk
    /// (direct child of `LIST hdrl`, after the strls / `LIST odml` /
    /// nested `LIST INFO`) whose body is the UTF-8 bytes of `s` followed
    /// by a single NUL terminator, even-padded with one zero byte when
    /// the resulting length is odd per RIFF §"data is always padded to
    /// nearest WORD boundary". `None` (the default) emits no `IDIT`
    /// chunk, preserving pre-round-107 byte layout. The staged docs do
    /// not pin a canonical on-disk text format, so the muxer writes the
    /// caller's string verbatim and the demuxer surfaces it verbatim —
    /// the round-trip is byte-faithful regardless of the chosen format.
    /// See [`Self::with_digitization_date`].
    pub digitization_date: Option<String>,
    /// SMPTE-timecode text emitted as an `ISMP` chunk inside
    /// `LIST hdrl` (round-112).
    ///
    /// `ISMP` is a member of the RIFF *Hdrl Tags* namespace (`TimeCode`)
    /// per `docs/container/riff/metadata/exiftool-riff-tags.html` §"RIFF
    /// Hdrl Tags" — it sits directly alongside `IDIT` and records the
    /// SMPTE timecode of the file's first frame. When `Some(s)` the
    /// muxer writes an `ISMP` chunk (direct child of `LIST hdrl`, after
    /// the strls / `LIST odml` / nested `LIST INFO` / any `IDIT`) whose
    /// body is the UTF-8 bytes of `s` followed by a single NUL
    /// terminator, even-padded with one zero byte when the resulting
    /// length is odd per RIFF §"data is always padded to nearest WORD
    /// boundary". `None` (the default) emits no `ISMP` chunk, preserving
    /// pre-round-112 byte layout. The staged docs do not pin a canonical
    /// on-disk text format, so the muxer writes the caller's string
    /// verbatim and the demuxer surfaces it verbatim — the round-trip is
    /// byte-faithful regardless of the chosen format.
    /// See [`Self::with_smpte_timecode`].
    pub smpte_timecode: Option<String>,
    /// Optional `LIST odml dmlh.dwTotalFrames` override (round-234).
    /// `None` (the default) keeps the long-standing auto-derived value
    /// the muxer has back-patched since round-3: the primary video
    /// stream's running `packet_count` (which already folds in every
    /// AVIX continuation packet because `TrackState::packet_count` is
    /// not reset across segments). `Some(n)` stamps `n` verbatim into
    /// the 32-bit DWORD at the start of the `dmlh` chunk body inside
    /// `LIST odml` per OpenDML 2.0 §5.0 "Extended AVI Header"
    /// (`docs/container/riff/opendml-avi-2.0.pdf`): the single DWORD
    /// `dwTotalFrames` is "the real total frame count across every
    /// `RIFF AVIX` segment", whereas `avih.dwTotalFrames` only counts
    /// the primary segment.
    ///
    /// The two counts can legitimately disagree in edge cases the
    /// auto-derived value can't reach: a writer that knows ahead of
    /// time the full sequence length (a fixed-budget capture that
    /// pre-allocates a target frame count, an edit-list trimming the
    /// physical packet stream, a streamer rounding to a known
    /// playlist boundary), a chained AVIX continuation file that was
    /// emitted by a separate process and concatenated post-hoc, or
    /// a fuzz / regression fixture deliberately exercising the
    /// demuxer's `super_index_duration_violations` cross-check
    /// against a stamped mismatch. Use [`Self::with_dmlh_total_frames`].
    ///
    /// Passing `0` stamps a structurally-present `dmlh` chunk whose
    /// body is the zero DWORD: the typed
    /// `AviDemuxer::dmlh_total_frames()` returns `Some(0)` and the
    /// `avi:total_frames_all_segments` metadata key is emitted as
    /// `"0"`. The absence-vs-zero distinction in this surface is
    /// *whether the chunk is emitted at all* — controlled by the
    /// envelope variant ([`AviKind::OpenDml`] always emits `dmlh`;
    /// [`AviKind::Avi10`] never does) — not the value stamped.
    ///
    /// Only meaningful in [`AviKind::OpenDml`] mode (AVI 1.0 doesn't
    /// emit `LIST odml`); ignored in [`AviKind::Avi10`].
    ///
    /// The override only changes the 32-bit byte stamp inside `dmlh`;
    /// it does NOT touch `avih.dwTotalFrames` (the primary-segment
    /// count, derived from the video stream's `packet_count`), does
    /// NOT touch any per-stream `strh.dwLength` (round-229 / its own
    /// override surface), and does NOT alter any downstream
    /// `idx1` / `ix##` derivation, so a stamp that disagrees with
    /// the actual segment frame totals is internally inconsistent on
    /// purpose and will surface through
    /// `super_index_duration_violations()` on re-demux.
    pub dmlh_total_frames: Option<u32>,
}

/// Per-stream override values for the OpenDML 2.0 `vprp` Video
/// Properties Header (round-4 P2 / round-10 candidate 2). All fields
/// are optional; a zero value falls back to the round-3 default the
/// muxer already emits for that field.
///
/// Per OpenDML 2.0 §5.0 the spec defines four well-known
/// `(VideoFormatToken, VideoStandard)` pairs — the helpers
/// [`VprpConfig::ntsc`] / [`VprpConfig::pal`] / [`VprpConfig::secam`]
/// fill in the well-known refresh rates so callers don't have to
/// remember the table.
///
/// Round-10 C2: the trailing per-field `VIDEO_FIELD_DESC[]` array
/// (whose first-line `VideoYValidStartLine` differs between PAL
/// 23 / 335 vs NTSC 23 / 285 — the round-9 muxer hard-coded a
/// PAL-flavoured `half_height + 23`) can now be supplied verbatim
/// by a caller via [`VprpConfig::with_field_descs`]. Empty `Vec` →
/// the muxer synthesises the rect array from `nbFieldPerFrame` /
/// frame dimensions exactly like round-9 (back-compat).
#[derive(Clone, Debug, Default)]
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
    /// Optional caller-supplied `VIDEO_FIELD_DESC[]` records (round-10
    /// C2). When non-empty, the muxer emits these verbatim instead of
    /// synthesising the per-field rects from frame dimensions. Length
    /// must be `>= nb_field_per_frame.max(1)` for the override to
    /// take effect (the muxer slices to the active field count); a
    /// shorter Vec is ignored and the synthesised default is used so
    /// a partial override doesn't silently truncate the array.
    pub field_descs: Vec<VprpFieldDescOverride>,
}

/// Caller-supplied per-field rectangle for a `vprp` chunk's trailing
/// `VIDEO_FIELD_DESC[]` array (round-10 candidate 2). 8 DWORDs / 32 B
/// per OpenDML 2.0 §5.0.
///
/// Use this struct when the muxer's synthesised per-field rects
/// (PAL-flavoured `half_height + 23` second-line) don't match the
/// signal-shape conventions of the file's broadcast standard — most
/// notably NTSC, where the bottom field starts at line 285 (= 263 + 22),
/// not at PAL's 335 (= 312 + 23). The shape mirrors
/// [`oxideav_avi::demuxer::VprpFieldDesc`] field-for-field so a
/// re-mux of a parsed file can round-trip the signal-domain offsets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VprpFieldDescOverride {
    /// `CompressedBMHeight` in lines.
    pub compressed_bm_height: u32,
    /// `CompressedBMWidth` in pixels.
    pub compressed_bm_width: u32,
    /// `ValidBMHeight` in lines (visible height).
    pub valid_bm_height: u32,
    /// `ValidBMWidth` in pixels (visible width).
    pub valid_bm_width: u32,
    /// `ValidBMXOffset` — x-offset of the visible rect inside the
    /// compressed bitmap.
    pub valid_bm_x_offset: u32,
    /// `ValidBMYOffset` — y-offset of the visible rect inside the
    /// compressed bitmap.
    pub valid_bm_y_offset: u32,
    /// `VideoXOffsetInT` — x-offset of the bitmap inside the video
    /// signal's horizontal active region (in `T` units).
    pub video_x_offset_in_t: u32,
    /// `VideoYValidStartLine` — first signal line of this field
    /// within the total `dwVTotalInLines` count. PAL: 23 / 335.
    /// NTSC: 23 / 285. Progressive: 0.
    pub video_y_valid_start_line: u32,
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
            field_descs: Vec::new(),
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
            field_descs: Vec::new(),
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
            field_descs: Vec::new(),
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
    /// Builder: pin the trailing `VIDEO_FIELD_DESC[]` array verbatim
    /// (round-10 candidate 2). Use this when the synthesised PAL-
    /// flavoured `half_height + 23` second-line default doesn't match
    /// the file's broadcast standard — most notably NTSC, where the
    /// bottom field starts at line 285. The muxer slices the override
    /// to the active `nbFieldPerFrame.max(1)` count; supply at least
    /// that many records or the override is ignored (back-compat with
    /// pre-round-10 callers that never supplied any).
    ///
    /// ```ignore
    /// // NTSC 720x480 with spec-correct first-line offsets:
    /// VprpConfig::ntsc()
    ///     .with_field_descs(vec![
    ///         VprpFieldDescOverride {
    ///             compressed_bm_height: 240, compressed_bm_width: 720,
    ///             valid_bm_height: 240, valid_bm_width: 720,
    ///             video_y_valid_start_line: 23,
    ///             ..Default::default()
    ///         },
    ///         VprpFieldDescOverride {
    ///             compressed_bm_height: 240, compressed_bm_width: 720,
    ///             valid_bm_height: 240, valid_bm_width: 720,
    ///             video_y_valid_start_line: 285,
    ///             ..Default::default()
    ///         },
    ///     ])
    /// ```
    pub fn with_field_descs(mut self, descs: Vec<VprpFieldDescOverride>) -> Self {
        self.field_descs = descs;
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

    /// Builder helper: raise the OpenDML super-index slot reserve
    /// past the default 256 slots (round-6 candidate 3). Values
    /// below the [`OPENDML_SUPER_INDEX_MIN_CAPACITY`] floor are
    /// dropped (the default 256 stays in effect). Only meaningful
    /// for [`AviKind::OpenDml`]; ignored for `AviKind::Avi10`.
    pub fn with_super_index_capacity(mut self, n: usize) -> Self {
        self.super_index_capacity = if n >= OPENDML_SUPER_INDEX_MIN_CAPACITY {
            Some(n)
        } else {
            None
        };
        self
    }

    /// Builder helper: append a single `LIST INFO` sub-chunk
    /// `(id, value)` (round-6 candidate 2). `id` is a 4-byte
    /// `INFO` FourCC (e.g. `*b"INAM"` for title) per the AVI 1.0
    /// spec's `LIST INFO` registry. `value` is stored verbatim and
    /// NUL-terminated on the wire. Calling the builder multiple
    /// times with the same `id` appends both — `LIST INFO`'s
    /// shape is a flat list, not a map. An empty `value` skips
    /// the entry.
    pub fn with_info(mut self, id: [u8; 4], value: impl Into<String>) -> Self {
        let v = value.into();
        if !v.is_empty() {
            self.info_entries.push((id, v));
        }
        self
    }

    /// Builder helper: emit `LIST INFO` as a sibling of `LIST hdrl`
    /// rather than nested inside `hdrl` (round-11 candidate 1).
    /// Both layouts are spec-compliant per the AVI 1.0 reference;
    /// the sibling placement matches the recommended layout in
    /// Microsoft's Multimedia File Reference and several modern
    /// authoring tools. Default is `false` (nested-in-hdrl, the
    /// round-6 default). The demuxer recognises both layouts so
    /// either selection round-trips byte-equally on the metadata
    /// payload. No-op when [`Self::with_info`] was never called.
    pub fn with_top_level_info(mut self, on: bool) -> Self {
        self.info_top_level = on;
        self
    }

    /// Builder helper: stamp `bits` verbatim into `avih.dwFlags`
    /// (round-12 candidate 2). Replaces the round-6 default of
    /// `AVIF_HASINDEX | AVIF_TRUSTCKTYPE` (`0x0000_0810`) with the
    /// caller's exact value. Use the constants from
    /// [`crate::demuxer`]'s `AVIF_*` namespace
    /// (`AVIF_HASINDEX | AVIF_ISINTERLEAVED | AVIF_WASCAPTUREFILE`,
    /// etc.) per Microsoft's `vfw.h`.
    ///
    /// To OR in additional flags on top of the muxer's default
    /// instead of replacing it, see [`Self::with_avih_flag_bit`].
    pub fn with_avih_flags(mut self, bits: u32) -> Self {
        self.avih_flags_override = Some(bits);
        self
    }

    /// Builder helper: OR a single `AVIF_*` bit into the muxer's
    /// default `avih.dwFlags` (round-12 candidate 2). Convenience over
    /// [`Self::with_avih_flags`] for the common case of "default plus
    /// one extra bit" — e.g. `with_avih_flag_bit(AVIF_ISINTERLEAVED)`
    /// to keep the default `AVIF_HASINDEX | AVIF_TRUSTCKTYPE` and
    /// additionally set the interleaved-streams hint.
    ///
    /// Repeated calls keep accumulating bits. Calling
    /// [`Self::with_avih_flags`] AFTER this method overwrites all
    /// accumulated bits (caller picks the final value).
    pub fn with_avih_flag_bit(mut self, bit: u32) -> Self {
        let base = self.avih_flags_override.unwrap_or(DEFAULT_AVIH_FLAGS);
        self.avih_flags_override = Some(base | bit);
        self
    }

    /// Builder helper: toggle the `AVIF_HASINDEX` bit in
    /// `avih.dwFlags` (round-13 candidate 3). Convenience over
    /// [`Self::with_avih_flag_bit`] / [`Self::with_avih_flags`] so
    /// callers don't have to import the bit constants. Pass `true`
    /// to OR the bit on top of the running flags value, `false` to
    /// mask it back out. The starting baseline is the current
    /// override (or [`DEFAULT_AVIH_FLAGS`] when none was set).
    pub fn with_has_index(mut self, on: bool) -> Self {
        let base = self.avih_flags_override.unwrap_or(DEFAULT_AVIH_FLAGS);
        self.avih_flags_override = Some(if on {
            base | crate::demuxer::AVIF_HASINDEX
        } else {
            base & !crate::demuxer::AVIF_HASINDEX
        });
        self
    }

    /// Builder helper: toggle `AVIF_MUSTUSEINDEX` in `avih.dwFlags`
    /// (round-13 candidate 3). See [`Self::with_has_index`] for the
    /// semantics.
    pub fn with_must_use_index(mut self, on: bool) -> Self {
        let base = self.avih_flags_override.unwrap_or(DEFAULT_AVIH_FLAGS);
        self.avih_flags_override = Some(if on {
            base | crate::demuxer::AVIF_MUSTUSEINDEX
        } else {
            base & !crate::demuxer::AVIF_MUSTUSEINDEX
        });
        self
    }

    /// Builder helper: toggle `AVIF_ISINTERLEAVED` in `avih.dwFlags`
    /// (round-13 candidate 3). See [`Self::with_has_index`] for the
    /// semantics.
    pub fn with_is_interleaved(mut self, on: bool) -> Self {
        let base = self.avih_flags_override.unwrap_or(DEFAULT_AVIH_FLAGS);
        self.avih_flags_override = Some(if on {
            base | crate::demuxer::AVIF_ISINTERLEAVED
        } else {
            base & !crate::demuxer::AVIF_ISINTERLEAVED
        });
        self
    }

    /// Builder helper: toggle `AVIF_TRUSTCKTYPE` in `avih.dwFlags`
    /// (round-13 candidate 3). See [`Self::with_has_index`] for the
    /// semantics.
    pub fn with_trust_ck_type(mut self, on: bool) -> Self {
        let base = self.avih_flags_override.unwrap_or(DEFAULT_AVIH_FLAGS);
        self.avih_flags_override = Some(if on {
            base | crate::demuxer::AVIF_TRUSTCKTYPE
        } else {
            base & !crate::demuxer::AVIF_TRUSTCKTYPE
        });
        self
    }

    /// Builder helper: toggle `AVIF_WASCAPTUREFILE` in
    /// `avih.dwFlags` (round-13 candidate 3). See
    /// [`Self::with_has_index`] for the semantics.
    pub fn with_was_capture_file(mut self, on: bool) -> Self {
        let base = self.avih_flags_override.unwrap_or(DEFAULT_AVIH_FLAGS);
        self.avih_flags_override = Some(if on {
            base | crate::demuxer::AVIF_WASCAPTUREFILE
        } else {
            base & !crate::demuxer::AVIF_WASCAPTUREFILE
        });
        self
    }

    /// Builder helper: toggle `AVIF_COPYRIGHTED` in `avih.dwFlags`
    /// (round-13 candidate 3). See [`Self::with_has_index`] for the
    /// semantics.
    pub fn with_copyrighted(mut self, on: bool) -> Self {
        let base = self.avih_flags_override.unwrap_or(DEFAULT_AVIH_FLAGS);
        self.avih_flags_override = Some(if on {
            base | crate::demuxer::AVIF_COPYRIGHTED
        } else {
            base & !crate::demuxer::AVIF_COPYRIGHTED
        });
        self
    }

    /// Builder helper: stamp `n` verbatim into
    /// `avih.dwSuggestedBufferSize` instead of letting the muxer
    /// compute the hint from the observed peak chunk size (round-13
    /// candidate 2). Per AVI 1.0 §3.1 the field is a read-ahead
    /// allocation hint — pass the value your capture pipeline already
    /// reserves per packet to skip the muxer's own walk over the
    /// per-track `max_chunk_size` table. The default (`None`) makes
    /// the muxer pick `max(per_track_max)` rounded up to the next
    /// 4-byte boundary.
    pub fn with_suggested_buffer_size(mut self, n: u32) -> Self {
        self.suggested_buffer_size_override = Some(n);
        self
    }

    /// Builder helper: stamp `n` verbatim into
    /// `avih.dwMaxBytesPerSec` instead of letting the muxer compute
    /// the value from observed per-track byte totals (round-14
    /// candidate 1). Per AVI 1.0 §3.1 the field is the approximate
    /// maximum data rate the file requires (used by capture-card
    /// players to size their disk-read pacing). The default (`None`)
    /// makes the muxer compute the value in `write_trailer` from
    /// `sum(per_track_total_bytes) / file_duration_seconds`, where
    /// `file_duration_seconds = avih.dwTotalFrames *
    /// avih.dwMicroSecPerFrame / 1_000_000`. Returns `0` when the
    /// duration can't be derived (no video stream / zero frame count
    /// / zero microseconds-per-frame).
    pub fn with_max_bytes_per_sec(mut self, n: u32) -> Self {
        self.max_bytes_per_sec_override = Some(n);
        self
    }

    /// Builder helper: set a per-stream `dwMaxBytesPerSec`-style cap
    /// (round-18 candidate 1). `stream_index` is the 0-based stream
    /// ordinal; `bytes_per_sec` is the ceiling
    /// [`AviMuxer::write_trailer`] compares the stream's observed
    /// `total_bytes / file_duration_seconds` against. Streams not
    /// passed to this builder have no per-track cap (the file-wide
    /// [`Self::with_max_bytes_per_sec`] still applies). Repeated
    /// calls for the same `stream_index` replace the prior cap.
    /// `bytes_per_sec == 0` removes any prior cap for that stream.
    ///
    /// On breach, the muxer surfaces every offending track via
    /// [`AviMuxer::over_budget_streams`] (a `(stream_idx,
    /// observed_bps, cap)` triple). Pair with
    /// [`Self::with_strict_per_stream_budget`] to promote the breach
    /// into a hard `write_trailer` error instead.
    pub fn with_per_stream_max_bytes_per_sec(
        mut self,
        stream_index: u32,
        bytes_per_sec: u32,
    ) -> Self {
        self.per_stream_max_bytes_per_sec
            .retain(|(idx, _)| *idx != stream_index);
        if bytes_per_sec > 0 {
            self.per_stream_max_bytes_per_sec
                .push((stream_index, bytes_per_sec));
        }
        self
    }

    /// Builder helper: promote per-stream budget breaches to a hard
    /// `Error::InvalidData` in [`AviMuxer::write_trailer`] (round-18
    /// candidate 1). Pass `true` to opt in; default is `false`
    /// (lenient surfacing via [`AviMuxer::over_budget_streams`]).
    /// Only meaningful when at least one
    /// [`Self::with_per_stream_max_bytes_per_sec`] entry was
    /// registered.
    pub fn with_strict_per_stream_budget(mut self, on: bool) -> Self {
        self.strict_per_stream_budget = on;
        self
    }

    /// Builder helper: rebuild `idx1` from the primary segment's
    /// `ix##` entries instead of from the running per-packet
    /// `IndexEntry` collection (round-16 candidate 1). See
    /// [`Self::synthesise_idx1_from_ix`] for the rationale.
    /// `true` opts in; `false` (the default) keeps the round-3
    /// idx1-from-packets path. Only meaningful for
    /// [`AviKind::OpenDml`]; ignored for `AviKind::Avi10`.
    pub fn synthesise_idx1_from_ix(mut self, on: bool) -> Self {
        self.synthesise_idx1_from_ix = on;
        self
    }

    /// Builder helper: enable mid-`movi` `ix##` index emit for
    /// `stream_index` (round-7 candidate 1). The muxer flushes an
    /// inline standard-index chunk (`ix##`) every `packets_per_flush`
    /// packets while writing into the open `movi` LIST, in addition
    /// to the segment-tail flush. Per OpenDML 2.0 §"Index Locations
    /// in RIFF File", inline `ix##` chunks are spec-blessed for
    /// timecode streams and any stream where consumers benefit from
    /// scrubbing the index without first walking to the segment end.
    /// `packets_per_flush == 0` disables the periodic flush;
    /// `packets_per_flush == 1` flushes after every packet (one entry
    /// per `ix##`, i.e. an inline index per chunk). Calling the
    /// builder twice for the same `stream_index` replaces the prior
    /// cadence. Only meaningful for [`AviKind::OpenDml`].
    pub fn with_mid_movi_index(mut self, stream_index: u32, packets_per_flush: u32) -> Self {
        self.mid_movi_index_streams
            .retain(|(i, _)| *i != stream_index);
        if packets_per_flush > 0 {
            self.mid_movi_index_streams
                .push((stream_index, packets_per_flush));
        }
        self
    }

    /// Builder helper: mark `stream_index` as a top-down DIB video
    /// stream (round-19 candidate 1). The muxer stamps a negative
    /// `biHeight` in that stream's BMIH `strf` payload per VfW
    /// `wingdi.h` §"biHeight sign rules" (negative ⇒ origin at
    /// upper-left). Only takes effect for uncompressed RGB streams
    /// — compressed FourCCs MUST use positive `biHeight` per the
    /// same VfW section, so the flag is silently dropped for them
    /// (see [`Self::top_down_video_streams`]). Duplicate calls for
    /// the same `stream_index` are deduplicated.
    pub fn with_top_down_video(mut self, stream_index: u32) -> Self {
        if !self.top_down_video_streams.contains(&stream_index) {
            self.top_down_video_streams.push(stream_index);
        }
        self
    }

    /// Builder helper: register `stream_index` as a
    /// `WAVE_FORMAT_EXTENSIBLE` audio stream (round-75). On
    /// `write_header`, the muxer emits a 40-byte
    /// `WAVEFORMATEXTENSIBLE` `strf` payload instead of the legacy
    /// 18-byte `WAVEFORMATEX` per Microsoft `mmreg.h` §
    /// "WAVEFORMATEXTENSIBLE": the trailing 22-byte extension carries
    /// `(channel_mask, valid_bps, subformat_guid)` so consumers that
    /// honour the extensible shape can resolve channel layout, actual
    /// sample precision, and codec identity from the file alone
    /// rather than relying on per-codec defaults.
    ///
    /// Use the constants in [`crate::stream_format`] for the GUID
    /// (e.g. `KSDATAFORMAT_SUBTYPE_PCM`, `_IEEE_FLOAT`, `_ALAW`,
    /// `_MULAW`, `_ADPCM`, `_MPEG`, `_DRM`) when emitting one of the
    /// canonical Microsoft `KSDATAFORMAT_SUBTYPE_*` codecs. For
    /// stream channel layouts, the docs README maps the most common
    /// `dwChannelMask` values:
    /// - mono → `0x00004` (`SPEAKER_FRONT_CENTER`)
    /// - stereo → `0x00003` (`FL | FR`)
    /// - 5.1 (Microsoft) → `0x0003F`
    /// - 7.1 → `0x0063F`
    ///
    /// `valid_bps` is the WAVEFORMATEXTENSIBLE
    /// `Samples.wValidBitsPerSample` field — the actual sample
    /// precision; for 24-bit-in-32-bit-container PCM, set
    /// `valid_bps = 24` and let the underlying WAVEFORMATEX-side
    /// `bits_per_sample` (derived from `params.sample_format` /
    /// codec_id) hold the container size of 32. For codecs whose
    /// container size equals the precision (e.g. PCM `s16le`), use
    /// the same value for both.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Repeated builder chaining is supported.
    pub fn with_extensible_audio(
        mut self,
        stream_index: u32,
        channel_mask: u32,
        valid_bps: u16,
        subformat: crate::stream_format::Guid,
    ) -> Self {
        self.extensible_audio_streams
            .retain(|(idx, _, _, _)| *idx != stream_index);
        self.extensible_audio_streams
            .push((stream_index, channel_mask, valid_bps, subformat));
        self
    }

    /// Builder helper: attach a human-readable name to `stream_index`
    /// (round-80). The muxer emits a `strn` chunk inside that stream's
    /// `strl` LIST per AVI 1.0 §"AVI Stream Headers"; the body is the
    /// UTF-8 bytes of `name` followed by a single NUL terminator, with
    /// one extra zero pad byte when the resulting length is odd per
    /// RIFF §"data is always padded to nearest WORD boundary".
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Passing an empty name emits an empty (NUL-only) `strn`
    /// body which the demuxer interprets as "no name" — call this
    /// builder with a non-empty name when the intent is to round-trip
    /// the value.
    pub fn with_stream_name(mut self, stream_index: u32, name: impl Into<String>) -> Self {
        let name = name.into();
        self.stream_names.retain(|(idx, _)| *idx != stream_index);
        self.stream_names.push((stream_index, name));
        self
    }

    /// Builder helper: attach an opaque codec-driver configuration blob
    /// to `stream_index` (round-89). The muxer emits a `strd` chunk
    /// inside that stream's `strl` LIST per AVI 1.0 §"AVI Stream
    /// Headers" (docs/container/riff/avi-riff-file-reference.md
    /// §"AVI Stream Headers"); the body is `bytes` verbatim, with one
    /// extra zero pad byte when the length is odd per RIFF §"data is
    /// always padded to nearest WORD boundary".
    ///
    /// Per the spec: "The format and content of this chunk are defined
    /// by the codec driver. Typically, drivers use this information
    /// for configuration. Applications that read and write AVI files
    /// do not need to interpret this information; they simple transfer
    /// it to and from the driver as a memory block." The muxer
    /// therefore performs no interpretation — callers pass whatever
    /// bytes their codec driver expects.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Passing an empty `Vec` emits an empty (`cb=0`) `strd`
    /// chunk which the demuxer surfaces as `Some(&[])` so empty-but-
    /// present can be distinguished from absent. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_header_data`] for a
    /// round-trip.
    pub fn with_stream_header_data(mut self, stream_index: u32, bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        self.stream_header_data
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_header_data.push((stream_index, bytes));
        self
    }

    /// Builder helper: override the `strh.rcFrame` destination rectangle
    /// for `stream_index` (round-115). The four signed WORDs are written
    /// little-endian in `[left, top, right, bottom]` order at byte offset
    /// 48 of the 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER".
    ///
    /// Per the spec the rect positions a text or video stream within the
    /// movie rectangle (`avih.dwWidth` × `dwHeight`); units are pixels and
    /// the origin is the movie rectangle's upper-left corner. Without an
    /// override the muxer writes `0,0,width,height` for video streams and
    /// all-zero for non-video streams; this builder lets a caller place a
    /// picture-in-picture second video stream or a subtitle overlay box at
    /// an arbitrary sub-rectangle.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_frame_rect`] for a round-trip;
    /// note the demuxer maps an all-zero rect back to `None`, so an
    /// override of `0,0,0,0` reads as "no rect" on re-demux.
    pub fn with_stream_frame_rect(
        mut self,
        stream_index: u32,
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    ) -> Self {
        self.stream_frame_rects
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_frame_rects
            .push((stream_index, [left, top, right, bottom]));
        self
    }

    /// Builder helper: stamp a `wLanguage` LANGID into the 56-byte
    /// AVISTREAMHEADER for `stream_index` (round-119). The 16-bit value
    /// is written little-endian at byte offset 14 of the strh per AVI
    /// 1.0 §"AVISTREAMHEADER" (`wLanguage` row).
    ///
    /// Per the spec the field is a language tag (BCP 47 / RFC 1766 /
    /// similar) but the staged docs note that AVI does **not**
    /// normatively pin a registry. Microsoft writers conventionally
    /// pack a Win32 LANGID; the muxer writes whatever 16-bit value the
    /// caller supplies verbatim and does not validate against any
    /// registry. Passing `0` is equivalent to omitting the override —
    /// the demuxer maps the all-zero default back to `None` so a stamp
    /// of `0` reads as "no language tag" on re-demux.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_language`] for a round-trip
    /// of any non-zero LANGID.
    pub fn with_stream_language(mut self, stream_index: u32, langid: u16) -> Self {
        self.stream_languages
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_languages.push((stream_index, langid));
        self
    }

    /// Builder helper: stamp a `dwInitialFrames` skew into the 56-byte
    /// AVISTREAMHEADER for `stream_index` (round-153). The 32-bit value
    /// is written little-endian at byte offset 16 of the strh per AVI
    /// 1.0 §"AVISTREAMHEADER" (`dwInitialFrames` row).
    ///
    /// Per the spec the field is the per-stream interleave skew:
    /// *"How far audio data is skewed ahead of the video frames in
    /// interleaved files. Typically, this is about 0.75 seconds. If
    /// creating interleaved files, set the value of this member to the
    /// number of frames in the file prior to the initial frame of the
    /// AVI sequence in this member."* The unit is the stream's own
    /// `dwRate` / `dwScale` tick (typically frames for video, blocks
    /// for audio); the muxer writes whatever 32-bit value the caller
    /// supplies verbatim and does not validate it against the per-stream
    /// `dwLength`. Passing `0` is equivalent to omitting the override —
    /// the demuxer maps the all-zero default back to `None` (per
    /// AVIMAINHEADER §`dwInitialFrames`: *"Noninterleaved files should
    /// specify zero."*) so a stamp of `0` reads as "no skew" on
    /// re-demux.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_initial_frames`] for a
    /// round-trip of any non-zero skew.
    pub fn with_stream_initial_frames(mut self, stream_index: u32, initial_frames: u32) -> Self {
        self.stream_initial_frames
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_initial_frames
            .push((stream_index, initial_frames));
        self
    }

    /// Builder helper: stamp a `dwQuality` quality indicator into the
    /// 56-byte AVISTREAMHEADER for `stream_index` (round-176). The
    /// 32-bit value is written little-endian at byte offset 40 of the
    /// strh per AVI 1.0 §"AVISTREAMHEADER" (`dwQuality` row in
    /// `docs/container/riff/avi-riff-file-reference.md` line 246).
    ///
    /// Per the spec the field is the per-stream quality indicator:
    /// *"Indicator of the quality of the data in the stream. Quality
    /// is represented as a number between 0 and 10,000. For compressed
    /// data, this typically represents the value of the quality
    /// parameter passed to the compression software. If set to -1,
    /// drivers use the default quality value."* The documented range
    /// is `[0, 10_000]` but the muxer writes whatever 32-bit value the
    /// caller supplies verbatim and does **not** clamp or normalise —
    /// anomalous out-of-spec writers (capture drivers stamping
    /// full-precision quality scores etc.) round-trip exactly.
    ///
    /// Passing `0xFFFF_FFFF` (= `-1` as i32) is equivalent to omitting
    /// the override — the demuxer maps the documented `-1` "use default
    /// driver quality" sentinel back to `None` (per AVI 1.0
    /// §"AVISTREAMHEADER" `dwQuality` row: *"If set to -1, drivers use
    /// the default quality value."*) so a stamp of `-1` reads as "no
    /// quality recorded" on re-demux.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_quality`] for a round-trip
    /// of any non-default-sentinel quality.
    pub fn with_stream_quality(mut self, stream_index: u32, quality: u32) -> Self {
        self.stream_qualities
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_qualities.push((stream_index, quality));
        self
    }

    /// Builder helper: stamp a `wPriority` selection-hint DWORD into
    /// the 56-byte AVISTREAMHEADER for `stream_index` (round-182). The
    /// 16-bit value is written little-endian at byte offset 12 of the
    /// strh per AVI 1.0 §"AVISTREAMHEADER" (`wPriority` row in
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix B
    /// line 238).
    ///
    /// Per the spec the field is a per-stream selection hint: *"Priority
    /// of a stream type. For example, in a file with multiple audio
    /// streams, the one with the highest priority might be the default
    /// stream."* The spec does not normatively pin a value range or a
    /// tie-break rule, so the muxer writes whatever 16-bit value the
    /// caller supplies verbatim and does **not** clamp or normalise —
    /// applications that use the field for ad-hoc tagging round-trip
    /// exactly.
    ///
    /// Passing `0` is equivalent to omitting the override — the
    /// demuxer maps the `0` legacy writer default back to `None` (the
    /// muxer has stamped a zero priority since round-3) so a stamp of
    /// `0` reads as "no priority recorded" on re-demux.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_priority`] for a round-trip
    /// of any non-zero selection hint.
    pub fn with_stream_priority(mut self, stream_index: u32, priority: u16) -> Self {
        self.stream_priorities
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_priorities.push((stream_index, priority));
        self
    }

    /// Builder helper: stamp a `dwStart` starting-time DWORD into the
    /// 56-byte AVISTREAMHEADER for `stream_index` (round-203). The
    /// 32-bit value is written little-endian at byte offset 28 of the
    /// strh per AVI 1.0 §"AVISTREAMHEADER" (`dwStart` row in
    /// `docs/container/riff/avi-riff-file-reference.md` line 243).
    ///
    /// Per the spec the field is the stream-local starting time:
    /// *"Starting time for this stream. The units are defined by the
    /// dwRate and dwScale members in the main file header. Usually,
    /// this is zero, but it can specify a delay time for a stream that
    /// does not start concurrently with the file."* The unit is the
    /// stream's own `(dwRate / dwScale)` tick (so frames for video,
    /// samples-or-blocks for audio) and the demuxer surfaces the raw
    /// 32-bit DWORD verbatim — the muxer writes whatever value the
    /// caller supplies and does not validate it against the per-stream
    /// `dwLength`.
    ///
    /// Passing `0` is equivalent to omitting the override — the demuxer
    /// maps the `0` legacy writer default back to `None` (the spec-
    /// documented "starts concurrently with the file" value) so a
    /// stamp of `0` reads as "no start offset recorded" on re-demux,
    /// mirroring the round-182 `wPriority` / round-176 `dwQuality` /
    /// round-153 `dwInitialFrames` / round-119 `wLanguage` "default ==
    /// absent" convention.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_start`] for a round-trip of
    /// any non-zero start offset.
    pub fn with_stream_start(mut self, stream_index: u32, start: u32) -> Self {
        self.stream_starts.retain(|(idx, _)| *idx != stream_index);
        self.stream_starts.push((stream_index, start));
        self
    }

    /// Builder helper: stamp an `fccHandler` driver-handler FourCC into
    /// the 56-byte AVISTREAMHEADER for `stream_index` (round-210). The
    /// 4 bytes are written verbatim at byte offset 4 of the strh per
    /// AVI 1.0 §"AVISTREAMHEADER" (`fccHandler` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, Appendix B
    /// line 236).
    ///
    /// Per the spec the field is the *optional FOURCC that identifies
    /// a specific data handler*. For video streams the packaging
    /// layer's default is to mirror `BITMAPINFOHEADER.biCompression`
    /// (so an `MJPG` video stream gets `fccHandler = b"MJPG"`,
    /// matching what most writers in the wild do); for audio streams
    /// the default is the all-zero `\0\0\0\0` "no preferred handler"
    /// value (the demuxer maps that back to `None`). This builder
    /// overrides whichever default applied — useful when re-muxing a
    /// capture whose original writer set a driver-suite identifier
    /// in fccHandler that differs from biCompression, when stamping
    /// an explicit driver hint on an audio stream, or when
    /// explicitly zeroing the field on a video stream whose original
    /// writer left it empty.
    ///
    /// Passing `[0, 0, 0, 0]` is equivalent to omitting the override
    /// for audio streams (the audio default is also all-zero) — the
    /// demuxer maps the all-zero default back to `None`, mirroring
    /// the round-203 `dwStart` / round-182 `wPriority` / round-176
    /// `dwQuality` / round-153 `dwInitialFrames` / round-119
    /// `wLanguage` / round-115 `rcFrame` "default == absent"
    /// convention. On a video stream `[0, 0, 0, 0]` explicitly
    /// zeroes the field, overriding the `biCompression`-mirror
    /// default.
    ///
    /// The muxer writes the 4 bytes verbatim and does not validate
    /// printability — the spec's *optional FOURCC* phrasing does
    /// not normatively pin it.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_handler`] for a round-trip
    /// of any non-zero driver-handler FourCC.
    pub fn with_stream_handler(mut self, stream_index: u32, fourcc: [u8; 4]) -> Self {
        self.stream_handlers.retain(|(idx, _)| *idx != stream_index);
        self.stream_handlers.push((stream_index, fourcc));
        self
    }

    /// Builder helper: stamp an `fccType` FOURCC into the 56-byte
    /// AVISTREAMHEADER for `stream_index` (round-253). The 4 bytes
    /// are written verbatim at byte offset 0 of the strh per AVI 1.0
    /// §"AVISTREAMHEADER" (`fccType` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, Appendix B
    /// line 235 + the `fcc` row at line 234 documenting the standard
    /// `auds` / `mids` / `txts` / `vids` values: *"A FOURCC code that
    /// specifies the type of data contained in the stream. The
    /// following standard AVI values are defined: `auds` (audio
    /// stream), `mids` (MIDI stream), `txts` (text stream), `vids`
    /// (video stream)."*).
    ///
    /// Without an override the muxer keeps its packaging-derived
    /// default (`vids` for video streams, `auds` for audio streams,
    /// per `packaging::StrfEntry::strh_type`). The override is useful
    /// for re-muxing a stream under a non-default FOURCC (e.g.
    /// stamping `mids` on a stream that's been packaged as audio for
    /// a MIDI-aware downstream tool), or for stamping a non-standard
    /// vendor FOURCC.
    ///
    /// The muxer writes the 4 bytes verbatim and does NOT validate
    /// printability or membership in the spec-documented
    /// `{auds, mids, txts, vids}` set — the spec does not pin a
    /// closed registry. Passing `[0, 0, 0, 0]` stamps the all-zero
    /// sentinel the demuxer maps back to `None`, mirroring the
    /// round-247 `dwFlags` / round-229 `dwLength` / round-210
    /// `fccHandler` "default == absent" convention this crate has
    /// carried since round-115.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_fcc_type`] for a
    /// round-trip raw-FOURCC surface.
    pub fn with_stream_fcc_type(mut self, stream_index: u32, fcc_type: [u8; 4]) -> Self {
        self.stream_fcc_types
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_fcc_types.push((stream_index, fcc_type));
        self
    }

    /// Builder helper: stamp a `dwSuggestedBufferSize` read-ahead hint
    /// into the 56-byte AVISTREAMHEADER for `stream_index` (round-217).
    /// The 32-bit value is written verbatim at byte offset 36 of the
    /// strh per AVI 1.0 §"AVISTREAMHEADER" (`dwSuggestedBufferSize`
    /// row in `docs/container/riff/avi-riff-file-reference.md`, line
    /// 245).
    ///
    /// Per the spec the field is *"How large a buffer should be used
    /// to read this stream. Typically, this contains a value
    /// corresponding to the largest chunk present in the stream.
    /// Using the correct buffer size makes playback more efficient.
    /// Use zero if you do not know the correct buffer size."* Without
    /// an override the muxer keeps its long-standing auto-derived
    /// default (`t.max_chunk_size` — the largest body it observed on
    /// that stream during `write_packet` calls, patched into the strh
    /// at the end of `write_trailer`). This builder overrides that
    /// default — useful for re-muxing a file whose original writer
    /// over-declared a peak the actual largest chunk doesn't match,
    /// for forcing the `0` "do not know" sentinel (so the demuxer
    /// round-trips the absent-hint case via
    /// [`crate::demuxer::AviDemuxer::stream_suggested_buffer_size`]
    /// `== None`), or for matching a downstream player's read-ahead
    /// budget exactly.
    ///
    /// Passing `0` stamps the spec-documented "do not know" sentinel
    /// — the demuxer maps that back to `None`, mirroring the
    /// round-210 `fccHandler` / round-203 `dwStart` / round-182
    /// `wPriority` / round-176 `dwQuality` / round-153
    /// `dwInitialFrames` / round-119 `wLanguage` / round-115
    /// `rcFrame` "default == absent" convention.
    ///
    /// The muxer writes the 32-bit value verbatim and does NOT
    /// validate it against the actual largest chunk observed in
    /// `movi` — over-declaration is the documented intent of the
    /// field and a caller that wants strict equality should pass the
    /// max-chunk value explicitly.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_suggested_buffer_size`]
    /// for a round-trip of any non-zero hint.
    pub fn with_stream_suggested_buffer_size(
        mut self,
        stream_index: u32,
        suggested_buffer_size: u32,
    ) -> Self {
        self.stream_suggested_buffer_sizes
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_suggested_buffer_sizes
            .push((stream_index, suggested_buffer_size));
        self
    }

    /// Builder helper: stamp a `dwSampleSize` indicator into the 56-byte
    /// AVISTREAMHEADER for `stream_index` (round-222). The 32-bit value
    /// is written verbatim at byte offset 44 of the strh per AVI 1.0
    /// §"AVISTREAMHEADER" (`dwSampleSize` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 247).
    ///
    /// Per the spec the field is *"The size of a single sample of data.
    /// This is set to zero if the samples can vary in size. If this
    /// number is nonzero, then multiple samples of data can be grouped
    /// into a single chunk within the file. If it is zero, each sample
    /// of data (such as a video frame) must be in a separate chunk.
    /// For video streams, this number is typically zero, although it
    /// can be nonzero if all video frames are the same size. For audio
    /// streams, this number should be the same as the nBlockAlign
    /// member of the WAVEFORMATEX structure describing the audio."*
    ///
    /// Without an override the muxer keeps its long-standing
    /// packaging-derived default: audio streams get the `nBlockAlign`
    /// byte size for PCM / CBR audio and `0` for VBR audio (MP3 / AAC
    /// / MPEG); video streams get `0` (one frame per chunk). This
    /// builder overrides that default — useful for re-muxing a file
    /// whose original writer stamped a non-canonical value (a
    /// fixed-frame-size raw-yuv recorder that wrote `dwSampleSize =
    /// frame_bytes` instead of leaving it `0` for the video stream;
    /// an audio stream that needs to match a downstream player's idea
    /// of `nBlockAlign` exactly), for forcing the `0` "samples can
    /// vary in size" sentinel back (so the demuxer round-trips the
    /// absent-hint case via
    /// [`crate::demuxer::AviDemuxer::stream_sample_size`] `== None`),
    /// or for reproducing a pathological writer for fuzz / regression
    /// purposes.
    ///
    /// Passing `0` stamps the spec-documented "samples can vary in
    /// size" sentinel — the demuxer maps that back to `None`,
    /// mirroring the round-217 `dwSuggestedBufferSize` / round-210
    /// `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
    /// round-176 `dwQuality` / round-153 `dwInitialFrames` /
    /// round-119 `wLanguage` / round-115 `rcFrame` "default ==
    /// absent" convention.
    ///
    /// The muxer writes the 32-bit value verbatim at byte offset 44
    /// and does NOT change the muxer's own `dwLength` derivation
    /// (which keeps using the packaging-derived `entry.sample_size`
    /// for the audio `size / sample_size` formula); a caller that
    /// stamps a `dwSampleSize` incompatible with their packet stream
    /// is creating an internally-inconsistent file on purpose, and
    /// will need [`crate::demuxer::open_avi_lenient`] to read it back
    /// (the round-14 C2 audio sample-size invariant rejects VBR/CBR
    /// mismatches by default).
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_sample_size`] for a
    /// round-trip of any non-zero hint.
    pub fn with_stream_sample_size(mut self, stream_index: u32, sample_size: u32) -> Self {
        self.stream_sample_sizes
            .retain(|(idx, _)| *idx != stream_index);
        self.stream_sample_sizes.push((stream_index, sample_size));
        self
    }

    /// Builder helper: stamp a per-stream `strh.dwLength` override
    /// (round-229). See [`Self::stream_lengths`] for the full
    /// semantics. The muxer writes `length` verbatim into the 32-bit
    /// DWORD at byte offset 32 of the named stream's 56-byte
    /// AVISTREAMHEADER at the [`AviMuxer::write_trailer`] /
    /// `patch_post_counts` site, replacing the auto-derived
    /// per-stream packet / sample count.
    ///
    /// Per AVI 1.0 §"AVISTREAMHEADER" (`dwLength` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 244):
    /// *"Length of this stream. The units are defined by the dwRate
    /// and dwScale members of the stream's header."* The unit is the
    /// stream's own `(dwRate / dwScale)` tick — frames for video,
    /// samples-or-blocks for audio — and the muxer writes whatever
    /// 32-bit value the caller supplies with no rate-conversion or
    /// validation. Passing `0` stamps the de-facto "no length
    /// declared" value — the demuxer maps that back to `None`.
    ///
    /// Useful for reproducing legacy writers whose stamped `dwLength`
    /// disagrees with their actual chunk count (half-written capture
    /// dumps, fixed-budget streamers that round to a playlist
    /// boundary, fuzz / regression fixtures), and for forcing the `0`
    /// "no length declared" value back so the demuxer round-trips the
    /// absent-length case via
    /// [`crate::demuxer::AviDemuxer::stream_length`] `== None`.
    ///
    /// The override only changes the byte stamp at offset 32; it does
    /// NOT touch `avih.dwTotalFrames` (the per-stream length and the
    /// file-global total are spec-independent fields) and does NOT
    /// alter any downstream `idx1` / `ix##` / `dmlh` derivation — a
    /// caller that stamps a `dwLength` incompatible with the file's
    /// actual packet count is creating an internally inconsistent
    /// file on purpose. The `StreamInfo::duration` the demuxer
    /// surfaces via [`oxideav_core::Demuxer::streams`] will reflect
    /// the stamped value (the framework already derives duration from
    /// this same DWORD).
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_length`] for a round-trip
    /// of any value (including the `0` "no length declared" stamp,
    /// which round-trips as `None`).
    pub fn with_stream_length(mut self, stream_index: u32, length: u32) -> Self {
        self.stream_lengths.retain(|(idx, _)| *idx != stream_index);
        self.stream_lengths.push((stream_index, length));
        self
    }

    /// Builder helper: stamp a `dwFlags` DWORD into the 56-byte
    /// AVISTREAMHEADER for `stream_index` (round-247). The 32-bit
    /// value is written little-endian at byte offset 8 of the strh
    /// per AVI 1.0 §"AVISTREAMHEADER" (`dwFlags` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 237).
    ///
    /// The spec's *dwFlags values* table at lines 252–255 documents
    /// two bits:
    ///
    /// - [`crate::demuxer::AVISF_DISABLED`] (`0x0000_0001`):
    ///   *"Indicates this stream should not be enabled by default."*
    /// - [`crate::demuxer::AVISF_VIDEO_PALCHANGES`] (`0x0001_0000`):
    ///   *"Indicates this video stream contains palette changes.
    ///   This flag warns the playback software that it will need to
    ///   animate the palette."*
    ///
    /// The muxer writes whatever 32-bit value the caller supplies
    /// verbatim and does **not** validate against the documented set
    /// — so a caller may stamp vendor-extension / driver-private bits
    /// in the upper half-DWORD that the spec does not pin, and they
    /// round-trip exactly through
    /// [`crate::demuxer::AviDemuxer::stream_flags`].
    ///
    /// Passing `0` is equivalent to omitting the override — the
    /// demuxer maps the `0` legacy writer default back to `None` (the
    /// pre-round-247 muxer behaviour) so a stamp of `0` reads as
    /// "no flags recorded" on re-demux, mirroring the round-229
    /// `dwLength` / round-222 `dwSampleSize` / round-217
    /// `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    /// `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    /// round-153 `dwInitialFrames` / round-119 `wLanguage` /
    /// round-115 `rcFrame` "default == absent" convention.
    ///
    /// The override is byte-stamp-only: it does NOT touch
    /// `avih.dwFlags` (the file-global `AVIF_*` flags handled
    /// independently via [`Self::with_avih_flags`] and friends), and
    /// it does NOT cross-validate against other strh fields (e.g.
    /// stamping `AVISF_VIDEO_PALCHANGES` on an audio stream is
    /// internally inconsistent on purpose). Duplicate calls for the
    /// same `stream_index` replace the prior entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_flags`] and
    /// [`crate::demuxer::AviDemuxer::stream_flags_typed`] for the
    /// raw and typed-decode round-trip surfaces.
    pub fn with_stream_flags(mut self, stream_index: u32, flags: u32) -> Self {
        self.stream_flags.retain(|(idx, _)| *idx != stream_index);
        self.stream_flags.push((stream_index, flags));
        self
    }

    /// Builder helper: stamp a `(scale, rate)` timebase pair into the
    /// 56-byte AVISTREAMHEADER for `stream_index` (round-249). The two
    /// 32-bit values are written little-endian at byte offsets 20 and
    /// 24 of the strh per AVI 1.0 §"AVISTREAMHEADER" (`dwScale` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 241 +
    /// the `dwRate` row line 242).
    ///
    /// Per the spec text: *"Used with dwRate to specify the time scale
    /// that this stream will use. Dividing dwRate by dwScale gives the
    /// number of samples per second. For video streams, this is the
    /// frame rate. For audio streams, this rate corresponds to the
    /// time needed to play nBlockAlign bytes of audio, which for PCM
    /// audio is the just the sample rate."*
    ///
    /// Without an override the muxer keeps its packaging-derived
    /// default — for video streams the per-stream `frame_rate` pair,
    /// for audio streams the `sample_rate / 1` pair, both mirroring
    /// the framework's [`oxideav_core::StreamInfo::time_base`]. The
    /// override replaces both DWORDs verbatim at the byte-stamp site;
    /// it does NOT alter the muxer's `(scale, rate)`-derived
    /// `dwLength` computation for audio streams (which still uses the
    /// packaging-derived `t.entry.{scale,rate}` to convert the running
    /// sample count into `dwLength` units), does NOT touch
    /// `avih.dwMicroSecPerFrame` (the file-global frame-rate hint,
    /// which the muxer derives independently from the first video
    /// stream's packaging pair), and does NOT cross-validate against
    /// the per-stream `dwLength` or `dwStart`. Stamping an audio
    /// sample-rate pair on a video stream is internally inconsistent
    /// on purpose; a `0` in either DWORD stamps the writer-skips-it /
    /// mathematically-undefined sentinel the demuxer maps back to
    /// `None`.
    ///
    /// Duplicate calls for the same `stream_index` replace the prior
    /// entry. Pairs with
    /// [`crate::demuxer::AviDemuxer::stream_timebase`] for a
    /// round-trip raw-DWORD surface.
    pub fn with_stream_timebase(mut self, stream_index: u32, scale: u32, rate: u32) -> Self {
        self.stream_timebases
            .retain(|(idx, _, _)| *idx != stream_index);
        self.stream_timebases.push((stream_index, scale, rate));
        self
    }

    /// Builder helper: enable stream-aligned packet emission with
    /// `n`-byte granularity (round-92). See
    /// [`Self::padding_granularity`] for semantics. `n` must be a
    /// power of two in `[2, 65536]`; other values reset the field to
    /// `None` (no padding, the legacy `avih.dwPaddingGranularity = 0`
    /// behaviour). The common useful values are 512 (filesystem
    /// sector), 2048 (CD-ROM sector), and 4096 (modern filesystem
    /// page).
    pub fn with_padding_granularity(mut self, n: u32) -> Self {
        self.padding_granularity = if (2..=65536).contains(&n) && n.is_power_of_two() {
            Some(n)
        } else {
            None
        };
        self
    }

    /// Builder helper: stamp the file-global `avih.dwInitialFrames`
    /// interleave skew (round-157). See [`Self::initial_frames`] for
    /// semantics. The muxer writes `n` verbatim into the 32-bit
    /// DWORD at byte offset 16 of the 56-byte AVIMAINHEADER body
    /// (byte 24 of the `avih` chunk).
    ///
    /// Per AVI 1.0 §"AVIMAINHEADER" (line 200,
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A):
    /// *"Initial frame for interleaved files. Noninterleaved files
    /// should specify zero. If creating interleaved files, specify
    /// the number of frames in the file prior to the initial frame
    /// of the AVI sequence."* Passing `0` is equivalent to omitting
    /// the override — the demuxer maps the all-zero default back to
    /// `None` so a stamp of `0` reads as "no skew" on re-demux.
    ///
    /// File-global counterpart of the per-stream
    /// [`Self::with_stream_initial_frames`] (round-153). Pairs with
    /// [`crate::demuxer::AviDemuxer::initial_frames`] for a
    /// round-trip of any non-zero skew.
    pub fn with_initial_frames(mut self, n: u32) -> Self {
        self.initial_frames = Some(n);
        self
    }

    /// Builder helper: stamp a digitization-date `IDIT` chunk into
    /// `LIST hdrl` (round-107). See [`Self::digitization_date`] for
    /// placement and byte-layout semantics.
    ///
    /// `IDIT` is the RIFF *Hdrl Tags* `DateTimeOriginal` field per
    /// `docs/container/riff/metadata/exiftool-riff-tags.html`. The
    /// staged docs do not pin a canonical text format; pass whatever
    /// timestamp string the consuming workflow expects (e.g. the
    /// `asctime` form `"Wed Jan 02 02:03:55 2002"` capture hardware
    /// emits, or an ISO-8601 `"2002-01-02T02:03:55"`). The string is
    /// written verbatim (plus a NUL terminator) and round-trips
    /// byte-faithfully through
    /// [`crate::demuxer::AviDemuxer::digitization_date`].
    ///
    /// Duplicate calls replace the prior value. Passing an empty string
    /// emits a NUL-only `IDIT` body, which the demuxer reads back as
    /// `None` (no usable timestamp) — call this with a non-empty string
    /// when the intent is to round-trip a value.
    pub fn with_digitization_date(mut self, date: impl Into<String>) -> Self {
        self.digitization_date = Some(date.into());
        self
    }

    /// Builder helper: stamp a SMPTE-timecode `ISMP` chunk into
    /// `LIST hdrl` (round-112). See [`Self::smpte_timecode`] for
    /// placement and byte-layout semantics.
    ///
    /// `ISMP` is the RIFF *Hdrl Tags* `TimeCode` field per
    /// `docs/container/riff/metadata/exiftool-riff-tags.html`, the
    /// sibling of the `IDIT` `DateTimeOriginal` field. The staged docs
    /// do not pin a canonical text format; pass whatever timecode string
    /// the consuming workflow expects (e.g. the SMPTE non-drop-frame
    /// colon form `"01:00:00:00"`, the drop-frame semicolon form
    /// `"01:00:00;02"`, or a fractional `"01:00:00.50"`). The string is
    /// written verbatim (plus a NUL terminator) and round-trips
    /// byte-faithfully through
    /// [`crate::demuxer::AviDemuxer::smpte_timecode`].
    ///
    /// Duplicate calls replace the prior value. Passing an empty string
    /// emits a NUL-only `ISMP` body, which the demuxer reads back as
    /// `None` (no usable timecode) — call this with a non-empty string
    /// when the intent is to round-trip a value.
    pub fn with_smpte_timecode(mut self, timecode: impl Into<String>) -> Self {
        self.smpte_timecode = Some(timecode.into());
        self
    }

    /// Builder helper: stamp the OpenDML 2.0 `dmlh.dwTotalFrames`
    /// extended-header count (round-234). See
    /// [`Self::dmlh_total_frames`] for placement and byte-layout
    /// semantics.
    ///
    /// Per OpenDML 2.0 §5.0 ("Extended AVI Header",
    /// `docs/container/riff/opendml-avi-2.0.pdf`): `dmlh`'s single
    /// DWORD body is "the real total frame count across every
    /// `RIFF AVIX` segment" — i.e. the cross-segment truth that
    /// `avih.dwTotalFrames` (primary segment only) can't carry.
    /// The muxer writes `n` verbatim into the `dmlh` body at the
    /// `write_trailer` patch site, replacing the auto-derived
    /// `total_video_frames` default. Only meaningful in
    /// [`AviKind::OpenDml`] mode; in [`AviKind::Avi10`] mode no
    /// `LIST odml` is emitted so the override has nothing to stamp.
    ///
    /// Duplicate calls replace the prior value. Passing `0` stamps
    /// a structurally-present `dmlh` chunk with a zero body — the
    /// demuxer reads it as `Some(0)` and emits the
    /// `avi:total_frames_all_segments` metadata key as `"0"`. The
    /// absence-vs-zero distinction is the chunk's presence (driven
    /// by the envelope variant), not the value stamped.
    pub fn with_dmlh_total_frames(mut self, n: u32) -> Self {
        self.dmlh_total_frames = Some(n);
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

/// One snapshot record taken from the primary segment for the
/// round-16 C1 idx1-from-ix synthesiser. We capture enough metadata
/// at packet-write time to rebuild a 16-byte idx1 entry without
/// re-walking `tracks[*].ix_entries` (which gets drained by
/// `flush_ix_for_track` long before `serialize_idx1` runs).
#[derive(Clone, Copy, Debug)]
struct PrimaryIxSnapshot {
    /// Chunk FourCC: `00dc` / `00wb` / `00tx` / `00pc` / etc.
    /// Mirrors the wire bytes the demuxer's idx1 parser expects.
    ckid: [u8; 4],
    /// Pre-baked idx1 flags DWORD (AVIIF_KEYFRAME for keyframe
    /// packets, plus AVIIF_FIRSTPART|AVIIF_LASTPART when the
    /// packet's stream is registered as 2-field per round-6 C1).
    flags: u32,
    /// Offset relative to the start of the primary `movi` LIST
    /// body (i.e. of the `"movi"` form-type FourCC) — same shape
    /// as [`IndexEntry::offset`].
    offset: u32,
    /// Payload size in bytes (high keyframe-bit cleared).
    size: u32,
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
        // Round-19 C1: honour `top_down_video_streams` for `BI_RGB`
        // (uncompressed RGB) video streams; the helper itself drops
        // the flag for compressed FourCCs.
        let top_down = options
            .top_down_video_streams
            .iter()
            .any(|&idx| idx as usize == i);
        // Round-75: honour `extensible_audio_streams` for audio
        // streams that want a 40-byte WAVEFORMATEXTENSIBLE `strf`
        // payload instead of the legacy 18-byte WAVEFORMATEX. The
        // helper itself drops the flag for non-audio streams (i.e.
        // it's a no-op on a video stream registered with this opt).
        let extensible = options
            .extensible_audio_streams
            .iter()
            .find(|(idx, _, _, _)| (*idx as usize) == i)
            .map(|(_, mask, valid, guid)| (*mask, *valid, *guid));
        let entry = build_strf(&s.params, top_down, extensible)?;
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
        current_segment_indexed_packets: 0,
        rec_open_size_off: None,
        rec_packets_in_cluster: 0,
        rec_bytes_in_cluster: 0,
        pending_field2_offset: None,
        primary_ix_snapshot: Vec::new(),
        over_budget_streams: Vec::new(),
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
    /// LIST, summed across all streams. Retained for diagnostics; the
    /// super-index `dwDuration` uses [`Self::indexed_packet_count`]
    /// instead (round-101).
    #[allow(dead_code)]
    packet_count: u32,
    /// Number of packets carried in this segment's `movi` LIST for the
    /// indexed stream (stream 0). Becomes `dwDuration` in the `indx`
    /// super-index entry per OpenDML 2.0 §"AVI Super Index Chunk" —
    /// "time span in stream ticks" of the chunks the segment's `ix##`
    /// indexes. For a one-tick-per-frame video stream this is the
    /// segment's frame count, matching OpenDML's intent even when the
    /// file carries additional audio / data streams (round-101 fixes
    /// the previous all-stream sum, which over-counted multi-stream
    /// files).
    indexed_packet_count: u32,
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
    /// Number of packets written into the current open segment's `movi`
    /// LIST for the **indexed** stream (stream 0 — the one that carries
    /// the `indx` super-index). Reset when a new segment is opened.
    /// Becomes the super-index entry's `dwDuration` per OpenDML 2.0
    /// §"AVI Super Index Chunk" ("time span in stream ticks" of the
    /// chunks indexed by that segment's `ix##`), which for a
    /// one-tick-per-frame video stream is that stream's per-segment
    /// frame count — *not* the all-stream packet total. Round-101.
    current_segment_indexed_packets: u32,
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
    /// Snapshot of every primary-segment `ix##` record, captured at
    /// `write_packet` / `write_sideband_chunk` time BEFORE
    /// [`Self::flush_ix_for_track`] drains the per-track buffer
    /// (round-16 candidate 1). Used by
    /// [`Self::serialize_idx1_from_ix`] to rebuild `idx1` when
    /// [`AviMuxOptions::synthesise_idx1_from_ix`] is on. Empty when
    /// the option is off (we don't pay the per-packet clone) or
    /// when the file is `AviKind::Avi10`.
    primary_ix_snapshot: Vec<PrimaryIxSnapshot>,
    /// Per-stream `dwMaxBytesPerSec` cap breaches detected at
    /// `write_trailer` time (round-18 candidate 1). Each entry is
    /// `(stream_index, observed_bps, cap_bps)` where `observed_bps`
    /// is the stream's `total_bytes * 1_000_000 / duration_micros`
    /// and `cap_bps` is the value passed to
    /// [`AviMuxOptions::with_per_stream_max_bytes_per_sec`].
    /// Populated regardless of `strict_per_stream_budget`; the
    /// strict flag only controls whether `write_trailer` ALSO
    /// returns an error on the first breach. Surfaced via
    /// [`AviMuxer::over_budget_streams`].
    over_budget_streams: Vec<(u32, u64, u32)>,
    header_written: bool,
    trailer_written: bool,
}

/// Default reserved slots in the OpenDML `indx` super-index. 256
/// slots is 4 KiB of payload and lets a 1-GiB-segment OpenDML file
/// index up to 256 GiB, which covers everything users need without
/// forcing up-front segment-count knowledge. Files with more than
/// the configured capacity still mux correctly — the trailing
/// entries simply don't land in the super-index (the demuxer falls
/// back to walking `RIFF AVIX` continuations).
///
/// To raise the reserve past the default, see
/// [`AviMuxOptions::with_super_index_capacity`] (round-6 candidate
/// 3). Round-4 / round-5 fixtures and tests assume this default.
pub const OPENDML_SUPER_INDEX_DEFAULT_CAPACITY: usize = 256;

/// Floor for [`AviMuxOptions::with_super_index_capacity`]. Below
/// this the default applies. 16 slots = 256 B payload, which is a
/// reasonable lower bound for tiny OpenDML test fixtures.
pub const OPENDML_SUPER_INDEX_MIN_CAPACITY: usize = 16;

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
        let avih = build_avih(
            &self.tracks,
            self.options.avih_flags_override,
            self.options.padding_granularity,
            self.options.initial_frames,
        );
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
        let indx_capacity = self
            .options
            .super_index_capacity
            .filter(|n| *n >= OPENDML_SUPER_INDEX_MIN_CAPACITY)
            .unwrap_or(OPENDML_SUPER_INDEX_DEFAULT_CAPACITY);
        for (i, t) in self.tracks.iter().enumerate() {
            let with_indx = want_indx && i == 0;
            let with_vprp = want_vprp && &t.entry.strh_type == b"vids";
            let vprp_override = self
                .options
                .vprp_overrides
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, c)| c.clone());
            let indx_2field_here = indx_is_2field && with_indx;
            // Round-80: optional `strn` chunk for this stream. The
            // first builder entry per stream index wins (see
            // `with_stream_name`'s retain-then-push pattern).
            let stream_name = self
                .options
                .stream_names
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, n)| n.as_str());
            // Round-89: optional `strd` codec-driver blob for this
            // stream (see `with_stream_header_data`'s retain-then-push
            // pattern).
            let stream_header_data = self
                .options
                .stream_header_data
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, b)| b.as_slice());
            // Round-115: optional `strh.rcFrame` override for this stream
            // (see `with_stream_frame_rect`'s retain-then-push pattern).
            let frame_rect_override = self
                .options
                .stream_frame_rects
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, r)| *r);
            // Round-119: optional `strh.wLanguage` LANGID override for
            // this stream (see `with_stream_language`'s retain-then-push
            // pattern). `None` keeps the legacy `0`
            // ("LANG_NEUTRAL / SUBLANG_NEUTRAL") default the demuxer
            // maps back to `None`.
            let language_override = self
                .options
                .stream_languages
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, l)| *l);
            // Round-153: optional `strh.dwInitialFrames` override for
            // this stream (see `with_stream_initial_frames`'s
            // retain-then-push pattern). `None` keeps the legacy `0`
            // ("noninterleaved file" per AVIMAINHEADER §`dwInitialFrames`)
            // default the demuxer maps back to `None`.
            let initial_frames_override = self
                .options
                .stream_initial_frames
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, f)| *f);
            // Round-176: optional `strh.dwQuality` override for this
            // stream (see `with_stream_quality`'s retain-then-push
            // pattern). `None` keeps the legacy `0xFFFF_FFFF` (-1, "use
            // default driver quality" per AVI 1.0 §"AVISTREAMHEADER"
            // `dwQuality` row) default the demuxer maps back to `None`.
            let quality_override = self
                .options
                .stream_qualities
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, q)| *q);
            // Round-182: optional `strh.wPriority` override for this
            // stream (see `with_stream_priority`'s retain-then-push
            // pattern). `None` keeps the legacy `0` writer default the
            // demuxer maps back to `None` (per AVI 1.0
            // §"AVISTREAMHEADER" Appendix B `wPriority` row).
            let priority_override = self
                .options
                .stream_priorities
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, p)| *p);
            // Round-203: optional `strh.dwStart` override for this
            // stream (see `with_stream_start`'s retain-then-push
            // pattern). `None` keeps the legacy `0` writer default
            // ("starts concurrently with the file" per AVI 1.0
            // §"AVISTREAMHEADER" `dwStart` row) the demuxer maps back
            // to `None`.
            let start_override = self
                .options
                .stream_starts
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, s)| *s);
            // Round-210: optional `strh.fccHandler` override for this
            // stream (see `with_stream_handler`'s retain-then-push
            // pattern). `None` keeps the packaging-derived default —
            // for video streams the per-codec FourCC (mirroring
            // `BITMAPINFOHEADER.biCompression`), for audio streams
            // the all-zero `\0\0\0\0` "no preferred handler" value
            // (per AVI 1.0 §"AVISTREAMHEADER" `fccHandler` row).
            let handler_override = self
                .options
                .stream_handlers
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, h)| *h);
            // Round-222: optional `strh.dwSampleSize` override for this
            // stream (see `with_stream_sample_size`'s retain-then-push
            // pattern). `None` keeps the packaging-derived default — for
            // audio streams the `nBlockAlign` byte size for PCM / CBR
            // audio and `0` for VBR audio (MP3 / AAC / MPEG); for video
            // streams `0` (one frame per chunk) — per AVI 1.0
            // §"AVISTREAMHEADER" `dwSampleSize` row.
            let sample_size_override = self
                .options
                .stream_sample_sizes
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, s)| *s);
            // Round-247: optional `strh.dwFlags` override for this
            // stream (see `with_stream_flags`'s retain-then-push
            // pattern). `None` keeps the legacy `0` writer default the
            // demuxer maps back to `None` (per AVI 1.0
            // §"AVISTREAMHEADER" `dwFlags` row + the *dwFlags values*
            // table at lines 252–255 carrying `AVISF_DISABLED` /
            // `AVISF_VIDEO_PALCHANGES`).
            let flags_override = self
                .options
                .stream_flags
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, f)| *f);
            // Round-249: optional `(strh.dwScale, strh.dwRate)`
            // timebase pair override for this stream (see
            // `with_stream_timebase`'s retain-then-push pattern).
            // `None` keeps the packaging-derived defaults
            // (`t.entry.scale` / `t.entry.rate`) the muxer has used
            // since round-3 — for video the per-stream `frame_rate`
            // pair, for audio the `sample_rate / 1` pair, per AVI 1.0
            // §"AVISTREAMHEADER" `dwScale` row line 241 + `dwRate`
            // row line 242.
            let timebase_override = self
                .options
                .stream_timebases
                .iter()
                .find(|(idx, _, _)| *idx == i as u32)
                .map(|(_, s, r)| (*s, *r));
            // Round-253: optional `strh.fccType` override for this
            // stream (see `with_stream_fcc_type`'s retain-then-push
            // pattern). `None` keeps the packaging-derived default —
            // `vids` for video streams, `auds` for audio streams (per
            // `packaging::StrfEntry::strh_type`).
            let fcc_type_override = self
                .options
                .stream_fcc_types
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, t)| *t);
            let (indx_count_off, indx_entries_off) = write_strl(
                self.output.as_mut(),
                i as u32,
                t,
                with_indx,
                with_vprp,
                vprp_override,
                indx_2field_here,
                indx_capacity,
                stream_name,
                stream_header_data,
                frame_rect_override,
                language_override,
                initial_frames_override,
                quality_override,
                priority_override,
                start_override,
                handler_override,
                sample_size_override,
                flags_override,
                timebase_override,
                fcc_type_override,
            )?;
            if with_indx {
                self.indx_entries_count_off = indx_count_off;
                self.indx_entries_start_off = indx_entries_off;
                self.indx_entries_capacity = indx_capacity;
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
        // Round-6 candidate 2: emit `LIST INFO` inside `hdrl` when
        // the caller registered any [`AviMuxOptions::with_info`]
        // entries and did NOT enable the top-level placement
        // (round-11 candidate 1). Placed after the strls (and after
        // `LIST odml` when present) so strl offsets stay stable for
        // `patch_post_counts`. Each child is a 4-CC chunk whose
        // payload is a NUL-terminated string per the AVI 1.0
        // `LIST INFO` registry — see `parse_info_list` in the
        // demuxer.
        let want_info = !self.options.info_entries.is_empty();
        if want_info && !self.options.info_top_level {
            write_info_list(self.output.as_mut(), &self.options.info_entries)?;
        }
        // Round-107: emit the `IDIT` digitization-date chunk inside
        // `hdrl` (RIFF *Hdrl Tags* namespace, `DateTimeOriginal`, per
        // docs/container/riff/metadata/exiftool-riff-tags.html). Placed
        // after the strls / `LIST odml` / nested `LIST INFO` so existing
        // strl offsets stay stable for `patch_post_counts`. Body is the
        // caller's UTF-8 string + a NUL terminator (RIFF word-pad
        // applied by `write_chunk` for odd lengths). The demuxer
        // (`parse_hdrl`'s `b"IDIT"` arm) reads it back verbatim.
        if let Some(ref date) = self.options.digitization_date {
            let mut body = date.as_bytes().to_vec();
            body.push(0);
            write_chunk(self.output.as_mut(), b"IDIT", &body)?;
        }
        // Round-112: emit the `ISMP` SMPTE-timecode chunk inside `hdrl`
        // (RIFF *Hdrl Tags* namespace, `TimeCode`, per
        // docs/container/riff/metadata/exiftool-riff-tags.html). Placed
        // after the strls / `LIST odml` / nested `LIST INFO` / `IDIT` so
        // existing strl offsets stay stable for `patch_post_counts`.
        // Body is the caller's UTF-8 string + a NUL terminator (RIFF
        // word-pad applied by `write_chunk` for odd lengths). The
        // demuxer (`parse_hdrl`'s `b"ISMP"` arm) reads it back verbatim.
        if let Some(ref timecode) = self.options.smpte_timecode {
            let mut body = timecode.as_bytes().to_vec();
            body.push(0);
            write_chunk(self.output.as_mut(), b"ISMP", &body)?;
        }
        finish_chunk(self.output.as_mut(), hdrl_size_off)?;
        // Round-11 candidate 1: emit `LIST INFO` as a SIBLING of
        // `LIST hdrl` (top-level child of the outer `RIFF AVI ` form).
        // Both placements are spec-compliant per the AVI 1.0
        // reference; sibling placement matches the recommended
        // layout in Microsoft's Multimedia File Reference and
        // several modern authoring tools. The demuxer (see
        // `walk_riff_body`'s `b"INFO" if is_primary` arm) recognises
        // either placement so the metadata payload round-trips
        // byte-equally regardless of which the muxer chose.
        if want_info && self.options.info_top_level {
            write_info_list(self.output.as_mut(), &self.options.info_entries)?;
        }

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
        self.current_segment_indexed_packets = 0;
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

        // Round-92: stream-aligned remux. If the caller asked for an
        // `avih.dwPaddingGranularity` of `n`, prepend a `JUNK` chunk
        // sized so the upcoming packet chunk's 8-byte header starts at
        // a file-absolute offset divisible by `n`. The JUNK body is
        // zero-filled; the AVI 1.0 reference (§"Other Data Chunks")
        // says applications must ignore its content. Sideband chunks
        // (`xxpc` / `xxtx` / `ix##` index emit / `rec ` open) are not
        // pre-aligned — only frame chunks honour the alignment.
        if let Some(n) = self.options.padding_granularity {
            // Measure pos before + after so `rec_bytes_in_cluster` (when
            // a `LIST rec ` cluster is open) still reflects the cluster's
            // true on-disk body size. JUNK chunks are accounted into the
            // cluster the same way packet chunks are.
            let before = self.output.stream_position()?;
            self.emit_padding_junk_for(packet.data.len(), n)?;
            let after = self.output.stream_position()?;
            if self.rec_open_size_off.is_some() {
                self.rec_bytes_in_cluster += after.saturating_sub(before);
            }
        }

        let fourcc = self.tracks[idx].packet_fourcc;
        // Record offset (relative to 'movi' fourcc) BEFORE writing the chunk.
        let chunk_off = self.output.stream_position()?;
        let rel_off_opt = chunk_off.checked_sub(self.movi_start_off);
        let size = packet.data.len() as u32;

        // Round-4 P1/P3: consume any pending field-2 offset signalled
        // by `set_field2_offset`. Always consume so a stray hook on a
        // non-2-field stream can't leak onto the next packet.
        let pending_field2 = self.pending_field2_offset.take();
        let stream_is_2field = self.options.field2_streams.contains(&(idx as u32));

        // idx1 flags. Per vfw.h `AVIIF_*`:
        //   AVIIF_KEYFRAME  = 0x0010
        //   AVIIF_FIRSTPART = 0x0020
        //   AVIIF_LASTPART  = 0x0040
        // Round-6 candidate 1: 2-field streams carry one chunk per
        // frame containing both fields back-to-back, i.e. the chunk
        // is BOTH the first and the last part of the frame. Setting
        // both bits (= 0x60) lets readers walking idx1 alone (no
        // ix## available) detect 2-field carriage from the index
        // entry's flags rather than parsing the OpenDML
        // super-index. The bits are additive with AVIIF_KEYFRAME
        // when the packet is a keyframe.
        let mut flags = if packet.flags.keyframe {
            AVIIF_KEYFRAME
        } else {
            0
        };
        if stream_is_2field {
            flags |= AVIIF_FIRSTPART | AVIIF_LASTPART;
        }

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
                    let ix_entry = IxStdEntry {
                        dw_offset: d as u32,
                        dw_size_with_flag,
                        dw_offset_field2,
                    };
                    self.tracks[idx].ix_entries.push(ix_entry);
                    // Round-16 C1: snapshot every primary-segment ix##
                    // record so `serialize_idx1_from_ix` can rebuild
                    // idx1 from the standard-index view. We only pay
                    // the per-packet snapshot when the option is on,
                    // and continuation segments (idx1 is 32-bit and
                    // primary-only) are skipped.
                    if self.options.synthesise_idx1_from_ix && self.segments.is_empty() {
                        // ix##.dw_offset is from qwBaseOffset =
                        // movi_start_off + 4 to chunk DATA, while
                        // idx1 wants the offset to the chunk HEADER
                        // from movi_start_off. So idx1.offset =
                        // ix##.dw_offset - 4.
                        let idx1_offset = (d as u32).saturating_sub(4);
                        self.primary_ix_snapshot.push(PrimaryIxSnapshot {
                            ckid: fourcc,
                            flags,
                            offset: idx1_offset,
                            size,
                        });
                    }
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
        // Sample count: for PCM (block_align > 0) we derive the frame
        // count from `size / block_align`; for VBR audio we prefer the
        // packet's `duration` field (round-5 candidate 3) so
        // `strh.dwLength` reflects per-packet sample budgets that can't
        // be reconstructed from a fixed block_align. Falls back to one
        // sample per packet (the round-3 behaviour) when neither is
        // available.
        t.sample_count += sample_count_of_packet(&t.stream, &t.entry, size, packet.duration);

        self.current_segment_packets += 1;
        // Track the indexed stream's per-segment frame count separately
        // so the `indx` super-index `dwDuration` reflects that stream's
        // span (OpenDML 2.0 §"AVI Super Index Chunk") rather than the
        // all-stream packet total (round-101). The super-index is always
        // emitted on stream 0 in `write_header`.
        if idx == 0 {
            self.current_segment_indexed_packets += 1;
        }
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

        // Round-7 candidate 1: mid-`movi` `ix##` periodic flush. When
        // the current stream is registered via
        // [`AviMuxOptions::with_mid_movi_index`] and the per-stream
        // pending entry count has reached the configured cadence,
        // emit an inline standard-index chunk right here (still
        // inside the open `movi` LIST). Per OpenDML 2.0 §"Index
        // Locations in RIFF File", inline `ix##` chunks are blessed
        // for streams whose consumers benefit from sub-segment
        // random access (timecode, sparse subtitles, ...). The
        // entries flushed inline are removed from the per-track
        // buffer so the segment-tail [`Self::flush_ix_chunks`] only
        // emits the residual tail (or nothing, if the cadence
        // divides the stream cleanly).
        if matches!(self.kind, AviKind::OpenDml(_)) {
            let cadence = self
                .options
                .mid_movi_index_streams
                .iter()
                .find(|(i, _)| *i == idx as u32)
                .map(|(_, n)| *n);
            if let Some(n) = cadence {
                if n > 0 && self.tracks[idx].ix_entries.len() as u32 >= n {
                    // If a `LIST rec ` cluster is open, close it first
                    // so the `ix##` chunk lands at the `movi` body
                    // level rather than nested inside the cluster.
                    // The next `write_packet` will open a fresh
                    // cluster on demand (the existing
                    // `rec_open_size_off.is_none()` check fires
                    // naturally).
                    if self.rec_open_size_off.is_some() {
                        self.close_rec_cluster()?;
                    }
                    self.flush_ix_for_track(idx)?;
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
                    indexed_packet_count: self.current_segment_indexed_packets,
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
                indexed_packet_count: self.current_segment_indexed_packets,
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

        // Round-18 candidate 1: per-stream `dwMaxBytesPerSec` cap
        // enforcement. Run AFTER `patch_post_counts` so the
        // duration-derivation source of truth (the first video
        // stream's micro_per_frame × packet_count) matches what we
        // stamped into `avih.dwMaxBytesPerSec`. Only fires when the
        // caller registered any
        // [`AviMuxOptions::with_per_stream_max_bytes_per_sec`]
        // entries; otherwise the per-track budget map is empty and
        // the loop is a no-op. With
        // `strict_per_stream_budget` set, the first breach fails the
        // trailer with `Error::InvalidData` (other breaches still
        // populate `over_budget_streams` so a caller catching the
        // error can still inspect the full set).
        if !self.options.per_stream_max_bytes_per_sec.is_empty() {
            self.compute_per_stream_budget_breaches();
            if self.options.strict_per_stream_budget {
                if let Some(&(idx, observed, cap)) = self.over_budget_streams.first() {
                    return Err(Error::invalid(format!(
                        "AVI: stream {idx} exceeded per-stream dwMaxBytesPerSec cap: \
                         observed={observed} cap={cap}"
                    )));
                }
            }
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

        // Round-14 candidate 1: avih.dwMaxBytesPerSec sits at body
        // offset 4 (the second u32 of AVIMAINHEADER) → file offset
        // 12 + 12 + 8 + 4 = 36. Per AVI 1.0 §3.1 it's the approximate
        // maximum data rate (bytes/sec) the file requires; capture-card
        // players use it to size their disk-read pacing. Compute as
        // `sum(per_track_total_bytes) / file_duration_seconds`, where
        // `file_duration_seconds = total_video_frames * micro_sec_per_frame /
        // 1_000_000`, or honour the caller's
        // [`AviMuxOptions::with_max_bytes_per_sec`] override.
        //
        // Pre-round-14 we hard-coded 0 here; conformant AVI 1.0 readers
        // (and capture players in particular) treat 0 as "rate unknown"
        // and fall back to a worst-case allocation heuristic. Populating
        // a real value lets them right-size the read pacing budget.
        let max_bytes_per_sec = self.options.max_bytes_per_sec_override.unwrap_or_else(|| {
            // Pull dwMicroSecPerFrame from the first video stream
            // (matches `build_avih`'s source of truth). Audio-only
            // files have no video stream → no usable
            // micro_sec_per_frame and we fall back to summing
            // per-stream WAVEFORMATEX `nAvgBytesPerSec` (round-15
            // candidate 2). Per AVI 1.0 §3.1, the field is the
            // approximate maximum data rate the file requires; for
            // audio-only files the per-stream `avg_bytes_per_sec`
            // is exactly the right pacing budget (a CBR sum gives
            // the wire rate; a VBR codec's `avg_bytes_per_sec` is
            // its declared average and the spec-blessed proxy when
            // no peak is known).
            let micro_per_frame: u64 = self
                .tracks
                .iter()
                .find(|t| &t.entry.strh_type == b"vids")
                .map(|t| {
                    let scale = t.entry.scale.max(1) as u64;
                    let rate = t.entry.rate.max(1) as u64;
                    1_000_000u64 * scale / rate
                })
                .unwrap_or(0);
            let total_bytes: u64 = self.tracks.iter().map(|t| t.total_bytes).sum();
            let micros: u64 = (total_video_frames as u64).saturating_mul(micro_per_frame);
            if micros == 0 || total_bytes == 0 {
                // Round-15 C2: audio-only fallback. Sum every audio
                // track's WAVEFORMATEX `nAvgBytesPerSec` (strf body
                // bytes 8..12, LE). Returns 0 when there are no
                // audio tracks (or each one had a zero
                // avg_bytes_per_sec, which most CBR PCM tracks
                // never do but a misconfigured VBR encoder may).
                self.tracks
                    .iter()
                    .filter(|t| &t.entry.strh_type == b"auds")
                    .filter_map(|t| audio_strf_avg_bytes_per_sec(&t.entry.strf))
                    .fold(0u32, |acc, v| acc.saturating_add(v))
            } else {
                // bytes_per_sec = total_bytes * 1_000_000 / micros
                // Use u128 for the intermediate to avoid overflow
                // on multi-GiB long-form captures (e.g. 4 GB at
                // 1 hour ≈ 1.16 MB/s — but the multiplication
                // factor 1_000_000 inflates a u64 sum past 2^64
                // for ~18 GB total bytes).
                let big = (total_bytes as u128) * 1_000_000u128;
                let bps = big / (micros as u128);
                // Clamp to u32::MAX — dwMaxBytesPerSec is a DWORD,
                // and a real-world file exceeding 4 GiB/s would
                // be wildly out of spec for AVI 1.0 anyway.
                bps.min(u32::MAX as u128) as u32
            }
        });
        self.output.seek(SeekFrom::Start(36))?;
        self.output.write_all(&max_bytes_per_sec.to_le_bytes())?;

        // Round-13 candidate 2: avih.dwSuggestedBufferSize sits at
        // body offset 28 → file offset 12 + 12 + 8 + 28 = 60. Per
        // AVI 1.0 §3.1 it's the recommended read-ahead allocation
        // hint, i.e. the largest chunk body a player should expect
        // to read in one shot. Compute as `max(per_track max_chunk_size)`
        // rounded up to the next 4-byte boundary, or honour the
        // caller's [`AviMuxOptions::with_suggested_buffer_size`]
        // override.
        let suggested_buffer_size =
            self.options
                .suggested_buffer_size_override
                .unwrap_or_else(|| {
                    let max_track = self
                        .tracks
                        .iter()
                        .map(|t| t.max_chunk_size)
                        .max()
                        .unwrap_or(0);
                    // Round up to a 4-byte boundary; saturate so a u32::MAX
                    // packet doesn't wrap silently.
                    max_track.saturating_add(3) & !3u32
                });
        self.output.seek(SeekFrom::Start(60))?;
        self.output
            .write_all(&suggested_buffer_size.to_le_bytes())?;

        // First strl LIST starts at the file offset right after the avih
        // chunk: 12 + 12 + 8 + 56 = 88 ... wait, but the avih body is
        // 56 B → avih chunk = 64 B → first strl LIST starts at
        //   12 (RIFF preamble) + 12 (hdrl LIST preamble) + 64 (avih chunk)
        // = 88.  However for the OpenDML envelope the first stream's
        // strl ALSO contains an indx chunk after strf, so the second
        // stream's strl starts an extra
        //   (8 + 24 + 16*indx_entries_capacity) bytes later.
        let mut strl_off: u64 = 88;
        let opendml = matches!(self.kind, AviKind::OpenDml(_));
        for (i, t) in self.tracks.iter().enumerate() {
            let strh_body_off = strl_off + 20;
            // strh.dwLength is at body offset 32 → file offset strh_body_off + 32.
            //
            // Round-229: an `AviMuxOptions::with_stream_length` override
            // (when present) wins over the long-standing auto-derived
            // per-stream packet / sample count. Per AVI 1.0
            // §"AVISTREAMHEADER" (`dwLength` row in
            // `docs/container/riff/avi-riff-file-reference.md`, line 244):
            // *"Length of this stream. The units are defined by the dwRate
            // and dwScale members of the stream's header."* The unit is
            // the stream's own `(dwRate / dwScale)` tick — frames for
            // video, samples-or-blocks for audio per the existing
            // packaging convention below — and the muxer writes whatever
            // 32-bit value the caller supplied verbatim. The override
            // does NOT touch `avih.dwTotalFrames`, nor any downstream
            // `idx1` / `ix##` / `dmlh` derivation — a caller that
            // stamps a `dwLength` incompatible with their actual chunk
            // count is creating an internally inconsistent file on
            // purpose (e.g. to reproduce a half-written legacy capture
            // dump or a fixed-budget streamer's playlist-boundary stamp).
            let length_override = self
                .options
                .stream_lengths
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, n)| *n);
            let length = length_override.unwrap_or_else(|| {
                if &t.entry.strh_type == b"auds" {
                    // For PCM we store sample_count (frames). For VBR we'd
                    // normally use packet count, but we don't support VBR audio
                    // in the mux yet.
                    t.sample_count as u32
                } else {
                    t.packet_count
                }
            });
            self.output.seek(SeekFrom::Start(strh_body_off + 32))?;
            self.output.write_all(&length.to_le_bytes())?;

            // Also patch strh.dwSuggestedBufferSize at body offset 36.
            // Round-217: an `AviMuxOptions::with_stream_suggested_buffer_size`
            // override (when present) wins over the long-standing
            // auto-derived `t.max_chunk_size` default. Per AVI 1.0
            // §"AVISTREAMHEADER" (`dwSuggestedBufferSize` row in
            // `docs/container/riff/avi-riff-file-reference.md` line 245)
            // the field is *"How large a buffer should be used to read
            // this stream. Typically, this contains a value corresponding
            // to the largest chunk present in the stream. Using the
            // correct buffer size makes playback more efficient. Use zero
            // if you do not know the correct buffer size."* The
            // pre-round-217 muxer wrote `t.max_chunk_size` unconditionally
            // (the spec's "largest chunk present" recommendation); the
            // override lets a caller stamp a different hint — including
            // the `0` "do not know" sentinel — for round-trippability of
            // legacy capture files.
            let strh_sbs_override = self
                .options
                .stream_suggested_buffer_sizes
                .iter()
                .find(|(idx, _)| *idx == i as u32)
                .map(|(_, n)| *n);
            let strh_sbs = strh_sbs_override.unwrap_or(t.max_chunk_size);
            self.output.seek(SeekFrom::Start(strh_body_off + 36))?;
            self.output.write_all(&strh_sbs.to_le_bytes())?;

            // Advance strl_off by the size of this strl LIST (8 header +
            // body). Body = 4 (form) + 64 (strh) + 8 + strf.len() + pad
            // [+ 8 + indx_payload_padded if i == 0 and opendml]
            // [+ 8 + vprp_payload_padded if video stream and opendml].
            let strf_padded = t.entry.strf.len() + (t.entry.strf.len() & 1);
            let mut strl_body = 4 + 64 + 8 + strf_padded;
            if opendml && i == 0 {
                let indx_payload = 24 + 16 * self.indx_entries_capacity;
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
        //
        // Round-234: an `AviMuxOptions::with_dmlh_total_frames`
        // override replaces the auto-derived value at this patch site.
        // The two counts can legitimately disagree in edge cases the
        // auto-derived value can't reach (a writer that knows the full
        // sequence length ahead of time, a chained AVIX continuation
        // emitted by a separate process and concatenated post-hoc, a
        // fuzz / regression fixture exercising the demuxer's
        // `super_index_duration_violations` cross-check); a stamp
        // that disagrees with the actual segment frame totals is
        // internally inconsistent on purpose and surfaces through
        // that demuxer accessor on re-demux.
        if let Some(off) = self.dmlh_total_frames_off {
            let dmlh_value = self.options.dmlh_total_frames.unwrap_or(total_video_frames);
            self.output.seek(SeekFrom::Start(off))?;
            self.output.write_all(&dmlh_value.to_le_bytes())?;
            self.output.seek(SeekFrom::Start(end_pos))?;
        }
        Ok(())
    }

    /// Serialize the idx1 body for the primary segment.
    ///
    /// When [`AviMuxOptions::synthesise_idx1_from_ix`] is set AND the
    /// muxer is in OpenDML mode AND a non-empty primary `ix##`
    /// snapshot has been captured, builds idx1 from those records via
    /// [`Self::serialize_idx1_from_ix`]. Otherwise emits the
    /// running per-packet [`IndexEntry`] collection (the round-3
    /// default).
    fn serialize_idx1(&self) -> Vec<u8> {
        if self.options.synthesise_idx1_from_ix
            && matches!(self.kind, AviKind::OpenDml(_))
            && !self.primary_ix_snapshot.is_empty()
        {
            return self.serialize_idx1_from_ix();
        }
        let mut idx_body = Vec::with_capacity(self.index.len() * 16);
        for e in &self.index {
            idx_body.extend_from_slice(&e.ckid);
            idx_body.extend_from_slice(&e.flags.to_le_bytes());
            idx_body.extend_from_slice(&e.offset.to_le_bytes());
            idx_body.extend_from_slice(&e.size.to_le_bytes());
        }
        idx_body
    }

    /// Round-16 candidate 1: rebuild the primary segment's idx1 body
    /// from the captured per-packet `ix##` standard-index records
    /// instead of the running [`IndexEntry`] collection. One 16-B
    /// `idx1` entry per snapshot record, in the order the packets
    /// were written.
    ///
    /// Per AVI 1.0 + OpenDML 2.0 §"Index Locations": AVI 1.0-only
    /// readers (Windows Media Player on XP, ffplay's strict AVI 1.0
    /// path) honour `idx1` alone — they don't walk OpenDML `ix##`
    /// super-indexes. When a file is OpenDML-muxed without `idx1`,
    /// those readers can't seek. This path closes that compat gap.
    ///
    /// All fields (chunk fourcc, flags, idx1-relative offset, size)
    /// are pre-baked into the [`PrimaryIxSnapshot`] at packet-write
    /// time so the offset/flags math here stays trivial.
    fn serialize_idx1_from_ix(&self) -> Vec<u8> {
        let mut idx_body = Vec::with_capacity(self.primary_ix_snapshot.len() * 16);
        for s in &self.primary_ix_snapshot {
            idx_body.extend_from_slice(&s.ckid);
            idx_body.extend_from_slice(&s.flags.to_le_bytes());
            idx_body.extend_from_slice(&s.offset.to_le_bytes());
            idx_body.extend_from_slice(&s.size.to_le_bytes());
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
            indexed_packet_count: self.current_segment_indexed_packets,
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
        for track_idx in 0..self.tracks.len() {
            self.flush_ix_for_track(track_idx)?;
        }
        Ok(())
    }

    /// Flush the accumulated `ix_entries` for a single track as one
    /// `ix##` AVISTDINDEX chunk, then clear them. Shared by
    /// [`Self::flush_ix_chunks`] (segment-tail flush) and the round-7
    /// mid-`movi` periodic flush. No-op if the track has no pending
    /// entries.
    fn flush_ix_for_track(&mut self, track_idx: usize) -> Result<()> {
        if self.tracks[track_idx].ix_entries.is_empty() {
            return Ok(());
        }
        let qw_base = self.movi_start_off + 4;
        let entries = std::mem::take(&mut self.tracks[track_idx].ix_entries);
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
        self.current_segment_indexed_packets = 0;
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

    /// Emit a single `JUNK` chunk sized so the next 8-byte chunk
    /// header lands at a file-absolute offset divisible by `granularity`
    /// (round-92). Helper for [`AviMuxer::write_packet`]'s stream-aligned
    /// remux path.
    ///
    /// Per AVI 1.0 §"Other Data Chunks": *"Data can be aligned in an
    /// AVI file by inserting 'JUNK' chunks as needed."* The JUNK chunk
    /// shape is the standard RIFF chunk: `'JUNK' <le32 size> <size
    /// bytes>`; a single pad byte follows when `size` is odd.
    ///
    /// `payload_len` is the upcoming packet's body byte count; it is
    /// reserved here for future spec-extension use (e.g. aligning the
    /// chunk's *data* rather than its header, which the spec leaves
    /// ambiguous — this implementation aligns the header per the
    /// industry convention).
    ///
    /// Algorithm:
    /// 1. Read the current file-absolute write position.
    /// 2. Compute the smallest multiple-of-`granularity` value `>=
    ///    current_pos + 8` (the JUNK chunk header itself takes 8
    ///    bytes, so we can't insert zero-length JUNK to "pad nothing").
    /// 3. The JUNK body length is `target - current_pos - 8`. Body is
    ///    zero-initialised. If `target - current_pos - 8` is odd, the
    ///    extra RIFF word-pad byte still goes inside `dwSize` so the
    ///    file position lands exactly at `target`.
    ///
    /// When the current position is already aligned and the chunk
    /// could be emitted as-is, the JUNK is skipped entirely — no
    /// zero-length JUNK is written, which avoids polluting the index
    /// with empty chunks.
    fn emit_padding_junk_for(&mut self, _payload_len: usize, granularity: u32) -> Result<()> {
        let n = granularity as u64;
        let cur = self.output.stream_position()?;
        let aligned_now = (cur % n) == 0;
        if aligned_now {
            return Ok(());
        }
        // Smallest multiple of n that's >= cur + 8 (JUNK chunk header
        // itself takes 8 bytes). If cur + 8 is already a multiple of
        // n, that's our target; otherwise round up.
        let needed = cur + 8;
        let target = needed.div_ceil(n) * n;
        let body_len = (target - cur - 8) as u32;
        // Per RIFF: body's `dwSize` excludes any odd-pad byte. To make
        // the file position land exactly at `target` we choose an even
        // body length whenever `target - cur - 8` is even; for odd
        // slack we write `body_len - 1` bytes of body, then RIFF's
        // word-pad byte makes the file position exactly `target`.
        let body_size_field = body_len & !1u32; // round down to even
        crate::riff::write_chunk_header(self.output.as_mut(), b"JUNK", body_size_field)?;
        let total_to_write = body_len as u64; // bytes after the 8-byte header
        let zeros = [0u8; 256];
        let mut remaining = total_to_write;
        while remaining > 0 {
            let chunk = remaining.min(zeros.len() as u64) as usize;
            self.output.write_all(&zeros[..chunk])?;
            remaining -= chunk as u64;
        }
        // Sanity: writer is now at exactly `target`. Asserted via the
        // mux-roundtrip test rather than here so a release build is
        // lean.
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

    /// Emit a `NNtx` text/subtitle side-band chunk for `stream_index`
    /// (round-11 candidate 3). Written into the current `movi` LIST
    /// after [`oxideav_core::Muxer::write_header`] and before
    /// [`oxideav_core::Muxer::write_trailer`]. Honours the active
    /// `LIST rec ` clustering and OpenDML segment-rolling, and
    /// records both an `idx1` entry (with no `AVIIF_KEYFRAME` bit so
    /// the demuxer's `scan_idx1_for_suffix` picks it up under
    /// `text_chunk_count`) and an `ix##` standard-index entry when
    /// in `AviKind::OpenDml` mode.
    ///
    /// Does NOT bump `strh.dwLength` for the parent stream — the
    /// chunk lives alongside the stream's regular packets without
    /// being counted as one of them. Mirror of the demuxer's round-10
    /// C1 read path: `xxtx` chunks come back via
    /// [`AviDemuxer::text_chunk_count`] and the
    /// `avi:text_chunk.<n>` metadata key.
    pub fn write_text_chunk(&mut self, stream_index: u32, data: &[u8]) -> Result<()> {
        self.write_sideband_chunk(stream_index, *b"tx", data)
    }

    /// Emit a `NNpc` palette-change side-band chunk for `stream_index`
    /// (round-11 candidate 3). Mirror of [`Self::write_text_chunk`]
    /// using suffix `b"pc"` so the demuxer's `scan_idx1_for_suffix`
    /// picks it up under [`AviDemuxer::palette_change_count`]. Per
    /// the AVI 1.0 spec the body is a `BITMAPINFO`-style payload
    /// (1-byte `bFirstEntry`, 1-byte `bNumEntries`, 2-byte `wFlags`,
    /// then `bNumEntries * 4`-byte palette quads); this helper
    /// writes `data` verbatim so callers compose the body themselves.
    pub fn write_palette_change(&mut self, stream_index: u32, data: &[u8]) -> Result<()> {
        self.write_sideband_chunk(stream_index, *b"pc", data)
    }

    /// Typed sibling of [`Self::write_palette_change`] (round-13
    /// candidate 1).
    ///
    /// Serialises the [`crate::demuxer::PaletteChange`] struct into
    /// the AVI 1.0 `BITMAPINFO`-style palette delta layout
    /// (`bFirstEntry` / `bNumEntries` / `wFlags` /
    /// `PALETTEENTRY[entries.len()]`) and emits it via the existing
    /// raw-bytes `write_palette_change` path. Closes the typed
    /// round-trip with [`crate::demuxer::AviDemuxer::palette_change_typed`]
    /// so callers can produce + consume palette deltas without
    /// hand-packing the BITMAPINFO header.
    pub fn with_palette_change_typed(
        &mut self,
        stream_index: u32,
        change: &crate::demuxer::PaletteChange,
    ) -> Result<()> {
        let body = change.to_bytes();
        self.write_palette_change(stream_index, &body)
    }

    /// Typed sibling of [`Self::write_text_chunk`] (round-15
    /// candidate 3).
    ///
    /// Serialises the [`crate::demuxer::TextChunk`] struct into the
    /// VfW-style `xxtx` body layout (2-byte LE `wCodePage` /
    /// `wLanguage` / `wDialect` followed by the body bytes — UTF-8
    /// for codepage `0` / `65001`, Latin-1 truncation otherwise) and
    /// emits it via the existing raw-bytes `write_text_chunk` path.
    /// Closes the typed round-trip with
    /// [`crate::demuxer::AviDemuxer::text_chunk_typed`] so callers
    /// don't have to hand-pack the 6-byte text-chunk header. Mirror
    /// of the round-13 [`Self::with_palette_change_typed`] pattern.
    pub fn with_text_chunk_typed(
        &mut self,
        stream_index: u32,
        text: &crate::demuxer::TextChunk,
    ) -> Result<()> {
        let body = text.to_bytes();
        self.write_text_chunk(stream_index, &body)
    }

    /// Common path for `NN<suffix>` side-band chunks (text / palette
    /// change). Lays out the chunk into the current `movi` LIST,
    /// honours `LIST rec ` clustering, rolls a fresh OpenDML segment
    /// when the projected size would push past the configured
    /// byte limit, records an `idx1` entry (no `AVIIF_KEYFRAME` so
    /// the demuxer's suffix scanner attributes the chunk to the
    /// per-stream side-band counter), and stamps an `ix##`
    /// standard-index entry when in OpenDML mode.
    fn write_sideband_chunk(
        &mut self,
        stream_index: u32,
        suffix: [u8; 2],
        data: &[u8],
    ) -> Result<()> {
        if !self.header_written {
            return Err(Error::other(
                "avi muxer: write_sideband_chunk before write_header",
            ));
        }
        if self.trailer_written {
            return Err(Error::other(
                "avi muxer: write_sideband_chunk after write_trailer",
            ));
        }
        let idx = stream_index as usize;
        if idx >= self.tracks.len() {
            return Err(Error::invalid(format!(
                "avi muxer: unknown stream index {idx}"
            )));
        }
        if data.len() > u32::MAX as usize {
            return Err(Error::invalid(
                "avi muxer: side-band chunk larger than 4 GiB",
            ));
        }

        // OpenDML: roll a new RIFF AVIX segment if this side-band chunk
        // would push the current segment past the configured byte
        // ceiling. Mirrors `write_packet` so side-band chunks don't
        // straddle segment boundaries.
        if let AviKind::OpenDml(limit) = self.kind {
            let projected = self.output.stream_position()?
                + 8 // chunk header
                + data.len() as u64
                + (data.len() & 1) as u64
                + 16 /* idx1 entry */;
            let segment_start = self.riff_size_off - 4;
            let segment_used = projected.saturating_sub(segment_start);
            if self.current_segment_packets > 0 && segment_used > limit.bytes() {
                self.close_current_segment()?;
                self.open_avix_segment()?;
            }
        }

        // `LIST rec ` clustering — open or roll the cluster the same
        // way `write_packet` does, so a side-band chunk lands inside
        // the same cluster as its surrounding regular packets.
        let want_clustering =
            self.options.rec_cluster_packets.is_some() || self.options.rec_cluster_bytes.is_some();
        if want_clustering {
            let projected_chunk_bytes = 8u64 + data.len() as u64 + (data.len() & 1) as u64;
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
                        && self.rec_bytes_in_cluster + projected_chunk_bytes > n as u64
                })
                .unwrap_or(false);
            if self.rec_open_size_off.is_none() {
                self.open_rec_cluster()?;
            } else if needs_close_for_packets || needs_close_for_bytes {
                self.close_rec_cluster()?;
                self.open_rec_cluster()?;
            }
        }

        let fourcc = packet_fourcc_for(stream_index, suffix);
        let chunk_off = self.output.stream_position()?;
        let rel_off_opt = chunk_off.checked_sub(self.movi_start_off);
        let size = data.len() as u32;

        // OpenDML std-index: record the side-band chunk so the
        // segment-tail `ix##` flush includes it, mirroring how
        // regular packets land. The chunk is never a keyframe, so
        // the high `dwSize` bit is set to flag a delta-frame entry.
        if matches!(self.kind, AviKind::OpenDml(_)) {
            let qw_base = self.movi_start_off + 4;
            let data_off = chunk_off + 8;
            if let Some(d) = data_off.checked_sub(qw_base) {
                if d <= u32::MAX as u64 {
                    let dw_size_with_flag = size | 0x8000_0000;
                    let ix_entry = IxStdEntry {
                        dw_offset: d as u32,
                        dw_size_with_flag,
                        dw_offset_field2: 0,
                    };
                    self.tracks[idx].ix_entries.push(ix_entry);
                    if self.options.synthesise_idx1_from_ix && self.segments.is_empty() {
                        // Side-band chunks (xxtx / xxpc) carry no
                        // AVIIF_KEYFRAME bit so the demuxer's
                        // suffix-scanner attributes them to the
                        // per-stream side-band counter (mirror of the
                        // `flags = 0` rule below).
                        let idx1_offset = (d as u32).saturating_sub(4);
                        self.primary_ix_snapshot.push(PrimaryIxSnapshot {
                            ckid: fourcc,
                            flags: 0,
                            offset: idx1_offset,
                            size,
                        });
                    }
                }
            }
        }

        write_chunk(self.output.as_mut(), &fourcc, data)?;

        if want_clustering {
            self.rec_packets_in_cluster += 1;
            self.rec_bytes_in_cluster += 8u64 + data.len() as u64 + (data.len() & 1) as u64;
        }

        // idx1 entry — flags = 0 so the demuxer's
        // `scan_idx1_for_suffix(*b"tx", ...)` /
        // `scan_idx1_for_suffix(*b"pc", ...)` picks up the chunk
        // under the per-stream side-band counter (the scanner only
        // matches on the chunk-id suffix; flags don't gate it).
        let in_primary_segment = self.segments.is_empty();
        if in_primary_segment {
            if let Some(rel_off) = rel_off_opt {
                if rel_off <= u32::MAX as u64 {
                    self.index.push(IndexEntry {
                        ckid: fourcc,
                        flags: 0,
                        offset: rel_off as u32,
                        size,
                    });
                }
            }
        }
        Ok(())
    }

    /// Number of OpenDML segments that overflowed the
    /// configured super-index reserve (round-5 candidate 4).
    /// Default reserve is
    /// [`OPENDML_SUPER_INDEX_DEFAULT_CAPACITY`] (256); callers may
    /// raise it via [`AviMuxOptions::with_super_index_capacity`]
    /// (round-6 candidate 3).
    ///
    /// Returns `0` when every segment lands in the super-index, or
    /// when `kind` is `AviKind::Avi10` (the legacy envelope has no
    /// super-index). When > 0, the trailing entries silently miss
    /// the super-index — the file is still demuxable (the demuxer
    /// walks `RIFF AVIX` continuations linearly) but a downstream
    /// inspector can flag the file as having lost some
    /// random-access fidelity.
    ///
    /// Meaningful only after [`oxideav_core::Muxer::write_trailer`]
    /// — until then `segments.len()` doesn't reflect the trailing
    /// open segment.
    pub fn truncated_super_index_segments(&self) -> usize {
        if !matches!(self.kind, AviKind::OpenDml(_)) {
            return 0;
        }
        if self.indx_entries_capacity == 0 {
            return 0;
        }
        self.segments
            .len()
            .saturating_sub(self.indx_entries_capacity)
    }

    /// Per-stream `dwMaxBytesPerSec` cap breaches (round-18
    /// candidate 1). Each entry is `(stream_index, observed_bps,
    /// cap_bps)`:
    /// - `stream_index` is the 0-based stream ordinal whose actual
    ///   bytes-per-sec exceeded the cap registered via
    ///   [`AviMuxOptions::with_per_stream_max_bytes_per_sec`],
    /// - `observed_bps` is `total_bytes * 1_000_000 /
    ///   duration_micros` for the stream (clamped to `u64::MAX`
    ///   when duration is zero — see below),
    /// - `cap_bps` is the value the caller registered.
    ///
    /// Empty when no caps were registered, when none of the
    /// registered streams breached, or when the file's
    /// `duration_micros` is zero (no usable per-frame timing — the
    /// muxer can't compute a meaningful per-stream rate without it,
    /// so it conservatively skips the comparison rather than
    /// false-positive every track).
    ///
    /// Meaningful only after [`oxideav_core::Muxer::write_trailer`]
    /// returns successfully OR returns
    /// `Err(Error::InvalidData)` from
    /// [`AviMuxOptions::with_strict_per_stream_budget`] — until
    /// then the breach detection hasn't run.
    pub fn over_budget_streams(&self) -> &[(u32, u64, u32)] {
        &self.over_budget_streams
    }

    /// Compute every per-stream `dwMaxBytesPerSec` cap breach and
    /// store the `(stream_index, observed_bps, cap_bps)` tuples in
    /// `self.over_budget_streams`. Called from `write_trailer` after
    /// `patch_post_counts` so the duration source matches what the
    /// avih stamp uses.
    fn compute_per_stream_budget_breaches(&mut self) {
        self.over_budget_streams.clear();
        // Same duration source as `patch_post_counts`'s
        // `max_bytes_per_sec` computation: first video stream's
        // micro_per_frame × packet_count. Audio-only files have no
        // video stream and thus no per-frame timing — skip the
        // per-stream budget check rather than emit false-positives
        // (the avih populator's audio-only fallback uses
        // `nAvgBytesPerSec` for the file-wide value, which doesn't
        // give us a per-second timeline either).
        let micro_per_frame: u64 = self
            .tracks
            .iter()
            .find(|t| &t.entry.strh_type == b"vids")
            .map(|t| {
                let scale = t.entry.scale.max(1) as u64;
                let rate = t.entry.rate.max(1) as u64;
                1_000_000u64 * scale / rate
            })
            .unwrap_or(0);
        let video_packet_count: u64 = self
            .tracks
            .iter()
            .find(|t| &t.entry.strh_type == b"vids")
            .map(|t| t.packet_count as u64)
            .unwrap_or(0);
        let micros: u64 = video_packet_count.saturating_mul(micro_per_frame);
        if micros == 0 {
            return;
        }
        for &(idx, cap) in &self.options.per_stream_max_bytes_per_sec {
            let s = idx as usize;
            let track = match self.tracks.get(s) {
                Some(t) => t,
                None => continue,
            };
            // observed_bps = total_bytes * 1_000_000 / micros.
            let big = (track.total_bytes as u128) * 1_000_000u128;
            let observed = (big / (micros as u128)).min(u64::MAX as u128) as u64;
            if observed > cap as u64 {
                self.over_budget_streams.push((idx, observed, cap));
            }
        }
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
            // dwDuration: the indexed stream's per-segment frame count
            // (OpenDML 2.0 §"AVI Super Index Chunk" — the time span of
            // the chunks this segment's `ix##` indexes), not the
            // all-stream packet total (round-101).
            self.output
                .write_all(&seg.indexed_packet_count.to_le_bytes())?;
        }
        self.output.seek(SeekFrom::Start(end_pos))?;
        Ok(())
    }
}

/// Write a `LIST INFO` chunk carrying the `(fourcc, value)` entries
/// per the AVI 1.0 `LIST INFO` registry. Each child is a 4-CC chunk
/// whose payload is a NUL-terminated string. Used by `write_header`
/// for both placements (nested-in-hdrl and sibling-of-hdrl per
/// round-11 candidate 1).
fn write_info_list<W: Write + Seek + ?Sized>(
    w: &mut W,
    entries: &[([u8; 4], String)],
) -> Result<()> {
    let info_size_off = begin_list(w, &LIST, b"INFO")?;
    for (id, value) in entries {
        // NUL-terminate per the AVI 1.0 `LIST INFO` convention and
        // pad odd lengths with the implicit RIFF pad byte.
        let mut body = value.as_bytes().to_vec();
        body.push(0);
        write_chunk(w, id, &body)?;
    }
    finish_chunk(w, info_size_off)?;
    Ok(())
}

/// Default `avih.dwFlags` value: `AVIF_HASINDEX | AVIF_TRUSTCKTYPE`
/// per Microsoft's `vfw.h` (the round-6 muxer baseline — the bit
/// pattern matches what a round-trip with the demuxer's `avih_flags()`
/// surfaces; older comments mislabeled this as
/// `AVIF_ISINTERLEAVED | AVIF_HASINDEX` but the constant emitted has
/// always been `0x0810`, which is HASINDEX | TRUSTCKTYPE per
/// `vfw.h`'s actual bit definitions). Used by
/// [`AviMuxOptions::with_avih_flag_bit`] as the OR base when the
/// caller wants "default plus one extra bit" without specifying every
/// flag explicitly. Override entirely via
/// [`AviMuxOptions::with_avih_flags`].
pub const DEFAULT_AVIH_FLAGS: u32 = 0x0000_0810;

/// AVIMAINHEADER (56 bytes): dwMicroSecPerFrame, dwMaxBytesPerSec,
/// dwPaddingGranularity, dwFlags, dwTotalFrames, dwInitialFrames, dwStreams,
/// dwSuggestedBufferSize, dwWidth, dwHeight, dwReserved[4].
///
/// Round-92: `padding_granularity` stamps `avih.dwPaddingGranularity`
/// from `AviMuxOptions::padding_granularity` (None → 0, the legacy
/// "no alignment guarantee" sentinel; Some(n) → n, matching the JUNK
/// chunk alignment the muxer also emits in `movi`).
///
/// Round-157: `initial_frames` stamps `avih.dwInitialFrames`
/// (byte offset 16 of the body) from `AviMuxOptions::initial_frames`
/// (None → 0, the legacy "noninterleaved file" sentinel per AVI 1.0
/// §"AVIMAINHEADER" line 200; Some(n) → n verbatim).
fn build_avih(
    tracks: &[TrackState],
    flags_override: Option<u32>,
    padding_granularity: Option<u32>,
    initial_frames: Option<u32>,
) -> Vec<u8> {
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
    // Round-12 candidate 2: caller may override `dwFlags` via
    // [`AviMuxOptions::with_avih_flags`] / [`with_avih_flag_bit`].
    // Default `AVIF_HASINDEX | AVIF_TRUSTCKTYPE` per Microsoft's
    // `vfw.h` (the round-6 baseline; see [`DEFAULT_AVIH_FLAGS`]).
    let flags: u32 = flags_override.unwrap_or(DEFAULT_AVIH_FLAGS);
    let total_frames: u32 = 0; // patched post-hoc
    let streams = tracks.len() as u32;

    let mut body = Vec::with_capacity(56);
    body.extend_from_slice(&video_micro_per_frame.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // MaxBytesPerSec
    body.extend_from_slice(&padding_granularity.unwrap_or(0).to_le_bytes()); // PaddingGranularity
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&total_frames.to_le_bytes());
    // Round-157: `dwInitialFrames` (body offset 16). The pre-round-157
    // default was hard-coded `0` ("noninterleaved file" per AVI 1.0
    // §"AVIMAINHEADER" line 200, which the demuxer maps back to
    // `None`). `AviMuxOptions::with_initial_frames(n)` lets a caller
    // stamp a non-zero file-global skew here without disturbing the
    // per-stream `strh.dwInitialFrames` field (round-153).
    body.extend_from_slice(&initial_frames.unwrap_or(0).to_le_bytes()); // InitialFrames
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
    indx_capacity: usize,
    stream_name: Option<&str>,
    stream_header_data: Option<&[u8]>,
    frame_rect_override: Option<[i16; 4]>,
    language_override: Option<u16>,
    initial_frames_override: Option<u32>,
    quality_override: Option<u32>,
    priority_override: Option<u16>,
    start_override: Option<u32>,
    handler_override: Option<[u8; 4]>,
    sample_size_override: Option<u32>,
    flags_override: Option<u32>,
    timebase_override: Option<(u32, u32)>,
    fcc_type_override: Option<[u8; 4]>,
) -> Result<(Option<u64>, Option<u64>)> {
    let strl_off = begin_list(w, &LIST, b"strl")?;

    // strh body (56 bytes).
    let mut strh = Vec::with_capacity(56);
    // fccType (round-253): an `AviMuxOptions::with_stream_fcc_type`
    // override (when present) stamps the per-stream type FOURCC at
    // byte offset 0 per AVI 1.0 §"AVISTREAMHEADER" (`fccType` row in
    // `docs/container/riff/avi-riff-file-reference.md`, Appendix B
    // line 235 + the `fcc` row at line 234: *"A FOURCC code that
    // specifies the type of data contained in the stream. The
    // following standard AVI values are defined: `auds` (audio
    // stream), `mids` (MIDI stream), `txts` (text stream), `vids`
    // (video stream)."*); otherwise the legacy default is the
    // packaging-derived `t.entry.strh_type` (for video streams
    // `vids`, for audio streams `auds`). The 4 bytes are written
    // verbatim; printability and membership in the standard
    // `{auds, mids, txts, vids}` set are not validated.
    let fcc_type_bytes = fcc_type_override.unwrap_or(t.entry.strh_type);
    strh.extend_from_slice(&fcc_type_bytes); // fccType
                                             // fccHandler (round-210): an `AviMuxOptions::with_stream_handler`
                                             // override (when present) stamps the per-stream preferred-driver
                                             // FourCC at byte offset 4 per AVI 1.0 §"AVISTREAMHEADER"
                                             // (`fccHandler` row in
                                             // `docs/container/riff/avi-riff-file-reference.md`, Appendix B
                                             // line 236: *"An optional FOURCC that identifies a specific data
                                             // handler. The data handler is the preferred handler for the
                                             // stream. For audio and video streams, this specifies the codec
                                             // for decoding the stream."*); otherwise the legacy default is
                                             // the packaging-derived `t.entry.handler_fourcc` (for video
                                             // streams: the per-codec FourCC, mirroring
                                             // `BITMAPINFOHEADER.biCompression`; for audio streams: the
                                             // all-zero `\0\0\0\0` "no preferred handler" default the
                                             // demuxer maps back to `None`). The 4 bytes are written
                                             // verbatim; printability is not validated.
    let handler_bytes = handler_override.unwrap_or(t.entry.handler_fourcc);
    strh.extend_from_slice(&handler_bytes);
    // dwFlags (round-247): an `AviMuxOptions::with_stream_flags`
    // override (when present) stamps the per-stream flag DWORD at byte
    // offset 8 per AVI 1.0 §"AVISTREAMHEADER" (`dwFlags` row in
    // `docs/container/riff/avi-riff-file-reference.md`, line 237 + the
    // *dwFlags values* table at lines 252–255 carrying `AVISF_DISABLED`
    // / `AVISF_VIDEO_PALCHANGES`); otherwise the legacy default is `0`
    // ("no flags set" — the muxer's own default since round-3, which
    // the demuxer maps back to `None`). The 32-bit value is written
    // verbatim; the muxer does not validate against the spec's two
    // documented bits so callers may stamp vendor / driver-private
    // bits in the upper half-DWORD for round-trippability.
    strh.extend_from_slice(&flags_override.unwrap_or(0).to_le_bytes()); // flags
                                                                        // wPriority (round-182): an `AviMuxOptions::with_stream_priority`
                                                                        // override (when present) stamps the per-stream selection hint at
                                                                        // byte offset 12 per AVI 1.0 §"AVISTREAMHEADER" (`wPriority` row in
                                                                        // `docs/container/riff/avi-riff-file-reference.md` Appendix B line
                                                                        // 238: *"Priority of a stream type. For example, in a file with
                                                                        // multiple audio streams, the one with the highest priority might
                                                                        // be the default stream."*); otherwise the legacy default is `0`
                                                                        // (the muxer's own default since round-3, which the demuxer maps
                                                                        // back to `None`). The 16-bit value is written verbatim; the spec
                                                                        // does not normatively pin a value range or a tie-break rule.
    strh.extend_from_slice(&priority_override.unwrap_or(0).to_le_bytes()); // priority

    // wLanguage (round-119): an `AviMuxOptions::with_stream_language`
    // override (when present) stamps the LANGID at byte offset 14 per
    // AVI 1.0 §"AVISTREAMHEADER"; otherwise the legacy default is `0`
    // ("LANG_NEUTRAL / SUBLANG_NEUTRAL", which the demuxer maps back
    // to `None`). The 16-bit value is written verbatim; no registry
    // validation.
    strh.extend_from_slice(&language_override.unwrap_or(0).to_le_bytes());

    // dwInitialFrames (round-153): an `AviMuxOptions::with_stream_initial_frames`
    // override (when present) stamps the per-stream interleave skew at
    // byte offset 16 per AVI 1.0 §"AVISTREAMHEADER"; otherwise the
    // legacy default is `0` ("noninterleaved file" per AVIMAINHEADER
    // §`dwInitialFrames`, which the demuxer maps back to `None`). The
    // 32-bit value is written verbatim; no validation against the
    // per-stream `dwLength`.
    strh.extend_from_slice(&initial_frames_override.unwrap_or(0).to_le_bytes()); // initial_frames
                                                                                 // dwScale + dwRate (round-249): an
                                                                                 // `AviMuxOptions::with_stream_timebase` override (when present)
                                                                                 // stamps the per-stream `(scale, rate)` pair at byte offsets 20
                                                                                 // and 24 per AVI 1.0 §"AVISTREAMHEADER" (`dwScale` row in
                                                                                 // `docs/container/riff/avi-riff-file-reference.md` line 241 +
                                                                                 // the `dwRate` row line 242: *"Used with dwRate to specify the
                                                                                 // time scale that this stream will use. Dividing dwRate by
                                                                                 // dwScale gives the number of samples per second."*); otherwise
                                                                                 // the packaging-derived `t.entry.scale` / `t.entry.rate` defaults
                                                                                 // stand (video: per-stream `frame_rate`, audio: `sample_rate / 1`).
                                                                                 // The two 32-bit values are written verbatim; the override does
                                                                                 // NOT alter the muxer's `(scale, rate)`-derived `dwLength`
                                                                                 // computation for audio streams (which still uses
                                                                                 // `t.entry.{scale,rate}` to convert running samples into
                                                                                 // `dwLength` units), does NOT touch `avih.dwMicroSecPerFrame`
                                                                                 // (independently derived from the first video stream's
                                                                                 // packaging pair), and does NOT cross-validate against the
                                                                                 // per-stream `dwLength` or `dwStart`. A `0` in either DWORD
                                                                                 // stamps the writer-skips-it / mathematically-undefined
                                                                                 // sentinel the demuxer maps back to `None`.
    let (scale_bytes, rate_bytes) = timebase_override.unwrap_or((t.entry.scale, t.entry.rate));
    strh.extend_from_slice(&scale_bytes.to_le_bytes());
    strh.extend_from_slice(&rate_bytes.to_le_bytes());
    // dwStart (round-203): an `AviMuxOptions::with_stream_start` override
    // (when present) stamps the per-stream starting time at byte offset
    // 28 per AVI 1.0 §"AVISTREAMHEADER" (`dwStart` row in
    // `docs/container/riff/avi-riff-file-reference.md` line 243:
    // *"Starting time for this stream. The units are defined by the
    // dwRate and dwScale members in the main file header. Usually, this
    // is zero, but it can specify a delay time for a stream that does
    // not start concurrently with the file."*); otherwise the legacy
    // default is `0` (the muxer's own default since round-3, which the
    // demuxer maps back to `None`). The 32-bit value is written verbatim
    // and is not validated against the per-stream `dwLength`.
    strh.extend_from_slice(&start_override.unwrap_or(0).to_le_bytes()); // start
    strh.extend_from_slice(&0u32.to_le_bytes()); // length (patched)
    strh.extend_from_slice(&0u32.to_le_bytes()); // suggested_buffer_size (patched)
                                                 // dwQuality (round-176): an `AviMuxOptions::with_stream_quality`
                                                 // override (when present) stamps the per-stream quality indicator at
                                                 // byte offset 40 per AVI 1.0 §"AVISTREAMHEADER" (`dwQuality` row in
                                                 // `docs/container/riff/avi-riff-file-reference.md` line 246);
                                                 // otherwise the legacy default is `0xFFFF_FFFF` (= `-1` as i32, the
                                                 // documented "use default driver quality" sentinel — *"If set to -1,
                                                 // drivers use the default quality value."* — which the demuxer maps
                                                 // back to `None`). The 32-bit value is written verbatim; no clamp to
                                                 // the documented `[0, 10_000]` range.
    strh.extend_from_slice(&quality_override.unwrap_or(0xFFFF_FFFFu32).to_le_bytes());
    // dwSampleSize (round-222): an `AviMuxOptions::with_stream_sample_size`
    // override (when present) stamps the per-stream sample-size indicator
    // at byte offset 44 per AVI 1.0 §"AVISTREAMHEADER" (`dwSampleSize` row
    // in `docs/container/riff/avi-riff-file-reference.md` line 247: *"The
    // size of a single sample of data. This is set to zero if the samples
    // can vary in size. If this number is nonzero, then multiple samples
    // of data can be grouped into a single chunk within the file. … For
    // video streams, this number is typically zero, although it can be
    // nonzero if all video frames are the same size. For audio streams,
    // this number should be the same as the nBlockAlign member of the
    // WAVEFORMATEX structure describing the audio."*); otherwise the
    // packaging-derived default `t.entry.sample_size` wins (audio: PCM /
    // CBR streams carry `nBlockAlign`, VBR streams carry `0`; video:
    // `0`). The 32-bit value is written verbatim. The override only
    // changes the byte stamp at offset 44; it does NOT alter the muxer's
    // own `dwLength` derivation (the audio `size / sample_size` formula
    // continues to use the packaging-derived `t.entry.sample_size`). An
    // explicit `0` stamps the spec-documented "samples can vary in size"
    // sentinel — the demuxer maps that back to `None`.
    strh.extend_from_slice(
        &sample_size_override
            .unwrap_or(t.entry.sample_size)
            .to_le_bytes(),
    );
    // rcFrame: left, top, right, bottom (i16 each) at byte offset 48 of
    // the 56-byte AVISTREAMHEADER. A round-115
    // `AviMuxOptions::with_stream_frame_rect` override (when present) wins
    // for any stream type; otherwise the legacy default is
    // `0,0,width,height` for video streams and all-zero for non-video.
    if let Some([l, t_, r, b]) = frame_rect_override {
        strh.extend_from_slice(&l.to_le_bytes());
        strh.extend_from_slice(&t_.to_le_bytes());
        strh.extend_from_slice(&r.to_le_bytes());
        strh.extend_from_slice(&b.to_le_bytes());
    } else if &t.entry.strh_type == b"vids" {
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
        //   <16-byte entries> × indx_capacity
        // Entries are zero-initialised so a partial back-patch leaves
        // a clean tail of zeros that demuxers tolerate.
        let chunk_id = packet_fourcc_for(0, t.entry.chunk_suffix);
        let entries_bytes = indx_capacity * 16;
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

    // AVI 1.0 §"AVI Stream Headers" (round-89): optional `strd`
    // codec-driver configuration blob. "If the stream-header data
    // ('strd') chunk is present, it follows the stream format chunk.
    // The format and content of this chunk are defined by the codec
    // driver." We emit it after `indx`/`vprp` (and before `strn`) to
    // keep pre-round-89 byte layout identical when no `strd` is
    // configured — same back-compat positioning the round-80 `strn`
    // emit uses. The body is the caller-supplied bytes verbatim,
    // RIFF-word-padded by `write_chunk` so an odd-length blob gets
    // one trailing zero byte.
    if let Some(bytes) = stream_header_data {
        write_chunk(w, b"strd", bytes)?;
    }

    // AVI 1.0 §"AVI Stream Headers": optional `strn` chunk carrying a
    // null-terminated text string describing the stream (round-80).
    // The spec places this last among the strl children ("'strh', 'strf'
    // [, 'strd'] [, 'strn']") so we emit it after `indx`/`vprp`/`strd`
    // to keep pre-round-80 byte layout identical when no name is
    // configured.
    if let Some(name) = stream_name {
        let mut body = Vec::with_capacity(name.len() + 1);
        body.extend_from_slice(name.as_bytes());
        body.push(0); // NUL terminator per spec.
        write_chunk(w, b"strn", &body)?;
    }

    finish_chunk(w, strl_off)?;
    Ok((indx_n_entries_off, indx_entries_start_off))
}

/// Build a `vprp` body for a video track. 9 fixed DWORDs followed by
/// `nbFieldPerFrame` `VIDEO_FIELD_DESC` records (8 DWORDs each).
/// Total length = 36 + 32 * nbFieldPerFrame bytes (round-9 candidate
/// 1 fixes the round-4 producer to actually emit one rect per field;
/// previously a 2-field stream was declared with `nbFieldPerFrame=2`
/// but only one rect was written, dropping half the spec-mandated
/// data on the floor).
///
/// `override_cfg`, when supplied, replaces the per-field defaults
/// with caller-chosen values per OpenDML 2.0 §5.0 (round-4 P2). A
/// zero override field falls back to the default so callers can
/// override only what they care about.
///
/// For 2-field interlaced streams the two emitted records describe
/// the top + bottom fields with `CompressedBMHeight = height/2` and
/// `VideoYValidStartLine` set to the spec's first-line conventions
/// (`23` for top, `height/2 + 23` for bottom — matching PAL/NTSC
/// CCIR-601 broadcast sequences). Progressive (1 field/frame)
/// emits a single full-frame record.
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
    // Body capacity = 9 fixed DWORDs (36 B) + per-field 32 B records.
    let mut body = Vec::with_capacity(36 + 32 * nb_field_per_frame as usize);
    body.extend_from_slice(&video_format_token.to_le_bytes());
    body.extend_from_slice(&video_standard.to_le_bytes());
    body.extend_from_slice(&refresh_rate.to_le_bytes()); // dwVerticalRefreshRate
    body.extend_from_slice(&width.to_le_bytes()); // dwHTotalInT (unknown — fall back to width)
    body.extend_from_slice(&height.to_le_bytes()); // dwVTotalInLines
    body.extend_from_slice(&frame_aspect_ratio.to_le_bytes()); // dwFrameAspectRatio
    body.extend_from_slice(&width.to_le_bytes()); // dwFrameWidthInPixels
    body.extend_from_slice(&height.to_le_bytes()); // dwFrameHeightInLines
    body.extend_from_slice(&nb_field_per_frame.to_le_bytes());

    // VIDEO_FIELD_DESC[0..nbFieldPerFrame]: spec-mandated per-field
    // rect array. Round-10 C2 honours a caller-supplied
    // `field_descs` override verbatim — required for NTSC and any
    // other standard whose first-line conventions don't match the
    // synthesised PAL-flavoured default. The override only takes
    // effect when the supplied Vec covers every active field; a
    // shorter Vec falls through to the synthesised default so a
    // partial override doesn't silently truncate the array.
    let active_fields = nb_field_per_frame.max(1) as usize;
    let use_override = !cfg.field_descs.is_empty() && cfg.field_descs.len() >= active_fields;
    if use_override {
        for d in cfg.field_descs.iter().take(active_fields) {
            body.extend_from_slice(&d.compressed_bm_height.to_le_bytes());
            body.extend_from_slice(&d.compressed_bm_width.to_le_bytes());
            body.extend_from_slice(&d.valid_bm_height.to_le_bytes());
            body.extend_from_slice(&d.valid_bm_width.to_le_bytes());
            body.extend_from_slice(&d.valid_bm_x_offset.to_le_bytes());
            body.extend_from_slice(&d.valid_bm_y_offset.to_le_bytes());
            body.extend_from_slice(&d.video_x_offset_in_t.to_le_bytes());
            body.extend_from_slice(&d.video_y_valid_start_line.to_le_bytes());
        }
    } else if nb_field_per_frame >= 2 {
        let half_height = height / 2;
        // PAL/NTSC CCIR-601 first-line conventions per OpenDML §5.0
        // table: top field starts at line 23, bottom at line 285+
        // for NTSC and 23 / 335 for PAL. Using `23` + `half_height +
        // 23` is a reasonable cross-standard default that matches
        // the PAL convention; consumers needing exact first-line
        // values for a specific standard read them via
        // `vprp_field_descs` and substitute their own (round-10 C2
        // adds [`VprpConfig::with_field_descs`] for muxers that want
        // standard-correct first-line offsets).
        for field_index in 0..nb_field_per_frame.min(2) {
            let video_y_valid_start_line = if field_index == 0 {
                23u32
            } else {
                half_height + 23
            };
            body.extend_from_slice(&half_height.to_le_bytes()); // CompressedBMHeight
            body.extend_from_slice(&width.to_le_bytes()); // CompressedBMWidth
            body.extend_from_slice(&half_height.to_le_bytes()); // ValidBMHeight
            body.extend_from_slice(&width.to_le_bytes()); // ValidBMWidth
            body.extend_from_slice(&0u32.to_le_bytes()); // ValidBMXOffset
            body.extend_from_slice(&0u32.to_le_bytes()); // ValidBMYOffset
            body.extend_from_slice(&0u32.to_le_bytes()); // VideoXOffsetInT
            body.extend_from_slice(&video_y_valid_start_line.to_le_bytes());
        }
    } else {
        // Progressive: single full-frame VIDEO_FIELD_DESC.
        body.extend_from_slice(&height.to_le_bytes()); // CompressedBMHeight
        body.extend_from_slice(&width.to_le_bytes()); // CompressedBMWidth
        body.extend_from_slice(&height.to_le_bytes()); // ValidBMHeight
        body.extend_from_slice(&width.to_le_bytes()); // ValidBMWidth
        body.extend_from_slice(&0u32.to_le_bytes()); // ValidBMXOffset
        body.extend_from_slice(&0u32.to_le_bytes()); // ValidBMYOffset
        body.extend_from_slice(&0u32.to_le_bytes()); // VideoXOffsetInT
        body.extend_from_slice(&0u32.to_le_bytes()); // VideoYValidStartLine
    }
    body
}

/// Compute how much to advance `TrackState::sample_count` for a
/// freshly-written packet.
///
/// PCM audio (`strh.fccType == "auds" && entry.sample_size > 0`)
/// uses `size / sample_size` so `strh.dwLength` ends up as the total
/// number of audio frames — exactly what the AVI 1.0 spec
/// (`AVISTREAMHEADER.dwLength`) wants for fixed block_align streams.
///
/// VBR audio (`sample_size == 0`) is the round-5 candidate 3
/// addition: when the encoder supplies `Packet.duration` (in stream
/// ticks; for audio that's `samples_per_second` time-base ticks per
/// the [`Demuxer::time_base`] convention), we accumulate that into
/// `sample_count` so `dwLength` ends up as the real frame count.
/// Without `duration` we fall back to the round-3 "1 per packet"
/// behaviour, which is at least monotonic — it just can't be
/// converted back to a wall-clock duration.
///
/// Non-audio streams always get 1 per packet (frame count).
/// Pull `nAvgBytesPerSec` (the third u32, byte offset 8..12) out of a
/// WAVEFORMATEX strf body (round-15 candidate 2). Returns `None` when
/// the strf is shorter than 12 bytes (defensive — the spec's minimum
/// is 14 for legacy WAVEFORMAT, 16 for PCMWAVEFORMAT, 18 for the full
/// WAVEFORMATEX, and `build_audio_strf` always emits 18+) or when the
/// declared rate is zero.
fn audio_strf_avg_bytes_per_sec(strf: &[u8]) -> Option<u32> {
    if strf.len() < 12 {
        return None;
    }
    let bps = u32::from_le_bytes([strf[8], strf[9], strf[10], strf[11]]);
    if bps == 0 {
        None
    } else {
        Some(bps)
    }
}

fn sample_count_of_packet(
    stream: &StreamInfo,
    entry: &StrfEntry,
    size: u32,
    duration: Option<i64>,
) -> u64 {
    if &entry.strh_type == b"auds" {
        if entry.sample_size > 0 {
            return (size as u64) / (entry.sample_size as u64);
        }
        // VBR (e.g. MP3, AC3, AAC): prefer the packet's duration when
        // the caller bothered to set it. The AVI strh.dwLength is in
        // the stream's ticks (= samples_per_sec for PCM-like audio),
        // so a positive `Packet.duration` lands directly here.
        if let Some(d) = duration {
            if d > 0 {
                return d as u64;
            }
        }
    }
    let _ = stream;
    1
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
