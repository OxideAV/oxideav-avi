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
        });
    }
    Ok(Box::new(AviMuxer {
        output,
        tracks,
        kind,
        riff_size_off: 0,
        movi_size_off: 0,
        movi_start_off: 0,
        index: Vec::new(),
        indx_entries_count_off: None,
        indx_entries_start_off: None,
        indx_entries_capacity: 0,
        segments: Vec::new(),
        current_segment_packets: 0,
        header_written: false,
        trailer_written: false,
    }))
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

struct AviMuxer {
    output: Box<dyn WriteSeek>,
    tracks: Vec<TrackState>,
    kind: AviKind,
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
    /// All closed-out segments in OpenDML mode. The primary segment
    /// is appended to this list when it's closed (i.e. when the next
    /// `write_packet` would push past the limit, or in
    /// `write_trailer`). Always empty for `AviKind::Avi10`.
    segments: Vec<SegmentRecord>,
    /// Number of packets written into the current open segment's
    /// `movi` LIST. Reset when a new segment is opened.
    current_segment_packets: u32,
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
        for (i, t) in self.tracks.iter().enumerate() {
            let with_indx = want_indx && i == 0;
            let (indx_count_off, indx_entries_off) =
                write_strl(self.output.as_mut(), i as u32, t, with_indx)?;
            if with_indx {
                self.indx_entries_count_off = indx_count_off;
                self.indx_entries_start_off = indx_entries_off;
                self.indx_entries_capacity = OPENDML_SUPER_INDEX_CAPACITY;
            }
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
        //   RIFF(12): 4 + size + 4(AVI )         — offset 0..12
        //   LIST(8): 4 + size + 4(hdrl)           — offset 12..20
        //   "avih" chunk:
        //     header: 4(avih) + 4(size)           — offset 20..28
        //     body  : 56 bytes                    — offset 28..84
        //       total_frames at body offset 16    — file offset 44..48
        //   For each stream i:
        //     LIST(8): 4 + size + 4(strl)
        //     strh(8): 4 + 4 + 56
        //       dwLength at strh body offset 32
        //     strf(8+N) ...
        //     [ indx(8 + 24 + 16*OPENDML_SUPER_INDEX_CAPACITY) ]
        let total_video_frames = self
            .tracks
            .iter()
            .find(|t| &t.entry.strh_type == b"vids")
            .map(|t| t.packet_count)
            .unwrap_or_else(|| self.tracks.first().map(|t| t.packet_count).unwrap_or(0));

        let end_pos = self.output.stream_position()?;

        // avih.dwTotalFrames is at offset 20 (LIST hdrl header end) + 8
        // ("avih" chunk header) + 16 (body offset of TotalFrames) = 44.
        self.output.seek(SeekFrom::Start(44))?;
        self.output.write_all(&total_video_frames.to_le_bytes())?;

        // Walk through strl lists to patch each strh.dwLength. The first
        // strl LIST starts at offset 88 (= 20 + 4 + 64). For OpenDML the
        // first stream's strl ALSO contains an indx chunk after strf, so
        // the second stream's strl is offset by an extra
        // (8 + 24 + 16*OPENDML_SUPER_INDEX_CAPACITY) bytes.
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
            // [+ 8 + indx_payload_padded if i == 0 and opendml].
            let strf_padded = t.entry.strf.len() + (t.entry.strf.len() & 1);
            let mut strl_body = 4 + 64 + 8 + strf_padded;
            if opendml && i == 0 {
                let indx_payload = 24 + 16 * OPENDML_SUPER_INDEX_CAPACITY;
                let indx_padded = indx_payload + (indx_payload & 1);
                strl_body += 8 + indx_padded;
            }
            strl_off += 8 + strl_body as u64;
        }

        // Restore writer position.
        self.output.seek(SeekFrom::Start(end_pos))?;
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

    /// Close the current `RIFF` segment in OpenDML mode. Finishes the
    /// movi LIST, writes idx1 if this is the primary segment, then
    /// finishes the outer RIFF and records the segment's `(offset,
    /// total_size, packet_count)`.
    fn close_current_segment(&mut self) -> Result<()> {
        let in_primary = self.segments.is_empty();
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
        Ok(())
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

/// Build and write a `strl` LIST (strh + strf [+ indx]).
///
/// Returns `(indx_n_entries_off, indx_entries_start_off)` when
/// `with_indx` is set, otherwise `(None, None)`. The two offsets let
/// the muxer back-patch the OpenDML super-index in `write_trailer`
/// once each segment's RIFF position is known.
fn write_strl<W: Write + Seek + ?Sized>(
    w: &mut W,
    _index: u32,
    t: &TrackState,
    with_indx: bool,
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
        // bIndexSubType, bIndexType already zero.
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

    finish_chunk(w, strl_off)?;
    Ok((indx_n_entries_off, indx_entries_start_off))
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
