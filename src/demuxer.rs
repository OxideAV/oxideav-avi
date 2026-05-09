//! AVI (RIFF/AVI) demuxer.
//!
//! On `open()`:
//! 1. Verify the top-level `RIFF`…`AVI ` header.
//! 2. Locate the `hdrl` LIST, parse `avih` (main header) and each `strl`
//!    LIST → `strh` (stream header) + `strf` (stream format) +
//!    optionally an `indx` super-index chunk (OpenDML 2.0).
//! 3. Locate the `movi` LIST. Remember its start offset and size so we can
//!    walk packet chunks lazily.
//! 4. If an `idx1` top-level chunk is present, parse it into an in-memory
//!    seek table (see [`IdxEntry`]).
//! 5. After the primary `RIFF AVI ` envelope, scan for additional
//!    `RIFF AVIX` continuation segments and append each one's `movi`
//!    LIST to the segment list (OpenDML 2.0 multi-RIFF carriage).
//!
//! `next_packet()` walks chunks inside every `movi` segment in
//! sequence: when one segment is exhausted it advances to the next.
//! Each payload chunk name is `NNxx` where `NN` is a two-ASCII-digit
//! stream index and `xx` is one of `dc` (compressed video), `db`
//! (uncompressed video), `wb` (audio), or something else which we
//! skip. Unknown or out-of-range indexes are skipped so we can
//! tolerate files with embedded junk (`JUNK`, `ix##`, unsupported
//! streams).
//!
//! `seek_to(stream, pts)` uses the AVI 1.0 `idx1` table when
//! present; OpenDML-driven seeking from the `indx` super-index is a
//! follow-up — `seek_to` returns `Error::Unsupported` when no `idx1`
//! was seen.
//!
//! ### Truncated-head tolerance
//!
//! Capture-card crash dumps and copy-aborted recordings often produce
//! AVI 1.0 files whose RIFF / `LIST hdrl` / `LIST movi` size fields
//! over-declare the bytes physically present (e.g. a 5 MiB head of
//! what was meant to be a 20 MiB capture, with `LIST movi
//! size=20353990`). The demuxer is **best-effort** for this case:
//!
//! 1. The actual file length is probed at `open()`; the top-level
//!    `RIFF` body and every `LIST` body offset are clamped so a
//!    declared size larger than the file becomes a logical end at
//!    end-of-file rather than an out-of-range seek that surfaces
//!    `read_exact` failures mid-walk.
//! 2. `walk_riff_body` treats a truncated 8-byte chunk header read at
//!    end-of-file as a clean stop (no more chunks) rather than
//!    propagating an "AVI: truncated chunk header" error — there is
//!    nothing more to parse, the file just ended early.
//! 3. `next_packet` returns `Error::Eof` when a `read_exact` mid-body
//!    short-reads (`UnexpectedEof`) instead of bubbling the I/O
//!    error up. Any frames wholly inside the file are still
//!    surfaced; the partial frame at the truncation boundary is
//!    dropped silently.
//!
//! Genuinely malformed inputs — wrong RIFF FourCC, recursive `LIST`
//! sizes inconsistent **before** the truncation point, missing
//! `hdrl`, missing `movi`, etc. — still error cleanly.

use std::io::{Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, Error, MediaType, Packet, ProbeContext,
    Rational, Result, SampleFormat, StreamInfo, TimeBase,
};
use oxideav_core::{Demuxer, ReadSeek};

use crate::riff::{read_chunk_header, read_form_type, skip_chunk, skip_pad, AVI_FORM, LIST, RIFF};
use crate::stream_format::{parse_bitmap_info_header, parse_waveformatex};

/// `bIndexType` of an `AVIMETAINDEX` super-index (`indx` of indexes).
const AVI_INDEX_OF_INDEXES: u8 = 0x00;
/// `bIndexType` of an `AVIMETAINDEX` chunk index (`ix##`).
const AVI_INDEX_OF_CHUNKS: u8 = 0x01;
/// `bIndexSubType` flag for a 2-field interlaced std-index (per OpenDML 2.0
/// §3.0 "AVI Field Index Chunk"). When set, each `aIndex` entry carries an
/// extra `dwOffsetField2` DWORD (so `wLongsPerEntry == 3` and entries are
/// 12 bytes instead of the default 8).
const AVI_INDEX_SUB_2FIELD: u8 = 0x01;
/// `dwSize` high bit in an `AVISTDINDEX_ENTRY` flags a non-keyframe (delta).
const AVISTDINDEX_DELTA_BIT: u32 = 0x8000_0000;

/// Soft cap on `indx` super-index entry count. Mirrors the muxer's
/// `OPENDML_SUPER_INDEX_CAPACITY` (256 slots = 4 KiB of payload). When
/// a parsed `indx` declares more entries than this, the demuxer
/// surfaces an `avi:indx.<stream>.overflow_entries` metadata key so
/// downstream tools can flag files written by encoders that didn't
/// pre-reserve enough super-index slots (round-5 candidate 4).
///
/// Per OpenDML 2.0 §3.0 "Super Index Chunk", `nEntriesInUse` is a
/// DWORD and a writer may legitimately reserve > 256 entries. We
/// don't truncate the parsed entry list (a downstream consumer may
/// still want every entry), we only signal the overflow.
const OPENDML_SUPER_INDEX_SOFT_CAP: usize = 256;

/// Factory registered with the container registry. Returns a boxed
/// trait object — callers that need AVI-specific accessors like
/// [`AviDemuxer::field2_offset_for_packet`] should use [`open_avi`]
/// instead.
pub fn open(input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(open_avi(input, codecs)?))
}

/// Open an AVI demuxer and return the concrete [`AviDemuxer`] so
/// callers can access AVI-specific accessors like
/// [`AviDemuxer::field2_offset_for_packet`] (round-5 candidate 1).
///
/// Same parsing behaviour as the trait-object [`open`]; the only
/// difference is the return type so callers can hold the concrete
/// handle alongside the [`oxideav_core::Demuxer`] trait it
/// implements.
///
/// Round-14 candidate 2: also runs the per-audio-stream
/// `(strh.dwSampleSize, wave_format.format_tag)` invariant check
/// — VBR codecs (MP3 / AAC / MPEG) require `dwSampleSize == 0`,
/// CBR codecs (PCM / G.711 / IMA-ADPCM) require `dwSampleSize > 0`.
/// A mismatch surfaces as [`Error::Validation`]. Use [`open_avi_lenient`]
/// to skip this check (e.g. when re-muxing a malformed legacy file).
pub fn open_avi(input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<AviDemuxer> {
    open_avi_inner(input, codecs, /* lenient */ false)
}

/// Open an AVI demuxer skipping the round-14 C2 audio sample-size
/// VBR/CBR validator. Use this when the caller wants to re-mux or
/// inspect a malformed legacy file whose `strh.dwSampleSize` doesn't
/// match the spec for its `wFormatTag`. All other open-time checks
/// still run — only the VBR/CBR invariant is bypassed.
pub fn open_avi_lenient(
    input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> Result<AviDemuxer> {
    open_avi_inner(input, codecs, /* lenient */ true)
}

fn open_avi_inner(
    mut input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
    lenient: bool,
) -> Result<AviDemuxer> {
    // Probe the actual file length so we can clamp over-declared chunk sizes
    // against it. Truncated-head AVI files (capture-card crash dumps,
    // copy-aborted recordings) routinely declare RIFF / LIST sizes that
    // exceed what's physically present; without this clamp the walker would
    // hit `read_exact` UnexpectedEof mid-stream instead of stopping cleanly.
    let file_len = probe_file_len(&mut *input)?;

    // Top-level RIFF chunk.
    let top = match read_chunk_header(&mut *input)? {
        Some(h) => h,
        None => return Err(Error::invalid("AVI: empty file")),
    };
    if top.id != RIFF {
        return Err(Error::invalid("AVI: not a RIFF file"));
    }
    let form = read_form_type(&mut *input)?;
    if form != AVI_FORM {
        return Err(Error::invalid("AVI: RIFF form type is not AVI"));
    }
    // Detect truncated-head by comparing the declared full RIFF length
    // (8 + top.size) to the physical file length. Over-declared by 8+
    // bytes ⇒ the file ends before its own RIFF claims to.
    let declared_riff_total = 8u64.saturating_add(top.size as u64);
    let truncated_head = declared_riff_total > file_len;
    // End of the primary RIFF (exclusive). `top.size` does not include the
    // 8-byte RIFF header itself; its body starts right after the 4-byte
    // form-type and ends at this offset. Clamp against the actual file
    // length so a truncated-head AVI doesn't surface an out-of-range walk.
    let riff_end = declared_riff_total.min(file_len);

    // Walk top-level nested chunks until we've processed both hdrl and movi.
    let mut streams: Vec<StreamInfo> = Vec::new();
    let mut packet_chunk_suffix: Vec<[u8; 2]> = Vec::new();
    // Multiple (start, end) movi segments: one inside the primary RIFF, plus
    // one per OpenDML `RIFF AVIX` extension RIFF that follows.
    let mut movi_segments: Vec<(u64, u64)> = Vec::new();
    let mut avih: Option<AviMainHeader> = None;
    let mut metadata: Vec<(String, String)> = Vec::new();
    let mut idx1_raw: Option<Vec<u8>> = None;
    // OpenDML 2.0 super-indexes, one per stream that declared an `indx`
    // chunk in its `strl` LIST. Empty for AVI 1.0 files. The vector is
    // indexed by stream number so a sparse population (only video carries
    // an `indx`) leaves audio entries empty.
    let mut super_indexes: Vec<SuperIndex> = Vec::new();
    // Per-stream `vprp` (Video Properties Header) per OpenDML 2.0 §5.0.
    // Default-initialised entries for streams that didn't declare a `vprp`.
    let mut vprps: Vec<VprpHeader> = Vec::new();
    // OpenDML 2.0 §5.0 `dmlh` extended-header `dwTotalFrames` value
    // (across all RIFF segments). `None` when no `LIST odml dmlh` was
    // seen in `hdrl`.
    let mut dmlh_total_frames: Option<u32> = None;
    // Per-stream audio strh `(format_tag, dwSampleSize)` capture for
    // the round-14 C2 VBR/CBR validator. Parallel to `streams`:
    // `Some` for audio, `None` otherwise.
    let mut audio_infos: Vec<Option<AudioStrhInfo>> = Vec::new();

    walk_riff_body(
        &mut *input,
        riff_end,
        file_len,
        &mut streams,
        &mut packet_chunk_suffix,
        &mut movi_segments,
        &mut avih,
        &mut metadata,
        &mut idx1_raw,
        &mut super_indexes,
        &mut vprps,
        &mut dmlh_total_frames,
        &mut audio_infos,
        codecs,
        /* is_primary */ true,
    )?;

    // OpenDML: additional `RIFF AVIX` extension segments may follow the
    // primary RIFF. Each holds more movi data.
    input.seek(SeekFrom::Start(riff_end))?;
    while let Some(hdr) = read_chunk_header_lenient(&mut *input)? {
        if hdr.id == RIFF {
            let form = read_form_type(&mut *input)?;
            let ext_end =
                (input.stream_position()? + hdr.size.saturating_sub(4) as u64).min(file_len);
            if &form == b"AVIX" {
                walk_riff_body(
                    &mut *input,
                    ext_end,
                    file_len,
                    &mut streams,
                    &mut packet_chunk_suffix,
                    &mut movi_segments,
                    &mut avih,
                    &mut metadata,
                    &mut idx1_raw,
                    &mut super_indexes,
                    &mut vprps,
                    &mut dmlh_total_frames,
                    &mut audio_infos,
                    codecs,
                    /* is_primary */ false,
                )?;
            }
            input.seek(SeekFrom::Start(ext_end))?;
            skip_pad(&mut *input, hdr.size)?;
        } else {
            skip_chunk(&mut *input, &hdr)?;
        }
    }

    // Round-14 candidate 2: audio `(format_tag, sample_size)` invariant.
    // Per AVI 1.0 / WAVEFORMATEX: VBR codecs (MPEG / MP3 / AAC) carry
    // one packet per audio frame so `dwSampleSize` MUST be 0; CBR
    // codecs (PCM / G.711 a-law / G.711 µ-law / IMA-ADPCM) carry a
    // fixed bytes-per-sample so `dwSampleSize` MUST be > 0. A mismatch
    // means the file lies about its own carriage and downstream
    // `strh.dwLength` derivations (AviMuxer's audio sample-count walk)
    // will be wrong. Skip when `lenient` (caller opted in via
    // [`open_avi_lenient`]) so a malformed legacy file can still be
    // re-muxed / inspected. Other format tags (codecs the spec doesn't
    // pin one way or the other — e.g. WMA, AC-3, custom registrations)
    // pass through with no constraint.
    if !lenient {
        for (i, ai) in audio_infos.iter().enumerate() {
            let info = match ai {
                Some(v) => v,
                None => continue,
            };
            if let Some(violation) = audio_strh_violation(info) {
                return Err(Error::invalid(format!(
                    "AVI: audio stream {i} (wFormatTag=0x{:04X}): {violation}",
                    info.format_tag
                )));
            }
        }
    }

    if movi_segments.is_empty() {
        return Err(Error::invalid("AVI: missing movi list"));
    }
    let movi_start = movi_segments[0].0;
    if streams.is_empty() {
        return Err(Error::invalid("AVI: no streams"));
    }

    // Duration: the AVI main header carries microseconds-per-frame and
    // total-frame-count for the primary (first) video stream. Multiply.
    let duration_micros: i64 = match avih {
        Some(h) if h.micro_sec_per_frame > 0 && h.total_frames > 0 => {
            (h.total_frames as i64) * (h.micro_sec_per_frame as i64)
        }
        _ => 0,
    };

    // Surface AVIMAINHEADER-derived diagnostics through `metadata()` —
    // see `Demuxer::metadata()`. Keys are namespaced under `avi:` so a
    // generic consumer can ignore them while a container-aware caller
    // (a player UI, a media-info dumper) can still display them.
    if let Some(h) = &avih {
        if h.width > 0 {
            metadata.push(("avi:width".into(), h.width.to_string()));
        }
        if h.height > 0 {
            metadata.push(("avi:height".into(), h.height.to_string()));
        }
        if h.streams > 0 {
            metadata.push(("avi:streams".into(), h.streams.to_string()));
        }
        if h.flags != 0 {
            metadata.push(("avi:flags".into(), format!("0x{:08X}", h.flags)));
        }
        if h.suggested_buffer_size > 0 {
            metadata.push((
                "avi:suggested_buffer_size".into(),
                h.suggested_buffer_size.to_string(),
            ));
        }
        if h.max_bytes_per_sec > 0 {
            metadata.push((
                "avi:max_bytes_per_sec".into(),
                h.max_bytes_per_sec.to_string(),
            ));
        }
    }
    // Truncated-head signal: capture-card crash dumps, copy-aborted
    // recordings. The demuxer is best-effort for this case (see
    // module docs) — a downstream tool can decide to surface a
    // warning to the user.
    if truncated_head {
        metadata.push(("avi:truncated".into(), "true".into()));
    }

    // OpenDML 2.0 §5.0 dmlh: real total-frame count across every RIFF
    // segment. `avih.dwTotalFrames` only reflects the primary segment
    // (per spec/06 §5.0 "Required Information"); a multi-segment file
    // built with the OpenDML envelope writes the cross-segment count
    // here. Surface as a separate key so the avih-derived
    // `avi:total_frames` (from `avih.total_frames`) stays
    // single-segment for legacy callers, while
    // `avi:total_frames_all_segments` carries the OpenDML truth.
    if let Some(total) = dmlh_total_frames {
        metadata.push(("avi:total_frames_all_segments".into(), total.to_string()));
    }

    // OpenDML 2.0 §5.0 vprp: surface signal-shape descriptors per
    // stream under `avi:vprp.<index>.*`. Skip default-zero headers
    // (streams without a `vprp` chunk) so absence is observable.
    for (i, vp) in vprps.iter().enumerate() {
        // A genuinely-present vprp will have at least one nonzero
        // field; a stream that didn't declare one leaves it
        // default-zero. Use `nb_field_per_frame` as the presence
        // signal — it's required to be 1 (progressive) or 2
        // (interlaced) per the spec.
        if vp.nb_field_per_frame == 0 {
            continue;
        }
        let prefix = format!("avi:vprp.{i}");
        metadata.push((
            format!("{prefix}.video_format_token"),
            vp.video_format_token.to_string(),
        ));
        metadata.push((
            format!("{prefix}.video_standard"),
            vp.video_standard.to_string(),
        ));
        if vp.vertical_refresh_rate > 0 {
            metadata.push((
                format!("{prefix}.vertical_refresh_rate"),
                vp.vertical_refresh_rate.to_string(),
            ));
        }
        if vp.h_total_in_t > 0 {
            metadata.push((
                format!("{prefix}.h_total_in_t"),
                vp.h_total_in_t.to_string(),
            ));
        }
        if vp.v_total_in_lines > 0 {
            metadata.push((
                format!("{prefix}.v_total_in_lines"),
                vp.v_total_in_lines.to_string(),
            ));
        }
        if vp.frame_aspect_ratio > 0 {
            // Encode as "X:Y" for human consumption; the high WORD is
            // X, low WORD is Y per spec/06 §5.0 "Active Frame Aspect
            // Ratio".
            let x = (vp.frame_aspect_ratio >> 16) & 0xFFFF;
            let y = vp.frame_aspect_ratio & 0xFFFF;
            metadata.push((format!("{prefix}.frame_aspect_ratio"), format!("{x}:{y}")));
        }
        if vp.frame_width_in_pixels > 0 {
            metadata.push((
                format!("{prefix}.frame_width_in_pixels"),
                vp.frame_width_in_pixels.to_string(),
            ));
        }
        if vp.frame_height_in_lines > 0 {
            metadata.push((
                format!("{prefix}.frame_height_in_lines"),
                vp.frame_height_in_lines.to_string(),
            ));
        }
        metadata.push((
            format!("{prefix}.nb_field_per_frame"),
            vp.nb_field_per_frame.to_string(),
        ));
        // Round-9 candidate 1: per-field VIDEO_FIELD_DESC rects. The
        // 8 DWORDs per record describe (compressed_bm_*, valid_bm_*
        // dims + offset, video_x_offset_in_t, video_y_valid_start_line).
        // Surface each non-default field as
        // `avi:vprp.<i>.field<j>.<key>` so downstream consumers wanting
        // per-field rendering (interlaced PAL/NTSC) can read the
        // active rectangle without re-parsing the chunk. Skip
        // all-zero records (default-init / muxer-emitted placeholder
        // for streams that didn't supply a real rect).
        for (j, fd) in vp.field_descs.iter().enumerate() {
            let all_zero = fd.compressed_bm_height == 0
                && fd.compressed_bm_width == 0
                && fd.valid_bm_height == 0
                && fd.valid_bm_width == 0
                && fd.valid_bm_x_offset == 0
                && fd.valid_bm_y_offset == 0
                && fd.video_x_offset_in_t == 0
                && fd.video_y_valid_start_line == 0;
            if all_zero {
                continue;
            }
            let fp = format!("{prefix}.field{j}");
            if fd.compressed_bm_height > 0 {
                metadata.push((
                    format!("{fp}.compressed_bm_height"),
                    fd.compressed_bm_height.to_string(),
                ));
            }
            if fd.compressed_bm_width > 0 {
                metadata.push((
                    format!("{fp}.compressed_bm_width"),
                    fd.compressed_bm_width.to_string(),
                ));
            }
            if fd.valid_bm_height > 0 {
                metadata.push((
                    format!("{fp}.valid_bm_height"),
                    fd.valid_bm_height.to_string(),
                ));
            }
            if fd.valid_bm_width > 0 {
                metadata.push((
                    format!("{fp}.valid_bm_width"),
                    fd.valid_bm_width.to_string(),
                ));
            }
            if fd.valid_bm_x_offset > 0 {
                metadata.push((
                    format!("{fp}.valid_bm_x_offset"),
                    fd.valid_bm_x_offset.to_string(),
                ));
            }
            if fd.valid_bm_y_offset > 0 {
                metadata.push((
                    format!("{fp}.valid_bm_y_offset"),
                    fd.valid_bm_y_offset.to_string(),
                ));
            }
            if fd.video_x_offset_in_t > 0 {
                metadata.push((
                    format!("{fp}.video_x_offset_in_t"),
                    fd.video_x_offset_in_t.to_string(),
                ));
            }
            if fd.video_y_valid_start_line > 0 {
                metadata.push((
                    format!("{fp}.video_y_valid_start_line"),
                    fd.video_y_valid_start_line.to_string(),
                ));
            }
        }
    }

    // Build the seek table from idx1 (if present). `build_idx_table` resolves
    // the per-file offset base (file-absolute vs movi-relative) by probing
    // the first entry against the known chunk header.
    //
    // Round-8 candidate 3: while we have raw idx1 in hand, also scan
    // it for `xxpc` palette-change chunks (FourCC ending in `pc` —
    // see `aviriff.h`'s `cktypePALchange = "PC"`). idx1 is the
    // canonical static list of every chunk in movi so this is the
    // cheapest place to count them — no second movi pass.
    //
    // Round-10 candidate 1: same trick for `xxtx` text/subtitle
    // chunks (FourCC ending in `tx` — `mmsystem.h`'s text-stream
    // FourCC family `ckidAVITextSF`). They're not video data so the
    // packet stream still skips them, but we expose a per-stream
    // count both via metadata (`avi:text_chunk.<n>`) and a typed
    // [`AviDemuxer::text_chunk_count`] accessor for parallel use
    // with [`AviDemuxer::palette_change_count`].
    let mut palette_change_counts: Vec<u32> = vec![0u32; streams.len()];
    let mut text_chunk_counts: Vec<u32> = vec![0u32; streams.len()];
    // Round-12 candidate 1: also buffer the actual `xxpc`/`xxtx` chunk
    // bodies eagerly when `idx1` is present so callers can inspect
    // them via `palette_change_data` / `text_chunk_data` without
    // first walking every regular packet via `next_packet`.
    let mut palette_change_data: Vec<Vec<Vec<u8>>> = vec![Vec::new(); streams.len()];
    let mut text_chunk_data: Vec<Vec<Vec<u8>>> = vec![Vec::new(); streams.len()];
    let mut sideband_data_loaded = false;
    let idx_table = if let Some(raw) = idx1_raw {
        scan_idx1_for_suffix(&raw, &streams, *b"pc", &mut palette_change_counts);
        scan_idx1_for_suffix(&raw, &streams, *b"tx", &mut text_chunk_counts);
        read_sideband_data_from_idx1(
            &mut *input,
            &raw,
            movi_start,
            &streams,
            *b"pc",
            &mut palette_change_data,
        );
        read_sideband_data_from_idx1(
            &mut *input,
            &raw,
            movi_start,
            &streams,
            *b"tx",
            &mut text_chunk_data,
        );
        sideband_data_loaded = true;
        build_idx_table(&mut *input, &raw, movi_start, &streams)?
    } else {
        Vec::new()
    };
    // Surface non-zero palette-change counts as metadata so callers
    // walking `Demuxer::metadata()` can detect palette animation
    // without calling the typed accessor.
    for (s, &count) in palette_change_counts.iter().enumerate() {
        if count > 0 {
            metadata.push((format!("avi:palette_change.{s}"), count.to_string()));
        }
    }
    // Round-10 C1: same shape for `xxtx` text/subtitle chunks.
    for (s, &count) in text_chunk_counts.iter().enumerate() {
        if count > 0 {
            metadata.push((format!("avi:text_chunk.{s}"), count.to_string()));
        }
    }

    // OpenDML 2.0 standard-index scan: walk every `movi` segment looking
    // for `ix##` chunks. Each maps back to one stream via the two ASCII
    // digits at the start of its FourCC. We perform this regardless of
    // whether a `super_index` was declared in `strl`, because some
    // writers emit `ix##` directly without a corresponding `indx` slot.
    // Trigger ix## scan when ANY of:
    //   - a super-index with at least one resolved entry was parsed,
    //   - more than one movi segment exists (OpenDML multi-RIFF),
    //   - any super-index declares `bIndexSubType = AVI_INDEX_2FIELD`
    //     (round-4 P3) — even a single-segment file may carry
    //     2-field std-indexes that we need to surface for downstream
    //     consumers, and the primary segment's qwOffset = 0 makes
    //     parse_indx drop the entry slot per spec/06's "0 is unused"
    //     convention.
    let want_ix_scan = super_indexes.iter().any(|s| !s.entries.is_empty())
        || movi_segments.len() > 1
        || super_indexes
            .iter()
            .any(|s| s.b_index_sub_type == AVI_INDEX_SUB_2FIELD);
    let std_indexes = if want_ix_scan {
        scan_ix_in_movi(&mut *input, &movi_segments).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Round-4 P3: surface per-stream 2-field signalling so downstream
    // consumers can detect interlaced AVIs from `Demuxer::metadata`.
    // For every stream whose ix## carries
    // `bIndexSubType == AVI_INDEX_2FIELD` we emit
    // `avi:ix.<index>.is_2field = true` and the comma-separated list
    // of `dwOffsetField2` values (qwBaseOffset-relative).
    let mut field2_streams_seen: std::collections::BTreeSet<u32> =
        std::collections::BTreeSet::new();
    {
        use std::collections::BTreeMap;
        let mut per_stream_offsets: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let mut per_stream_2field: BTreeMap<u32, bool> = BTreeMap::new();
        for ix in &std_indexes {
            if let Some(stream) = parse_stream_index(&ix.chunk_id) {
                if ix.b_index_sub_type == AVI_INDEX_SUB_2FIELD {
                    per_stream_2field.insert(stream, true);
                    let v = per_stream_offsets.entry(stream).or_default();
                    for e in &ix.entries {
                        v.push(e.dw_offset_field2);
                    }
                }
            }
        }
        for (stream, _) in per_stream_2field.iter() {
            metadata.push((format!("avi:ix.{stream}.is_2field"), "true".into()));
            field2_streams_seen.insert(*stream);
            if let Some(offsets) = per_stream_offsets.get(stream) {
                let joined = offsets
                    .iter()
                    .map(|o| o.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                metadata.push((format!("avi:ix.{stream}.field2_offsets"), joined));
            }
        }
    }

    // Round-5 candidate 4: surface a soft-cap warning when a parsed
    // `indx` super-index declared more entries than the conventional
    // 256-slot reserve. Per OpenDML 2.0 §3.0 "Super Index Chunk" the
    // `nEntriesInUse` field is a DWORD so this is technically valid,
    // but an entry count beyond ~256 is unusual and may signal a
    // writer that didn't allow for fixed-slot back-patching (the
    // round-trip muxer caps at 256 and silently drops the tail).
    // Emitting `avi:indx.<stream>.overflow_entries` lets downstream
    // tools flag the file even if seek still works.
    for (i, sx) in super_indexes.iter().enumerate() {
        if sx.entries.len() > OPENDML_SUPER_INDEX_SOFT_CAP {
            metadata.push((
                format!("avi:indx.{i}.overflow_entries"),
                sx.entries.len().to_string(),
            ));
        }
    }

    // Round-5 candidate 2: when an idx1 table is present alongside
    // an `AVI_INDEX_2FIELD` ix## for the same stream, surface a
    // per-stream "interlaced via idx1" hint at the idx1 layer. The
    // AVI 1.0 idx1 entry layout (`AVIINDEXENTRY`) doesn't define
    // its own field-2 columns, but vfw.h's `AVIIF_FIRSTPART` /
    // `AVIIF_LASTPART` flags semantically mean "this idx1 entry's
    // chunk is the first/last part of a multi-part frame". For our
    // single-chunk-per-frame 2-field carriage both bits would be
    // set on every idx1 entry; rather than rewrite the parsed
    // flags we surface the equivalent `avi:idx1.<stream>.is_2field`
    // metadata key so consumers can apply field-aware rendering
    // when seeking via idx1 too.
    if !idx_table.is_empty() {
        for s in &field2_streams_seen {
            // Only stamp the hint when idx1 actually carries entries
            // for this stream — otherwise the idx1 layer doesn't
            // describe field carriage anyway.
            let any = idx_table.iter().any(|e| e.stream == *s);
            if any {
                metadata.push((format!("avi:idx1.{s}.is_2field"), "true".into()));
            }
        }
    }

    // Round-6 candidate 1: detect 2-field carriage from the idx1
    // flag bits themselves. The muxer sets
    // `AVIIF_FIRSTPART | AVIIF_LASTPART` (= 0x60) on every idx1
    // entry for a 2-field stream so AVI-1.0-only readers (no ix##
    // available) can still detect interlaced carriage by looking
    // at the index alone. We surface the per-stream hint when
    // EVERY idx1 entry for that stream carries both bits — a
    // partial pattern would indicate genuine multi-part-frame
    // carriage (very rare; legacy capture cards) rather than
    // 2-field interlace.
    if !idx_table.is_empty() {
        const PART_BOTH: u32 = 0x0020 | 0x0040; // AVIIF_FIRSTPART | AVIIF_LASTPART
        for s in 0..(streams.len() as u32) {
            // Skip streams that already produced an `avi:idx1.<n>.is_2field`
            // hint via the ix##-driven path above.
            if field2_streams_seen.contains(&s) {
                continue;
            }
            let mut entries = 0usize;
            let mut all_part_both = true;
            for e in idx_table.iter().filter(|e| e.stream == s) {
                entries += 1;
                if (e.flags & PART_BOTH) != PART_BOTH {
                    all_part_both = false;
                    break;
                }
            }
            if entries > 0 && all_part_both {
                metadata.push((format!("avi:idx1.{s}.is_2field"), "true".into()));
            }
        }
    }

    // Pad super_indexes to streams.len() so per-stream lookup is always
    // safe even if some strl LISTs didn't declare an indx.
    while super_indexes.len() < streams.len() {
        super_indexes.push(SuperIndex::default());
    }

    // Round-8 candidate 1: pre-compute the per-stream idx1-flags
    // lookup table once so [`AviDemuxer::idx1_flags_for_packet`] is
    // O(1) instead of O(N). idx_table is in file order so a single
    // pass populates each stream's per-packet flags Vec in
    // packet_seq order.
    let mut idx1_flags_per_stream: Vec<Vec<u32>> = vec![Vec::new(); streams.len()];
    for e in &idx_table {
        let s = e.stream as usize;
        if s < idx1_flags_per_stream.len() {
            idx1_flags_per_stream[s].push(e.flags);
        }
    }

    // Seek to start of first movi body for next_packet.
    input.seek(SeekFrom::Start(movi_start))?;

    Ok(AviDemuxer {
        input,
        streams,
        packet_chunk_suffix,
        movi_start,
        movi_segments,
        current_segment: 0,
        per_stream_counter: Vec::new(),
        metadata,
        duration_micros,
        idx_table,
        super_indexes,
        std_indexes,
        idx1_flags_per_stream,
        palette_change_counts,
        text_chunk_counts,
        avih_flags: avih.as_ref().map(|h| h.flags).unwrap_or(0),
        avih_suggested_buffer_size: avih.as_ref().map(|h| h.suggested_buffer_size).unwrap_or(0),
        vprps,
        dmlh_total_frames,
        palette_change_data,
        text_chunk_data,
        sideband_data_loaded,
    })
}

/// Walk the body of one RIFF (`AVI ` or `AVIX`). Collects `hdrl` metadata
/// (only the primary RIFF carries it), records every `LIST movi` as a
/// segment, and reads `idx1` if present. `end` is the exclusive end offset
/// of this RIFF's body; `file_len` is the underlying stream length used to
/// clamp over-declared `LIST` body sizes (truncated-head tolerance).
#[allow(clippy::too_many_arguments)]
fn walk_riff_body(
    input: &mut dyn ReadSeek,
    end: u64,
    file_len: u64,
    streams: &mut Vec<StreamInfo>,
    packet_chunk_suffix: &mut Vec<[u8; 2]>,
    movi_segments: &mut Vec<(u64, u64)>,
    avih: &mut Option<AviMainHeader>,
    metadata: &mut Vec<(String, String)>,
    idx1_raw: &mut Option<Vec<u8>>,
    super_indexes: &mut Vec<SuperIndex>,
    vprps: &mut Vec<VprpHeader>,
    dmlh_total_frames: &mut Option<u32>,
    audio_infos: &mut Vec<Option<AudioStrhInfo>>,
    codecs: &dyn CodecResolver,
    is_primary: bool,
) -> Result<()> {
    while input.stream_position()? < end {
        let hdr = match read_chunk_header_lenient(input)? {
            Some(h) => h,
            None => break,
        };
        if hdr.id == LIST {
            let list_type = read_form_type(input)?;
            let body_len = hdr.size.saturating_sub(4);
            let body_start = input.stream_position()?;
            // Clamp the declared body end to the enclosing RIFF and to the
            // physical file length. AVI 1.0 capture dumps regularly
            // over-declare `LIST movi` size when the recording was
            // truncated; without clamping, downstream `read_exact` calls
            // walk into UnexpectedEof.
            let body_end = (body_start + body_len as u64).min(end).min(file_len);
            match &list_type {
                b"hdrl" if is_primary => {
                    let (main, stream_infos, suffixes, sxs, vps, dmlh, info_md, ais) =
                        parse_hdrl(input, body_end, codecs)?;
                    *avih = Some(main);
                    *streams = stream_infos;
                    *packet_chunk_suffix = suffixes;
                    *super_indexes = sxs;
                    *vprps = vps;
                    *dmlh_total_frames = dmlh;
                    metadata.extend(info_md);
                    *audio_infos = ais;
                }
                b"movi" => {
                    movi_segments.push((body_start, body_end));
                }
                b"INFO" if is_primary => {
                    let avail = body_end.saturating_sub(body_start) as usize;
                    let mut buf = vec![0u8; avail];
                    let _ = read_up_to(input, &mut buf)?;
                    parse_info_list(&buf, metadata);
                }
                _ => {}
            }
            input.seek(SeekFrom::Start(body_end))?;
            skip_pad(input, hdr.size)?;
        } else if &hdr.id == b"idx1" && is_primary {
            // Clamp idx1 size against remaining bytes so a truncation
            // partway through the index doesn't fail open(); we just take
            // whatever entries fit. Each entry is 16 B so a partial entry
            // at the tail is dropped by build_idx_table's `n = raw.len() / 16`.
            let pos = input.stream_position()?;
            let avail = file_len.saturating_sub(pos);
            let take = (hdr.size as u64).min(avail) as usize;
            let mut buf = vec![0u8; take];
            let read = read_up_to(input, &mut buf)?;
            buf.truncate(read);
            // Skip any remaining declared bytes (best-effort) + pad.
            let remaining = hdr.size as u64 - read as u64;
            if remaining > 0 {
                let _ = input.seek(SeekFrom::Current(remaining as i64));
            }
            skip_pad(input, hdr.size)?;
            *idx1_raw = Some(buf);
        } else {
            skip_chunk(input, &hdr)?;
        }
    }
    Ok(())
}

/// Read a chunk header tolerantly: at end-of-file (or when fewer than
/// 8 bytes remain), return `Ok(None)` rather than the strict error
/// "AVI: truncated chunk header" used by `read_chunk_header`. Used by
/// `walk_riff_body` so a RIFF whose declared size over-runs the
/// physical file ends cleanly instead of bubbling up an error.
fn read_chunk_header_lenient<R: std::io::Read + ?Sized>(
    r: &mut R,
) -> Result<Option<crate::riff::ChunkHeader>> {
    let mut buf = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        match r.read(&mut buf[got..]) {
            Ok(0) => return Ok(None),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let id = [buf[0], buf[1], buf[2], buf[3]];
    let size = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Ok(Some(crate::riff::ChunkHeader { id, size }))
}

/// Read up to `buf.len()` bytes; return how many were actually read
/// (may be `0` at EOF, may be less than `buf.len()` on truncation).
/// Unlike `read_exact`, never fails on short reads.
fn read_up_to<R: std::io::Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(got)
}

/// Parse a `LIST INFO` body (the 4-byte "INFO" form-type has already been
/// consumed). Each child is a 4-CC chunk whose payload is a NUL-terminated
/// string. Maps known FourCCs to standard metadata keys; round-7 candidate
/// 2 surfaces every other FourCC under `avi:info.<fourcc>` so callers
/// wanting full fidelity (no metadata loss) can still read entries the
/// well-known map doesn't recognise.
fn parse_info_list(buf: &[u8], out: &mut Vec<(String, String)>) {
    let mut i = 0usize;
    while i + 8 <= buf.len() {
        let id: [u8; 4] = [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]];
        let size = u32::from_le_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]]) as usize;
        i += 8;
        if i + size > buf.len() {
            break;
        }
        let raw = &buf[i..i + size];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let value = String::from_utf8_lossy(&raw[..end]).trim().to_string();
        if !value.is_empty() {
            match info_id_to_key(&id) {
                Some(k) => out.push((k.to_string(), value)),
                None => {
                    // Round-7 candidate 2: surface unknown FourCCs
                    // under `avi:info.<fourcc>` rather than dropping
                    // them silently. The FourCC is preserved verbatim
                    // when it's printable ASCII; otherwise we encode
                    // the raw bytes as `tag_<hex>` so the key stays
                    // legal UTF-8 (mirrors the demuxer's
                    // `avi:tag_<hex>` fallback for unrecognised
                    // codec tags).
                    let key = if id.iter().all(|b| b.is_ascii_graphic()) {
                        format!("avi:info.{}", std::str::from_utf8(&id).unwrap_or("____"))
                    } else {
                        format!(
                            "avi:info.tag_{:02x}{:02x}{:02x}{:02x}",
                            id[0], id[1], id[2], id[3]
                        )
                    };
                    out.push((key, value));
                }
            }
        }
        i += size;
        if size % 2 == 1 {
            i += 1;
        }
    }
}

fn info_id_to_key(id: &[u8; 4]) -> Option<&'static str> {
    match id {
        b"INAM" => Some("title"),
        b"IART" => Some("artist"),
        b"IPRD" => Some("album"),
        b"ICMT" => Some("comment"),
        b"ICRD" => Some("date"),
        b"IGNR" => Some("genre"),
        b"ICOP" => Some("copyright"),
        b"IENG" => Some("engineer"),
        b"ITCH" => Some("technician"),
        b"ISFT" => Some("encoder"),
        b"ISBJ" => Some("subject"),
        b"ITRK" => Some("track"),
        _ => None,
    }
}

/// Decoded AVIMAINHEADER (dwMicroSecPerFrame / … struct).
///
/// Per Microsoft's `aviriff.h` `AVIMAINHEADER` definition (see
/// `docs/container/riff/avi-riff-file-reference.md` Appendix A). Fields
/// kept beyond what the demuxer's seek logic needs are surfaced via
/// `Demuxer::metadata()` under the `avi:*` namespace so callers can
/// inspect AVIMAINHEADER without re-parsing the file.
#[derive(Clone, Copy, Debug, Default)]
struct AviMainHeader {
    micro_sec_per_frame: u32,
    max_bytes_per_sec: u32,
    flags: u32,
    total_frames: u32,
    #[allow(dead_code)]
    initial_frames: u32,
    streams: u32,
    suggested_buffer_size: u32,
    width: u32,
    height: u32,
}

/// Parse the AVIMAINHEADER body (should be 56 bytes).
fn parse_avih(buf: &[u8]) -> Result<AviMainHeader> {
    if buf.len() < 40 {
        return Err(Error::invalid("AVI: avih too short"));
    }
    Ok(AviMainHeader {
        micro_sec_per_frame: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
        max_bytes_per_sec: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        // dwPaddingGranularity at offset 8 is ignored.
        flags: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        total_frames: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
        initial_frames: u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
        streams: u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]),
        suggested_buffer_size: u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]),
        width: u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
        height: u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]),
    })
}

/// Bundle of values returned from [`parse_hdrl`]: the parsed
/// [`AviMainHeader`], the list of per-stream [`StreamInfo`]s, the
/// matching list of packet-chunk suffixes (e.g. `b"dc"`, `b"wb"`),
/// the OpenDML 2.0 super-index per stream (empty for streams that
/// don't declare an `indx` chunk in their `strl`), the per-stream
/// [`VprpHeader`] (empty for streams without a `vprp` chunk), the
/// optional `dmlh` extended-header `dwTotalFrames` value (`Some`
/// only when `LIST odml dmlh` was present), the metadata pairs
/// parsed from any hdrl-nested `LIST INFO` (round-6 candidate 2;
/// empty when no nested `LIST INFO` is present), and the per-stream
/// audio-strh `(format_tag, sample_size)` capture (round-14
/// candidate 2 — used by the VBR/CBR validator at `open_avi`).
/// `audio_infos` is parallel to `streams`: `Some` for audio streams,
/// `None` for video / data streams.
type HdrlOutput = (
    AviMainHeader,
    Vec<StreamInfo>,
    Vec<[u8; 2]>,
    Vec<SuperIndex>,
    Vec<VprpHeader>,
    Option<u32>,
    Vec<(String, String)>,
    Vec<Option<AudioStrhInfo>>,
);

/// Parse the `hdrl` LIST body.
///
/// Reads `avih`, then walks each nested `strl` LIST to build one `StreamInfo`
/// per stream. The `LIST odml` child carrying the `dmlh` extended header
/// (per OpenDML 2.0 §5.0 "Source and Header Information Storage") is
/// parsed when present; its single `dwTotalFrames` DWORD is returned so
/// the demuxer can surface it as `avi:total_frames_all_segments`.
/// See [`HdrlOutput`] for the return shape.
fn parse_hdrl<R: ReadSeek + ?Sized>(
    r: &mut R,
    end_pos: u64,
    codecs: &dyn CodecResolver,
) -> Result<HdrlOutput> {
    let mut main = AviMainHeader::default();
    let mut streams: Vec<StreamInfo> = Vec::new();
    let mut suffixes: Vec<[u8; 2]> = Vec::new();
    let mut super_indexes: Vec<SuperIndex> = Vec::new();
    let mut vprps: Vec<VprpHeader> = Vec::new();
    let mut audio_infos: Vec<Option<AudioStrhInfo>> = Vec::new();
    let mut dmlh_total_frames: Option<u32> = None;
    let mut info_metadata: Vec<(String, String)> = Vec::new();

    while r.stream_position()? < end_pos {
        let hdr = match read_chunk_header(r)? {
            Some(h) => h,
            None => break,
        };
        match &hdr.id {
            b"avih" => {
                let body = read_body_bounded(r, hdr.size)?;
                main = parse_avih(&body)?;
                skip_pad(r, hdr.size)?;
            }
            b"LIST" => {
                let list_type = read_form_type(r)?;
                let body_len = hdr.size.saturating_sub(4);
                let body_start = r.stream_position()?;
                let body_end = body_start + body_len as u64;
                if &list_type == b"strl" {
                    let (si, suf, sx, vp, ai) =
                        parse_strl(r, body_end, streams.len() as u32, codecs)?;
                    if let Some(si) = si {
                        streams.push(si);
                        suffixes.push(suf.unwrap_or(*b"xx"));
                        super_indexes.push(sx);
                        vprps.push(vp);
                        audio_infos.push(ai);
                    }
                } else if &list_type == b"odml" {
                    // OpenDML 2.0 extended AVI header: `LIST odml dmlh`.
                    // `dmlh`'s body is a single DWORD (`dwTotalFrames`)
                    // covering the real total-frame count across every
                    // RIFF segment (whereas `avih.dwTotalFrames` only
                    // reflects the primary segment per spec/06 §5.0).
                    dmlh_total_frames = parse_odml_list(r, body_end)?;
                } else if &list_type == b"INFO" {
                    // Round-6 candidate 2: AVI 1.0 spec permits
                    // `LIST INFO` either as a top-level RIFF child or
                    // as a child of `hdrl`. The top-level placement is
                    // already handled in `walk_riff_body`; this branch
                    // covers the hdrl-nested form (which the round-6
                    // muxer emits to keep INFO close to the per-stream
                    // metadata it documents).
                    let avail = body_end.saturating_sub(body_start) as usize;
                    let mut buf = vec![0u8; avail];
                    let _ = read_up_to(r, &mut buf)?;
                    parse_info_list(&buf, &mut info_metadata);
                }
                r.seek(SeekFrom::Start(body_end))?;
                skip_pad(r, hdr.size)?;
            }
            _ => {
                skip_chunk(r, &hdr)?;
            }
        }
    }
    Ok((
        main,
        streams,
        suffixes,
        super_indexes,
        vprps,
        dmlh_total_frames,
        info_metadata,
        audio_infos,
    ))
}

/// Parse a `LIST odml` body for the `dmlh` extended-header chunk.
///
/// `dmlh` carries a single 32-bit `dwTotalFrames` value across all RIFF
/// segments (per OpenDML 2.0 §5.0 "Source and Header Information
/// Storage" / "Extended AVI Header"). Some encoders pad `dmlh` past the
/// nominal 4 bytes; we only consume the first DWORD.
fn parse_odml_list<R: ReadSeek + ?Sized>(r: &mut R, end_pos: u64) -> Result<Option<u32>> {
    while r.stream_position()? < end_pos {
        let hdr = match read_chunk_header(r)? {
            Some(h) => h,
            None => break,
        };
        if &hdr.id == b"dmlh" {
            // dmlh body is at minimum 4 bytes; some writers emit a
            // larger zero-padded body — read what's there and pick the
            // first DWORD.
            let take = (hdr.size as u64).min(4096) as u32;
            let body = read_body_bounded(r, take)?;
            // Skip any trailing bytes past what we read.
            let remaining = (hdr.size as u64).saturating_sub(take as u64);
            if remaining > 0 {
                r.seek(SeekFrom::Current(remaining as i64))?;
            }
            skip_pad(r, hdr.size)?;
            if body.len() >= 4 {
                let total = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                return Ok(Some(total));
            }
            return Ok(None);
        }
        skip_chunk(r, &hdr)?;
    }
    Ok(None)
}

/// 5-tuple returned by [`parse_strl`]: optional [`StreamInfo`],
/// optional packet-chunk suffix, [`SuperIndex`] (default-empty when
/// no `indx`), [`VprpHeader`] (default when no `vprp`), and the
/// audio-stream's `(format_tag, sample_size)` pair when the strh
/// declared `fccType == "auds"` (round-14 C2 — used by the VBR/CBR
/// validator at `open_avi`).
type StrlOutput = (
    Option<StreamInfo>,
    Option<[u8; 2]>,
    SuperIndex,
    VprpHeader,
    Option<AudioStrhInfo>,
);

/// Parse a `strl` LIST. Returns the `StreamInfo`, expected packet
/// suffix, the OpenDML 2.0 [`SuperIndex`] parsed from the `indx`
/// chunk inside the strl (empty if absent), and the [`VprpHeader`]
/// parsed from any `vprp` chunk (default if absent).
fn parse_strl<R: ReadSeek + ?Sized>(
    r: &mut R,
    end_pos: u64,
    index: u32,
    codecs: &dyn CodecResolver,
) -> Result<StrlOutput> {
    let mut strh_buf: Option<Vec<u8>> = None;
    let mut strf_buf: Option<Vec<u8>> = None;
    let mut super_index = SuperIndex::default();
    let mut vprp = VprpHeader::default();
    while r.stream_position()? < end_pos {
        let hdr = match read_chunk_header(r)? {
            Some(h) => h,
            None => break,
        };
        match &hdr.id {
            b"strh" => {
                strh_buf = Some(read_body_bounded(r, hdr.size)?);
                skip_pad(r, hdr.size)?;
            }
            b"strf" => {
                strf_buf = Some(read_body_bounded(r, hdr.size)?);
                skip_pad(r, hdr.size)?;
            }
            b"indx" => {
                // OpenDML 2.0 super-index (AVI_INDEX_OF_INDEXES).
                // Layout (preamble, 24 B):
                //   WORD  wLongsPerEntry  (= 4 for super-index)
                //   BYTE  bIndexSubType
                //   BYTE  bIndexType      (= 0x00 AVI_INDEX_OF_INDEXES)
                //   DWORD nEntriesInUse
                //   DWORD dwChunkId       (e.g. '00dc')
                //   DWORD dwReserved[3]
                // Followed by 16-byte entries: qwOffset (u64), dwSize
                // (u32), dwDuration (u32). Each entry points at one
                // `ix##` standard-index chunk in a movi LIST (typically
                // a different RIFF segment for OpenDML 2.0 multi-RIFF
                // files). The standard-index chunks themselves are
                // located opportunistically during the per-segment
                // movi scan in `scan_ix_in_movi`.
                let body = read_body_bounded(r, hdr.size)?;
                skip_pad(r, hdr.size)?;
                super_index = parse_indx(&body)?;
            }
            b"vprp" => {
                // OpenDML 2.0 §5.0 "Video Properties Header" — captures
                // pixel-aspect / NTSC-PAL-SECAM token / framing flags
                // for a video stream. Optional; absent on most files.
                let body = read_body_bounded(r, hdr.size)?;
                skip_pad(r, hdr.size)?;
                if let Some(parsed) = parse_vprp(&body) {
                    vprp = parsed;
                }
            }
            _ => {
                skip_chunk(r, &hdr)?;
            }
        }
    }
    let strh = match strh_buf {
        Some(b) => b,
        None => return Ok((None, None, super_index, vprp, None)),
    };
    let strf = strf_buf.unwrap_or_default();
    let parsed = build_stream(index, &strh, &strf, codecs)?;
    Ok((Some(parsed.0), Some(parsed.1), super_index, vprp, parsed.2))
}

/// Parse a `vprp` (Video Properties Header) chunk per OpenDML 2.0 §5.0.
///
/// Layout (9 fixed DWORDs = 36 B, then `nbFieldPerFrame * 32 B` of
/// `VIDEO_FIELD_DESC` records):
///
/// ```text
///   DWORD VideoFormatToken
///   DWORD VideoStandard
///   DWORD dwVerticalRefreshRate
///   DWORD dwHTotalInT
///   DWORD dwVTotalInLines
///   DWORD dwFrameAspectRatio   (high WORD = X, low WORD = Y)
///   DWORD dwFrameWidthInPixels
///   DWORD dwFrameHeightInLines
///   DWORD nbFieldPerFrame
///   VIDEO_FIELD_DESC FieldInfo[nbFieldPerFrame]   // 8 DWORDs each = 32 B
/// ```
///
/// Returns `None` when the chunk is shorter than 36 B (the fixed
/// preamble); returns the parsed header even when the trailing
/// per-field-rect array is missing or truncated (round-9 candidate 1
/// surfaces whatever rect records fit in the body, capped at
/// `nb_field_per_frame`).
fn parse_vprp(body: &[u8]) -> Option<VprpHeader> {
    if body.len() < 36 {
        return None;
    }
    let read_dword = |off: usize| -> u32 {
        u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]])
    };
    let nb_field_per_frame = read_dword(32);
    // Cap parse against:
    //   1. nbFieldPerFrame (the spec's intent),
    //   2. the bytes actually present after the 36-byte preamble (some
    //      writers truncate the tail; we surface the prefix that fits),
    //   3. a sanity ceiling of 8 — production AVIs never declare more
    //      than 2 fields/frame, so any larger value is almost certainly
    //      garbage.
    let max_descs_by_body = (body.len().saturating_sub(36)) / 32;
    let n = (nb_field_per_frame as usize).min(max_descs_by_body).min(8);
    let mut field_descs = Vec::with_capacity(n);
    for i in 0..n {
        let base = 36 + i * 32;
        field_descs.push(VprpFieldDesc {
            compressed_bm_height: read_dword(base),
            compressed_bm_width: read_dword(base + 4),
            valid_bm_height: read_dword(base + 8),
            valid_bm_width: read_dword(base + 12),
            valid_bm_x_offset: read_dword(base + 16),
            valid_bm_y_offset: read_dword(base + 20),
            video_x_offset_in_t: read_dword(base + 24),
            video_y_valid_start_line: read_dword(base + 28),
        });
    }
    Some(VprpHeader {
        video_format_token: read_dword(0),
        video_standard: read_dword(4),
        vertical_refresh_rate: read_dword(8),
        h_total_in_t: read_dword(12),
        v_total_in_lines: read_dword(16),
        frame_aspect_ratio: read_dword(20),
        frame_width_in_pixels: read_dword(24),
        frame_height_in_lines: read_dword(28),
        nb_field_per_frame,
        field_descs,
    })
}

/// Parse an OpenDML 2.0 `indx` super-index payload into a structured
/// [`SuperIndex`]. Validates the 24-byte preamble + the per-entry
/// table; tolerates excess padding past the declared `nEntriesInUse`
/// (some writers preallocate a fixed-size slot table and back-patch
/// only the used entries). Returns `Error::InvalidData` only when the
/// chunk is truncated below the 24-byte header or the entry table
/// short-reads `nEntriesInUse * 16` bytes.
fn parse_indx(body: &[u8]) -> Result<SuperIndex> {
    if body.len() < 24 {
        return Err(Error::invalid("AVI: indx super-index header truncated"));
    }
    let w_longs_per_entry = u16::from_le_bytes([body[0], body[1]]);
    let b_index_sub_type = body[2];
    let b_index_type = body[3];
    let n_entries_in_use = u32::from_le_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut chunk_id = [0u8; 4];
    chunk_id.copy_from_slice(&body[8..12]);
    let entries_byte_len = n_entries_in_use.saturating_mul(16);
    let need = 24usize.saturating_add(entries_byte_len);
    if body.len() < need {
        return Err(Error::invalid(
            "AVI: indx super-index entry table truncated",
        ));
    }
    if b_index_type != AVI_INDEX_OF_INDEXES {
        // Per spec/06 §6.1 the `indx` chunk in the strl always carries
        // bIndexType = AVI_INDEX_OF_INDEXES. Some encoders are sloppy
        // here, so we tolerate it but won't have working seek.
        return Ok(SuperIndex::default());
    }
    let mut entries = Vec::with_capacity(n_entries_in_use);
    for i in 0..n_entries_in_use {
        let base = 24 + i * 16;
        let qw_offset = u64::from_le_bytes([
            body[base],
            body[base + 1],
            body[base + 2],
            body[base + 3],
            body[base + 4],
            body[base + 5],
            body[base + 6],
            body[base + 7],
        ]);
        let dw_size = u32::from_le_bytes([
            body[base + 8],
            body[base + 9],
            body[base + 10],
            body[base + 11],
        ]);
        let dw_duration = u32::from_le_bytes([
            body[base + 12],
            body[base + 13],
            body[base + 14],
            body[base + 15],
        ]);
        // Skip zero-offset slots — those are unused capacity (the muxer
        // reserves a fixed number of slots and back-patches only the
        // ones it filled).
        if qw_offset == 0 {
            continue;
        }
        entries.push(SuperIndexEntry {
            qw_offset,
            dw_size,
            dw_duration,
        });
    }
    Ok(SuperIndex {
        w_longs_per_entry,
        b_index_sub_type,
        chunk_id,
        entries,
    })
}

/// Parse an `ix##` AVISTDINDEX body. Layout per
/// `aviriff.h::AVISTDINDEX` (preamble = 24 B; the chunk header's
/// `fcc`+`cb` aren't part of the body we receive here):
///
/// ```text
///   WORD      wLongsPerEntry  (= 2 for std-index; entry is 8 B
///                              | = 3 for 2-field; entry is 12 B)
///   BYTE      bIndexSubType   (0 default; 1 for AVI_INDEX_2FIELD)
///   BYTE      bIndexType      (= 0x01 AVI_INDEX_OF_CHUNKS)
///   DWORD     nEntriesInUse
///   DWORD     dwChunkId       (e.g. '00dc')
///   DWORDLONG qwBaseOffset    (typically the file offset of the 'movi' LIST)
///   DWORD     dwReserved3
///   AVISTDINDEX_ENTRY aIndex[]   // 8 B (default) or 12 B (2-field) each
/// ```
///
/// Each `AVISTDINDEX_ENTRY.dwOffset` is added to `qwBaseOffset` to get
/// the file-absolute offset of the chunk's data (i.e. just after its
/// 8-byte header). `dwSize`'s high bit being set marks a non-keyframe.
///
/// Per OpenDML 2.0 §3.0 "AVI Field Index Chunk", when
/// `bIndexSubType == AVI_INDEX_2FIELD` the entry layout extends to
/// `(dwOffset, dwSize, dwOffsetField2)` — each entry now spans 12 B and
/// `wLongsPerEntry == 3`. The decoder surfaces the field-2 offset on
/// [`StdIndexEntry::dw_offset_field2`]; default-subtype entries leave
/// it at zero.
fn parse_ix_chunk(body: &[u8]) -> Option<StdIndex> {
    if body.len() < 24 {
        return None;
    }
    let w_longs_per_entry = u16::from_le_bytes([body[0], body[1]]);
    let b_index_sub_type = body[2];
    let b_index_type = body[3];
    if b_index_type != AVI_INDEX_OF_CHUNKS {
        return None;
    }
    // wLongsPerEntry is 2 (default 8-B entries) or 3 (2-field, 12 B).
    let entry_size = match w_longs_per_entry {
        2 => 8usize,
        3 if b_index_sub_type == AVI_INDEX_SUB_2FIELD => 12usize,
        _ => return None,
    };
    let n_entries_in_use = u32::from_le_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut chunk_id = [0u8; 4];
    chunk_id.copy_from_slice(&body[8..12]);
    let qw_base_offset = u64::from_le_bytes([
        body[12], body[13], body[14], body[15], body[16], body[17], body[18], body[19],
    ]);
    let entries_byte_len = n_entries_in_use.saturating_mul(entry_size);
    let need = 24usize.saturating_add(entries_byte_len);
    if body.len() < need {
        return None;
    }
    let mut entries = Vec::with_capacity(n_entries_in_use);
    for i in 0..n_entries_in_use {
        let base = 24 + i * entry_size;
        let dw_offset =
            u32::from_le_bytes([body[base], body[base + 1], body[base + 2], body[base + 3]]);
        let dw_size_raw = u32::from_le_bytes([
            body[base + 4],
            body[base + 5],
            body[base + 6],
            body[base + 7],
        ]);
        let is_keyframe = (dw_size_raw & AVISTDINDEX_DELTA_BIT) == 0;
        let dw_size = dw_size_raw & !AVISTDINDEX_DELTA_BIT;
        let dw_offset_field2 = if entry_size == 12 {
            u32::from_le_bytes([
                body[base + 8],
                body[base + 9],
                body[base + 10],
                body[base + 11],
            ])
        } else {
            0
        };
        entries.push(StdIndexEntry {
            dw_offset,
            dw_size,
            is_keyframe,
            dw_offset_field2,
        });
    }
    Some(StdIndex {
        chunk_id,
        qw_base_offset,
        b_index_sub_type,
        entries,
    })
}

/// Walk the per-segment `movi` LIST scanning for `ix##` AVISTDINDEX
/// chunks. Used for OpenDML 2.0 random-access seek when no `idx1`
/// table is present (typical for files written by recent ffmpeg /
/// VirtualDub2 with `--max_riff_size` set). Returns the parsed
/// std-index per `ix##` chunk found (each maps back to one stream via
/// the `##` ASCII digits in its FourCC).
fn scan_ix_in_movi<R: ReadSeek + ?Sized>(
    r: &mut R,
    movi_segments: &[(u64, u64)],
) -> Result<Vec<StdIndex>> {
    let mut out: Vec<StdIndex> = Vec::new();
    for &(start, end) in movi_segments {
        r.seek(SeekFrom::Start(start))?;
        while r.stream_position()? + 8 <= end {
            let hdr = match read_chunk_header_lenient(r)? {
                Some(h) => h,
                None => break,
            };
            // `ix##` FourCCs (ASCII digits at bytes 2..4 instead of
            // 0..2 — note the spec's "##ix" → "ix##" reversal for AVI
            // backward compatibility): the two ASCII digits live at
            // hdr.id[2..4] for std-index chunks. Microsoft's
            // aviriff.h-style notation is "ix##"; some files in the
            // wild also flip and emit "##ix" but those are rare.
            let body_end = (r.stream_position()? + hdr.size as u64).min(end);
            if hdr.id[0] == b'i' && hdr.id[1] == b'x' {
                let body = read_body_bounded(r, hdr.size).ok();
                if let Some(b) = body {
                    if let Some(idx) = parse_ix_chunk(&b) {
                        out.push(idx);
                    }
                }
                skip_pad(r, hdr.size)?;
            } else if hdr.id == LIST {
                // Skip the 4-byte form-type and continue scanning the
                // body — `LIST rec ` clusters can contain ix## too.
                let _ = read_form_type(r)?;
                continue;
            } else {
                // Skip every other chunk (frames, JUNK, …).
                let _ = body_end; // documentation aid
                skip_chunk(r, &hdr)?;
            }
        }
    }
    Ok(out)
}

/// Build a StreamInfo from strh + strf payloads.
///
/// Codec identification flows through `codecs.resolve_tag()`: a codec
/// crate claims the AVI FourCC (for video streams) or the WAVEFORMATEX
/// `wFormatTag` (for audio) via the shared registry, which gives the
/// codec's own crate ownership of the mapping and lets it attach a
/// probe function for tag-collision cases (e.g. `DIV3` that's actually
/// MPEG-4 Part 2). When the registry returns nothing the demuxer
/// surfaces a synthetic `avi:<fourcc>` (or `avi:tag_<hex>`) codec_id;
/// downstream decoder lookup will then fail with a clean error, which
/// is the right signal for "this codec crate hasn't been wired in".
/// Audio-stream sample-size invariant info captured at parse time
/// (round-14 candidate 2). For each audio stream, the muxer's strh
/// `dwSampleSize` is supposed to be `0` for VBR codecs (MP3 / AAC /
/// MPEG) and `> 0` for CBR codecs (PCM / G.711 / IMA-ADPCM); a
/// mismatch means the file lies about its own carriage and downstream
/// `strh.dwLength` derivations will be wrong. The validator at
/// `open()` time uses these per-audio captures to surface
/// [`Error::Validation`] (or skip it with `open_lenient`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct AudioStrhInfo {
    pub format_tag: u16,
    pub sample_size: u32,
}

fn build_stream(
    index: u32,
    strh: &[u8],
    strf: &[u8],
    codecs: &dyn CodecResolver,
) -> Result<(StreamInfo, [u8; 2], Option<AudioStrhInfo>)> {
    // AVISTREAMHEADER layout (56 bytes):
    //   0  fccType       [4]
    //   4  fccHandler    [4]
    //   8  dwFlags       u32
    //  12  wPriority     u16
    //  14  wLanguage     u16
    //  16  dwInitialFrames u32
    //  20  dwScale       u32
    //  24  dwRate        u32  (rate/scale = samples/sec)
    //  28  dwStart       u32
    //  32  dwLength      u32
    //  36  dwSuggestedBufferSize u32
    //  40  dwQuality     u32
    //  44  dwSampleSize  u32
    //  48  rcFrame       [4 * i16]
    if strh.len() < 48 {
        return Err(Error::invalid("AVI: strh too short"));
    }
    let mut fcc_type = [0u8; 4];
    fcc_type.copy_from_slice(&strh[0..4]);
    let mut fcc_handler = [0u8; 4];
    fcc_handler.copy_from_slice(&strh[4..8]);
    let scale = u32::from_le_bytes([strh[20], strh[21], strh[22], strh[23]]).max(1);
    let rate = u32::from_le_bytes([strh[24], strh[25], strh[26], strh[27]]).max(1);
    let length = u32::from_le_bytes([strh[32], strh[33], strh[34], strh[35]]);
    let sample_size = u32::from_le_bytes([strh[44], strh[45], strh[46], strh[47]]);

    let mut audio_info: Option<AudioStrhInfo> = None;
    let (media_type, codec_id, params, suffix) = match &fcc_type {
        b"vids" => {
            let bmih = if !strf.is_empty() {
                Some(parse_bitmap_info_header(strf)?)
            } else {
                None
            };
            let compression = bmih.as_ref().map(|b| b.compression).unwrap_or(fcc_handler);
            let tag = CodecTag::fourcc(&compression);
            let mut ctx = ProbeContext::new(&tag).header(strf);
            if let Some(b) = &bmih {
                ctx = ctx.width(b.width).height(b.height);
            }
            let codec_id = codecs
                .resolve_tag(&ctx)
                .unwrap_or_else(|| video_codec_id_fallback(&compression));
            let mut p = CodecParameters::video(codec_id.clone());
            // Stamp the on-wire FourCC straight onto the params so a
            // muxer re-emitting this stream round-trips byte-for-byte
            // (no walking the registry's first-declared tag).
            p.tag = Some(CodecTag::fourcc(&compression));
            if let Some(b) = &bmih {
                p.width = Some(b.width);
                p.height = Some(b.height);
                p.extradata = b.extradata.clone();
            }
            // Frame rate from scale/rate (rate/scale = fps).
            p.frame_rate = Some(Rational::new(rate as i64, scale as i64));
            // MJPEG packets from AVI should be flagged as standalone JPEGs.
            let suffix = if codec_id.as_str() == "rgb24" {
                *b"db"
            } else {
                *b"dc"
            };
            (MediaType::Video, codec_id, p, suffix)
        }
        b"auds" => {
            let wfx = if !strf.is_empty() {
                Some(parse_waveformatex(strf)?)
            } else {
                None
            };
            let format_tag = wfx.as_ref().map(|w| w.format_tag).unwrap_or(0);
            let bits = wfx.as_ref().map(|w| w.bits_per_sample).unwrap_or(0);
            let tag = CodecTag::wave_format(format_tag);
            let mut ctx = ProbeContext::new(&tag).header(strf);
            if let Some(w) = &wfx {
                ctx = ctx
                    .bits(w.bits_per_sample)
                    .channels(w.channels)
                    .sample_rate(w.samples_per_sec);
            }
            let codec_id = codecs
                .resolve_tag(&ctx)
                .unwrap_or_else(|| audio_codec_id_fallback(format_tag, bits));
            let mut p = CodecParameters::audio(codec_id.clone());
            // Stamp the on-wire wFormatTag onto the params for
            // round-trip preservation.
            p.tag = Some(CodecTag::wave_format(format_tag));
            if let Some(w) = &wfx {
                p.channels = Some(w.channels);
                p.sample_rate = Some(w.samples_per_sec);
                p.extradata = w.extradata.clone();
                p.sample_format = sample_format_for(codec_id.as_str(), w.bits_per_sample);
                p.bit_rate = if w.avg_bytes_per_sec > 0 {
                    Some(w.avg_bytes_per_sec as u64 * 8)
                } else {
                    None
                };
            }
            // Capture (format_tag, sample_size) for the round-14 C2
            // VBR/CBR validator: VBR codecs require dwSampleSize == 0
            // (one packet = one variable-length frame); CBR codecs
            // require dwSampleSize > 0 (fixed bytes-per-sample). The
            // validator runs in `open_avi` (or is skipped by
            // `open_avi_lenient`).
            audio_info = Some(AudioStrhInfo {
                format_tag,
                sample_size,
            });
            (MediaType::Audio, codec_id, p, *b"wb")
        }
        _ => {
            // "txts", "mids", "dats" — represent as data.
            let codec_id = CodecId::new(format!(
                "avi:{}",
                std::str::from_utf8(&fcc_type).unwrap_or("????")
            ));
            let mut p = CodecParameters::audio(codec_id.clone());
            p.media_type = MediaType::Data;
            (MediaType::Data, codec_id, p, *b"xx")
        }
    };

    let _ = codec_id; // absorbed into params

    // Stream time base. For video: scale/rate seconds per frame. For audio
    // at rate/scale samples per second, pick 1/samples_per_sec (standard
    // choice). For anything else, fall back to 1/rate.
    let time_base = match media_type {
        MediaType::Video => TimeBase::new(scale as i64, rate as i64),
        MediaType::Audio => {
            // rate/scale = samples_per_sec for PCM.
            TimeBase::new(scale as i64, rate as i64)
        }
        _ => TimeBase::new(scale as i64, rate as i64),
    };

    let duration = if length > 0 {
        Some(length as i64)
    } else {
        None
    };
    let stream = StreamInfo {
        index,
        time_base,
        duration,
        start_time: Some(0),
        params,
    };
    Ok((stream, suffix, audio_info))
}

/// Synthesise a placeholder `avi:<fourcc>` codec_id when the resolver
/// has no claim on the FourCC. Downstream `make_decoder` will return
/// `CodecNotFound` for these; the prefix lets callers tell "the codec
/// crate isn't wired in" apart from "the codec id is genuinely unknown".
fn video_codec_id_fallback(fourcc: &[u8; 4]) -> CodecId {
    if fourcc == &[0, 0, 0, 0] {
        // BI_RGB sentinel. There's no meaningful FourCC string to print
        // and `rgb24` is the conventional codec_id we'd ascribe; emit it
        // here as the one historical exception so unregistered builds
        // still surface uncompressed AVI as `rgb24`.
        return CodecId::new("rgb24");
    }
    let printable = fourcc.iter().all(|b| b.is_ascii_graphic() || *b == b' ');
    if printable {
        let s = std::str::from_utf8(fourcc).unwrap_or("????");
        CodecId::new(format!("avi:{s}"))
    } else {
        CodecId::new(format!(
            "avi:0x{:02X}{:02X}{:02X}{:02X}",
            fourcc[0], fourcc[1], fourcc[2], fourcc[3]
        ))
    }
}

/// Synthesise a placeholder `avi:tag_<hex>` codec_id (or one of the PCM
/// pseudo-claims for the integer / float WAVE_FORMAT_PCM tags) when the
/// resolver has no claim on the wFormatTag. This is the only place the
/// AVI demuxer hard-codes audio codec mappings; codec crates that want
/// proper resolution for their wFormatTag should claim it via
/// `CodecInfo::tags(...)`.
fn audio_codec_id_fallback(format_tag: u16, bits: u16) -> CodecId {
    let name = match format_tag {
        // WAVE_FORMAT_PCM — pick the integer flavour by bit depth.
        // Even when no `pcm_*` codec is registered, surfacing the
        // depth-aware id keeps downstream demux+inspect tools useful
        // for raw-PCM AVIs.
        0x0001 => match bits {
            8 => "pcm_u8",
            24 => "pcm_s24le",
            32 => "pcm_s32le",
            _ => "pcm_s16le",
        },
        0x0003 => match bits {
            64 => "pcm_f64le",
            _ => "pcm_f32le",
        },
        _ => return CodecId::new(format!("avi:tag_{format_tag:04x}")),
    };
    CodecId::new(name)
}

/// Map a decoded audio codec + WAVEFORMATEX `bits_per_sample` to the
/// corresponding `SampleFormat`. Used only to hint downstream consumers;
/// packet bytes are passed through verbatim regardless of this result.
fn sample_format_for(codec: &str, bits: u16) -> Option<SampleFormat> {
    match codec {
        "pcm_u8" => Some(SampleFormat::U8),
        "pcm_s16le" | "pcm_s16be" => Some(SampleFormat::S16),
        "pcm_s24le" => Some(SampleFormat::S24),
        "pcm_s32le" => Some(SampleFormat::S32),
        "pcm_f32le" => Some(SampleFormat::F32),
        "pcm_f64le" => Some(SampleFormat::F64),
        // μ-law / A-law expand to S16 once decoded.
        "pcm_mulaw" | "pcm_alaw" => Some(SampleFormat::S16),
        _ => match bits {
            8 => Some(SampleFormat::U8),
            16 => Some(SampleFormat::S16),
            24 => Some(SampleFormat::S24),
            32 => Some(SampleFormat::S32),
            _ => None,
        },
    }
}

fn read_body_bounded<R: std::io::Read + ?Sized>(r: &mut R, size: u32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Probe the underlying stream's total length by seeking to end and
/// restoring the original position. Used for truncated-head clamping
/// of declared `RIFF` / `LIST` sizes (see module-level
/// "Truncated-head tolerance" doc).
fn probe_file_len<R: ReadSeek + ?Sized>(r: &mut R) -> Result<u64> {
    let cur = r.stream_position()?;
    let end = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(cur))?;
    Ok(end)
}

/// True if `e` is an `Error::Io` wrapping a `std::io::ErrorKind::UnexpectedEof`.
/// Used to translate truncated-tail body reads into a clean `Error::Eof`.
fn is_unexpected_eof(e: &Error) -> bool {
    matches!(e, Error::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof)
}

/// Parse a raw `idx1` body, decide whether the recorded offsets are
/// file-absolute or `movi`-relative (both are seen in the wild), and
/// populate each entry with a synthesised per-stream pts.
///
/// Offset-base detection: AVI 1.0 is ambiguous about the reference point
/// for idx1 offsets. Some muxers (MS reference, ffmpeg) emit offsets
/// relative to the `movi` FourCC; others emit file-absolute offsets. We
/// probe the first plausible entry by reading the 8-byte chunk header at
/// `file_start + offset` and `movi_start - 4 + offset` (the "- 4" puts us
/// at the `movi` FourCC byte) and picking whichever yields the matching
/// `ckid`. Default to movi-relative if the file is too small to probe.
/// Scan raw idx1 bytes for chunks whose ckid ends with the given
/// 2-byte suffix and bump per-stream counts (round-8 C3 / round-10 C1).
///
/// Each idx1 entry is a 16-byte `AVIINDEXENTRY`: ckid(4) + flags(4) +
/// offset(4) + size(4). We treat any ckid whose final two bytes match
/// `suffix` (e.g. `b"pc"` per `aviriff.h`'s `cktypePALchange = "PC"`,
/// or `b"tx"` for text/subtitle chunks per `mmsystem.h`'s text-stream
/// FourCC family) as belonging to that family and bump the per-stream
/// count. The first two bytes of ckid are ASCII digits encoding the
/// stream index.
///
/// Counts beyond `u32::MAX - 1` saturate; that's a single-frame
/// chunk-soup file with ~4G of one suffix per stream, well outside
/// any real-world capture pattern.
fn scan_idx1_for_suffix(raw: &[u8], streams: &[StreamInfo], suffix: [u8; 2], counts: &mut [u32]) {
    if raw.len() < 16 || counts.len() < streams.len() {
        return;
    }
    let n = raw.len() / 16;
    for i in 0..n {
        let base = i * 16;
        let ckid = [raw[base], raw[base + 1], raw[base + 2], raw[base + 3]];
        if ckid[2] != suffix[0] || ckid[3] != suffix[1] {
            continue;
        }
        if let Some(stream) = parse_stream_index(&ckid) {
            let s = stream as usize;
            if s < counts.len() && s < streams.len() {
                counts[s] = counts[s].saturating_add(1);
            }
        }
    }
}

/// Round-12 candidate 1: walk `idx1` for entries whose `ckid` ends in
/// `suffix` (`b"pc"` for palette change, `b"tx"` for text/subtitle),
/// resolve each entry's offset to a file-absolute chunk header, and
/// read the chunk body into the matching per-stream Vec. Mirrors the
/// offset-resolution rules in [`build_idx_table`] (idx1 offsets may be
/// `movi`-relative or file-absolute; we use the same `movi_relative`
/// flag the seek-table builder probed).
///
/// Returns silently on malformed input (truncated raw, short header,
/// over-declared body sizes) — side-band data is best-effort and a
/// missing chunk body just leaves the corresponding slot empty so
/// downstream `palette_change_data(s)[k]` still indexes correctly with
/// the per-stream count.
fn read_sideband_data_from_idx1<R: ReadSeek + ?Sized>(
    r: &mut R,
    raw: &[u8],
    movi_start: u64,
    streams: &[StreamInfo],
    suffix: [u8; 2],
    out: &mut [Vec<Vec<u8>>],
) {
    if raw.len() < 16 || out.len() < streams.len() {
        return;
    }
    // Same offset-base probe as `build_idx_table` so the two stay in sync.
    let n = raw.len() / 16;
    let movi_fourcc_pos = movi_start.saturating_sub(4);
    let mut probe_raw_offset: Option<u32> = None;
    let mut probe_ckid: Option<[u8; 4]> = None;
    for i in 0..n {
        let base = i * 16;
        let off =
            u32::from_le_bytes([raw[base + 8], raw[base + 9], raw[base + 10], raw[base + 11]]);
        if off != 0 {
            let mut ckid = [0u8; 4];
            ckid.copy_from_slice(&raw[base..base + 4]);
            probe_raw_offset = Some(off);
            probe_ckid = Some(ckid);
            break;
        }
    }
    let mut movi_relative = true;
    if let (Some(raw_off), Some(ckid)) = (probe_raw_offset, probe_ckid) {
        let try_movi = movi_fourcc_pos.checked_add(raw_off as u64);
        let movi_ok = match try_movi {
            Some(p) => probe_offset_has_ckid(r, p, &ckid).unwrap_or(false),
            None => false,
        };
        let abs_ok = probe_offset_has_ckid(r, raw_off as u64, &ckid).unwrap_or(false);
        movi_relative = match (movi_ok, abs_ok) {
            (true, false) => true,
            (false, true) => false,
            _ => true,
        };
    }
    let base_off = if movi_relative { movi_fourcc_pos } else { 0 };
    for i in 0..n {
        let base = i * 16;
        let ckid = [raw[base], raw[base + 1], raw[base + 2], raw[base + 3]];
        if ckid[2] != suffix[0] || ckid[3] != suffix[1] {
            continue;
        }
        let stream = match parse_stream_index(&ckid) {
            Some(s) => s,
            None => continue,
        };
        let s = stream as usize;
        if s >= out.len() || s >= streams.len() {
            continue;
        }
        let raw_off =
            u32::from_le_bytes([raw[base + 8], raw[base + 9], raw[base + 10], raw[base + 11]]);
        let size = u32::from_le_bytes([
            raw[base + 12],
            raw[base + 13],
            raw[base + 14],
            raw[base + 15],
        ]);
        let chunk_off = base_off.saturating_add(raw_off as u64);
        // `chunk_off` points at the 4-byte ckid; body starts 8 bytes in.
        // Read the body bytes directly. Failure leaves the chunk slot
        // out (best-effort).
        if r.seek(SeekFrom::Start(chunk_off + 8)).is_err() {
            continue;
        }
        match read_body_bounded(r, size) {
            Ok(body) => out[s].push(body),
            Err(_) => continue,
        }
    }
}

fn build_idx_table<R: ReadSeek + ?Sized>(
    r: &mut R,
    raw: &[u8],
    movi_start: u64,
    streams: &[StreamInfo],
) -> Result<Vec<IdxEntry>> {
    if raw.len() < 16 {
        return Ok(Vec::new());
    }
    let n = raw.len() / 16;
    // Pick the first entry with a non-zero offset as a probe.
    let mut probe_raw_offset: Option<u32> = None;
    let mut probe_ckid: Option<[u8; 4]> = None;
    for i in 0..n {
        let base = i * 16;
        let off =
            u32::from_le_bytes([raw[base + 8], raw[base + 9], raw[base + 10], raw[base + 11]]);
        if off != 0 {
            let mut ckid = [0u8; 4];
            ckid.copy_from_slice(&raw[base..base + 4]);
            probe_raw_offset = Some(off);
            probe_ckid = Some(ckid);
            break;
        }
    }

    // `movi_start` points at the first chunk header inside movi (i.e. 4
    // bytes *after* the `movi` FourCC). idx1 offsets relative to the
    // `movi` FourCC therefore need an adjustment of `movi_start - 4`.
    let movi_fourcc_pos = movi_start.saturating_sub(4);
    let mut movi_relative = true; // conservative default: most files.
    if let (Some(raw_off), Some(ckid)) = (probe_raw_offset, probe_ckid) {
        let try_movi = movi_fourcc_pos.checked_add(raw_off as u64);
        let try_abs = Some(raw_off as u64);
        let movi_ok = match try_movi {
            Some(p) => probe_offset_has_ckid(r, p, &ckid).unwrap_or(false),
            None => false,
        };
        let abs_ok = match try_abs {
            Some(p) => probe_offset_has_ckid(r, p, &ckid).unwrap_or(false),
            None => false,
        };
        movi_relative = match (movi_ok, abs_ok) {
            (true, false) => true,
            (false, true) => false,
            // If both or neither match, stick with movi-relative (the
            // more common convention). A broken index is tolerable — it
            // just means seek_to lands on wrong data and the player
            // discovers it on next read.
            _ => true,
        };
    }
    let base_off = if movi_relative { movi_fourcc_pos } else { 0 };

    // First pass: build entries with file-absolute offsets. Drop entries
    // for unknown stream indexes (tolerate stray junk).
    let mut entries: Vec<IdxEntry> = Vec::with_capacity(n);
    for i in 0..n {
        let base = i * 16;
        let mut ckid = [0u8; 4];
        ckid.copy_from_slice(&raw[base..base + 4]);
        let flags =
            u32::from_le_bytes([raw[base + 4], raw[base + 5], raw[base + 6], raw[base + 7]]);
        let raw_off =
            u32::from_le_bytes([raw[base + 8], raw[base + 9], raw[base + 10], raw[base + 11]]);
        let size = u32::from_le_bytes([
            raw[base + 12],
            raw[base + 13],
            raw[base + 14],
            raw[base + 15],
        ]);
        let stream = match parse_stream_index(&ckid) {
            Some(s) => s,
            None => continue,
        };
        if (stream as usize) >= streams.len() {
            continue;
        }
        let abs = base_off.saturating_add(raw_off as u64);
        entries.push(IdxEntry {
            stream,
            flags,
            offset: abs,
            size,
            pts: 0,
        });
    }

    // Second pass: assign per-stream pts by walking each stream's entries
    // in idx1 order, mirroring the pts-bump logic in `next_packet`.
    let mut per_stream_pts: Vec<i64> = vec![0; streams.len()];
    for e in entries.iter_mut() {
        let s = e.stream as usize;
        e.pts = per_stream_pts[s];
        let bump = packet_time_delta(&streams[s], e.size as usize) as i64;
        per_stream_pts[s] = per_stream_pts[s].saturating_add(bump);
    }

    Ok(entries)
}

/// Read the 4-byte ckid at `offset` (no seek restore) and check whether
/// it matches `expected`. Returns `Ok(false)` on short read rather than
/// propagating EOF, so the caller can probe both offset bases safely.
fn probe_offset_has_ckid<R: ReadSeek + ?Sized>(
    r: &mut R,
    offset: u64,
    expected: &[u8; 4],
) -> Result<bool> {
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut buf[got..]) {
            Ok(0) => return Ok(false),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(false),
        }
    }
    Ok(&buf == expected)
}

// --- Demuxer runtime ------------------------------------------------------

/// Concrete AVI demuxer. Returned by [`open_avi`] for callers that
/// need direct access to AVI-specific accessors like
/// [`AviDemuxer::field2_offset_for_packet`] (round-5 candidate 1).
/// Implements [`oxideav_core::Demuxer`] for the usual streams /
/// next_packet / seek_to / metadata / duration_micros entry points.
pub struct AviDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    /// For each stream, the expected 2-byte chunk-name suffix in `movi`.
    packet_chunk_suffix: Vec<[u8; 2]>,
    /// Absolute start-of-first-movi offset. Retained so `seek_to` can bound
    /// against the beginning of packet data and build_idx_table has an
    /// offset base.
    movi_start: u64,
    /// All movi segments: `(start, end)` pairs. There is always at least
    /// one; OpenDML `RIFF AVIX` extension RIFFs contribute additional
    /// segments.
    movi_segments: Vec<(u64, u64)>,
    /// Index into `movi_segments` of the segment `next_packet` is
    /// currently walking.
    current_segment: usize,
    /// Running packet counter per stream — used to synthesise PTS.
    per_stream_counter: Vec<u64>,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
    /// Optional idx1-derived seek table (empty = not available).
    idx_table: Vec<IdxEntry>,
    /// Optional OpenDML 2.0 super-index per stream (parallel to `streams`,
    /// indexed by stream number). Empty `SuperIndex` for streams that
    /// didn't declare an `indx` chunk in their strl. Used as a probe
    /// signal for the std-index scan; the actual seek table is built
    /// from [`AviDemuxer::std_indexes`] (the `ix##` chunks the
    /// super-index points at).
    #[allow(dead_code)]
    super_indexes: Vec<SuperIndex>,
    /// Standard `ix##` index chunks (one per (stream, segment) pair) if
    /// present. Combined with `super_indexes` to drive seek for
    /// OpenDML files with no `idx1`.
    std_indexes: Vec<StdIndex>,
    /// Per-stream idx1-flags lookup table (round-8 candidate 1).
    ///
    /// Built once at `open()` from `idx_table`: outer index is
    /// `stream_index`, inner index is the per-stream `packet_seq`
    /// (zero-based file-order ordinal). Replaces the prior
    /// O(N)-per-call linear scan in
    /// [`AviDemuxer::idx1_flags_for_packet`] with an O(1) lookup so
    /// callers walking every packet (e.g. extracting a per-frame
    /// keyframe map) don't pay quadratic time. Empty when no `idx1`
    /// was parsed.
    idx1_flags_per_stream: Vec<Vec<u32>>,
    /// Per-stream `xxpc` palette-change packet count (round-8 candidate 3).
    ///
    /// VfW palette-change chunks (`NNpc` per `aviriff.h` —
    /// `cktypePALchange = "PC"`) carry `BITMAPINFO`-style palette
    /// updates that retroactively rewrite the indexed-colour palette
    /// for subsequent video chunks. They're separate from regular
    /// video data chunks (`NNdc`/`NNdb`), so the demuxer skips them
    /// from the packet stream but counts them per stream and surfaces
    /// the count via `avi:palette_change.<stream>` metadata so
    /// downstream consumers can detect that the file carries
    /// palette animation. Empty Vec means no `xxpc` was seen.
    palette_change_counts: Vec<u32>,
    /// Per-stream `xxtx` text/subtitle chunk count (round-10
    /// candidate 1). Mirror of [`Self::palette_change_counts`] for
    /// the text-stream FourCC family per `mmsystem.h` —
    /// `ckidAVITextSF`. Like palette-change chunks, text chunks are
    /// not video data and are excluded from the regular packet
    /// stream; the count surfaces via `avi:text_chunk.<stream>`
    /// metadata and the [`AviDemuxer::text_chunk_count`] accessor.
    text_chunk_counts: Vec<u32>,
    /// Raw `dwFlags` from `AVIMAINHEADER` (round-10 candidate 3).
    /// Retained on the demuxer struct so [`AviDemuxer::avih_flags`]
    /// can return a typed [`AvihFlags`] decode without re-parsing
    /// the metadata Vec's hex string. Zero when no `avih` chunk was
    /// seen — typed-flag accessors then return their `false` /
    /// "no flag set" defaults.
    avih_flags: u32,
    /// Raw `dwSuggestedBufferSize` from `AVIMAINHEADER` (round-13
    /// candidate 2). Mirror of [`Self::avih_flags`] for the
    /// per-AVI 1.0 §3.1 read-ahead allocation hint. Populated from
    /// the parsed `avih.dwSuggestedBufferSize` DWORD (same data also
    /// surfaces under the `avi:suggested_buffer_size` metadata key);
    /// zero when the file had no parsable `avih`.
    avih_suggested_buffer_size: u32,
    /// Per-stream parsed `vprp` Video Properties Header (round-9
    /// candidate 1). Indexed by stream number; default-initialised
    /// for streams that didn't carry a `vprp` chunk. Retained on the
    /// demuxer struct so [`AviDemuxer::vprp_field_descs`] can hand
    /// out `&[VprpFieldDesc]` slices without re-parsing.
    vprps: Vec<VprpHeader>,
    /// OpenDML 2.0 §5.0 `dmlh.dwTotalFrames` (round-9 candidate 3).
    /// `None` when no `LIST odml dmlh` was seen. Surfaced via the
    /// typed [`AviDemuxer::dmlh_total_frames`] accessor in addition
    /// to the existing `avi:total_frames_all_segments` metadata key.
    dmlh_total_frames: Option<u32>,
    /// Per-stream buffered `xxpc` palette-change chunk bodies in file
    /// order (round-12 candidate 1). Each inner Vec is the raw chunk
    /// payload — typically an AVI 1.0 `BITMAPINFO`-style palette delta:
    /// 1-byte `bFirstEntry`, 1-byte `bNumEntries`, 2-byte `wFlags`,
    /// then `bNumEntries * 4`-byte palette quads. Surfaced via
    /// [`AviDemuxer::palette_change_data`] so muxer→demuxer round-trips
    /// can compare bytes directly with what
    /// [`crate::muxer::AviMuxer::write_palette_change`] emitted.
    /// Populated eagerly from `idx1` at `open()`; for `idx1`-less
    /// (OpenDML-only) files the lazy `next_packet` walk appends as it
    /// sees each chunk so callers iterating packets get the data
    /// progressively. Empty when no `xxpc` chunks were seen for that
    /// stream.
    palette_change_data: Vec<Vec<Vec<u8>>>,
    /// Per-stream buffered `xxtx` text/subtitle chunk bodies in file
    /// order (round-12 candidate 1). Mirror of
    /// [`Self::palette_change_data`] for the text-stream FourCC family.
    /// Each inner Vec is the chunk payload verbatim — typically a
    /// caption / subtitle / cuepoint string written by
    /// [`crate::muxer::AviMuxer::write_text_chunk`].
    text_chunk_data: Vec<Vec<Vec<u8>>>,
    /// `true` once the side-band data buffers have been populated from
    /// `idx1` at `open()` time. The lazy `next_packet` skip-and-buffer
    /// path checks this flag to avoid double-appending the same chunks
    /// once the eager path already cached them. `false` for `idx1`-less
    /// (OpenDML-only) files where `next_packet` is the only producer.
    sideband_data_loaded: bool,
}

/// Result of [`AviDemuxer::seek_to_keyframe_strict`] (round-9
/// candidate 4).
///
/// Captures the originally-requested PTS, the keyframe PTS the
/// demuxer actually landed on (always at-or-before target), and the
/// gap between them in stream ticks. A `gop_distance` of 0 means the
/// requested PTS *is* a keyframe; a non-zero distance means the
/// caller must decode-and-discard `gop_distance` ticks worth of
/// frames before reaching the wanted PTS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyframeSeekResult {
    /// The PTS the caller asked for.
    pub target_pts: i64,
    /// The PTS of the keyframe the demuxer actually landed on. Always
    /// at-or-before `target_pts` (or the first keyframe in the file,
    /// if the request fell before that).
    pub landed_pts: i64,
    /// `target_pts - landed_pts`, clamped to `>= 0`. The number of
    /// stream ticks worth of frames a caller must walk past after the
    /// seek to reach the originally-requested PTS.
    pub gop_distance: i64,
}

/// `AVIF_HASINDEX` per Microsoft's `vfw.h` — the file has an `idx1`.
pub const AVIF_HASINDEX: u32 = 0x0000_0010;
/// `AVIF_MUSTUSEINDEX` per `vfw.h` — players must use the index to
/// determine the order of the presentation, not the order of chunks
/// in `movi`.
pub const AVIF_MUSTUSEINDEX: u32 = 0x0000_0020;
/// `AVIF_ISINTERLEAVED` per `vfw.h` — file is interleaved (audio +
/// video chunks alternate within `movi`).
pub const AVIF_ISINTERLEAVED: u32 = 0x0000_0100;
/// `AVIF_TRUSTCKTYPE` per `vfw.h` — players can trust the keyframe
/// flag in the per-chunk index entries.
pub const AVIF_TRUSTCKTYPE: u32 = 0x0000_0800;
/// `AVIF_WASCAPTUREFILE` per `vfw.h` — file was created by a capture
/// application (specially allocated for streaming-capture write).
pub const AVIF_WASCAPTUREFILE: u32 = 0x0001_0000;
/// `AVIF_COPYRIGHTED` per `vfw.h` — file contains copyrighted data.
pub const AVIF_COPYRIGHTED: u32 = 0x0002_0000;

// --- WAVEFORMATEX format-tag constants (mmreg.h) used by the round-14
// candidate 2 audio sample-size VBR/CBR validator. -------------------

/// `WAVE_FORMAT_PCM` per Microsoft's `mmreg.h` — uncompressed integer
/// PCM. CBR: requires `strh.dwSampleSize > 0`.
pub const WAVE_FORMAT_PCM: u16 = 0x0001;
/// `WAVE_FORMAT_ALAW` per `mmreg.h` — G.711 a-law companded PCM. CBR:
/// requires `strh.dwSampleSize > 0`.
pub const WAVE_FORMAT_ALAW: u16 = 0x0006;
/// `WAVE_FORMAT_MULAW` per `mmreg.h` — G.711 µ-law companded PCM. CBR:
/// requires `strh.dwSampleSize > 0`.
pub const WAVE_FORMAT_MULAW: u16 = 0x0007;
/// `WAVE_FORMAT_DVI_ADPCM` per `mmreg.h` (a.k.a. IMA ADPCM). CBR:
/// requires `strh.dwSampleSize > 0`.
pub const WAVE_FORMAT_DVI_ADPCM: u16 = 0x0011;
/// `WAVE_FORMAT_MPEG` per `mmreg.h` — MPEG-1 Audio Layer I/II/III
/// generic. VBR: requires `strh.dwSampleSize == 0`.
pub const WAVE_FORMAT_MPEG: u16 = 0x0050;
/// `WAVE_FORMAT_MPEGLAYER3` per `mmreg.h` — MP3. VBR: requires
/// `strh.dwSampleSize == 0`.
pub const WAVE_FORMAT_MPEGLAYER3: u16 = 0x0055;
/// `WAVE_FORMAT_AAC` per `mmreg.h` (Microsoft's AAC tag). VBR:
/// requires `strh.dwSampleSize == 0`.
pub const WAVE_FORMAT_AAC: u16 = 0x00FF;

/// Round-14 candidate 2: classify a WAVEFORMATEX `wFormatTag` per
/// the AVI 1.0 sample-size invariant.
///
/// - `Some(true)` ⇒ VBR codec (one packet = one variable-length
///   frame); `strh.dwSampleSize` MUST be 0.
/// - `Some(false)` ⇒ CBR codec (fixed bytes per sample);
///   `strh.dwSampleSize` MUST be > 0.
/// - `None` ⇒ no constraint (codec the spec doesn't pin one way or
///   the other — e.g. WMA, AC-3, custom registrations).
fn classify_audio_sample_size(format_tag: u16) -> Option<bool> {
    match format_tag {
        WAVE_FORMAT_MPEG | WAVE_FORMAT_MPEGLAYER3 | WAVE_FORMAT_AAC => Some(true),
        WAVE_FORMAT_PCM | WAVE_FORMAT_ALAW | WAVE_FORMAT_MULAW | WAVE_FORMAT_DVI_ADPCM => {
            Some(false)
        }
        _ => None,
    }
}

/// Round-14 candidate 2: return `Some(message)` when
/// `(format_tag, sample_size)` violates the AVI 1.0 VBR/CBR invariant
/// (see [`classify_audio_sample_size`]); `None` when it passes (or the
/// format tag isn't constrained).
fn audio_strh_violation(info: &AudioStrhInfo) -> Option<String> {
    let vbr = classify_audio_sample_size(info.format_tag)?;
    if vbr {
        if info.sample_size != 0 {
            return Some(format!(
                "VBR codec requires strh.dwSampleSize == 0, got {}",
                info.sample_size
            ));
        }
    } else if info.sample_size == 0 {
        return Some("CBR codec requires strh.dwSampleSize > 0, got 0".to_string());
    }
    None
}

/// Typed decode of `AVIMAINHEADER.dwFlags` (round-10 candidate 3).
///
/// Each documented `AVIF_*` bit per Microsoft's `vfw.h` (see this
/// crate's `AVIF_HASINDEX` / `AVIF_MUSTUSEINDEX` / `AVIF_ISINTERLEAVED`
/// / `AVIF_TRUSTCKTYPE` / `AVIF_WASCAPTUREFILE` / `AVIF_COPYRIGHTED`
/// constants) decodes to its own `bool`. The raw `bits` field carries
/// the original DWORD so callers wanting to inspect undocumented or
/// vendor-extension bits don't lose information.
///
/// Returned by [`AviDemuxer::avih_flags`]; same source as the
/// `avi:flags` hex-string metadata key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AvihFlags {
    /// File has an `idx1` chunk. Set to true by every conformant
    /// AVI 1.0 writer that emits an idx1; absent in OpenDML-only
    /// files that only carry `ix##` standard indexes.
    pub has_index: bool,
    /// Index *must* drive playback order (chunks in `movi` aren't
    /// guaranteed to be in presentation order). Rare; usually paired
    /// with `has_index`.
    pub must_use_index: bool,
    /// Streams are interleaved. The conventional flag any writer
    /// targeting general-purpose AVI players should set.
    pub is_interleaved: bool,
    /// Keyframe bits in idx1 entries can be trusted (no decoder-side
    /// re-derivation needed).
    pub trust_ck_type: bool,
    /// File was specially allocated for capture; some players
    /// optimise read-ahead based on this hint.
    pub was_capture_file: bool,
    /// File is marked copyrighted.
    pub copyrighted: bool,
    /// Raw `dwFlags` DWORD as parsed from `avih`. Non-zero bits
    /// outside the documented set are vendor-extension / future-spec
    /// bits and are exposed verbatim.
    pub bits: u32,
}

impl AvihFlags {
    /// Decode a raw `dwFlags` u32 into a structured [`AvihFlags`].
    pub fn from_bits(bits: u32) -> Self {
        Self {
            has_index: bits & AVIF_HASINDEX != 0,
            must_use_index: bits & AVIF_MUSTUSEINDEX != 0,
            is_interleaved: bits & AVIF_ISINTERLEAVED != 0,
            trust_ck_type: bits & AVIF_TRUSTCKTYPE != 0,
            was_capture_file: bits & AVIF_WASCAPTUREFILE != 0,
            copyrighted: bits & AVIF_COPYRIGHTED != 0,
            bits,
        }
    }
}

/// One palette entry inside a [`PaletteChange`] body — `PALETTEENTRY`
/// per Microsoft's `wingdi.h`. Layout matches the on-wire byte order
/// used by AVI 1.0 `xxpc` chunks: `peRed`, `peGreen`, `peBlue`,
/// `peFlags`. The trailing `flags` byte usually carries
/// `PC_RESERVED | PC_EXPLICIT | PC_NOCOLLAPSE` bits per Microsoft's
/// `wingdi.h` palette flags; most files leave it zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaletteEntry {
    /// `peRed`.
    pub red: u8,
    /// `peGreen`.
    pub green: u8,
    /// `peBlue`.
    pub blue: u8,
    /// `peFlags` (palette-entry flag byte; usually zero).
    pub flags: u8,
}

/// Typed decode of an `xxpc` palette-change chunk body (round-13
/// candidate 1).
///
/// Per AVI 1.0 / `vfw.h`'s `PALCHANGE` shape the chunk body is:
/// ```text
/// BYTE  bFirstEntry            // first palette index updated
/// BYTE  bNumEntries            // number of entries (0 → 256)
/// WORD  wFlags                 // reserved (usually zero)
/// PALETTEENTRY entries[bNumEntries]   // 4 bytes each
/// ```
/// Composed by [`AviDemuxer::palette_change_typed`] from the round-12
/// raw [`AviDemuxer::palette_change_data`] accessor; consumed by
/// [`crate::muxer::AviMuxer::with_palette_change_typed`] to write the
/// equivalent chunk back. Closes the typed round-trip pair so callers
/// don't have to hand-pack `BITMAPINFO` palette deltas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaletteChange {
    /// `bFirstEntry` — first palette index this delta updates.
    pub first_entry: u8,
    /// `bNumEntries` as parsed from the wire. The spec's literal-zero
    /// convention ("0 → all 256 entries") is honoured by checking
    /// against the actual `entries` slice length: a trailing array of
    /// 256 quads with `bNumEntries == 0` round-trips intact.
    pub num_entries: u8,
    /// `wFlags`. Most files leave this zero; the spec reserves the
    /// field for future palette-update flag bits.
    pub flags: u16,
    /// Decoded `PALETTEENTRY[]`. Length matches the number of quads
    /// found after the 4-byte header — usually `num_entries`, or `256`
    /// when the body declared `num_entries == 0`. An empty `entries`
    /// vector is allowed (spec doesn't forbid an empty delta).
    pub entries: Vec<PaletteEntry>,
}

impl PaletteChange {
    /// Parse a raw `xxpc` chunk body into the typed shape. Returns
    /// `None` for bodies shorter than the 4-byte fixed header or with
    /// a trailing array length that isn't a multiple of 4 bytes (the
    /// `PALETTEENTRY` size). The trailing-array length determines the
    /// `entries` vector size: callers can detect the spec's
    /// `num_entries == 0 → 256` convention by checking
    /// `entries.len() == 256 && num_entries == 0`.
    pub fn parse(body: &[u8]) -> Option<Self> {
        if body.len() < 4 {
            return None;
        }
        let first_entry = body[0];
        let num_entries = body[1];
        let flags = u16::from_le_bytes([body[2], body[3]]);
        let tail = &body[4..];
        if tail.len() % 4 != 0 {
            return None;
        }
        let mut entries = Vec::with_capacity(tail.len() / 4);
        for chunk in tail.chunks_exact(4) {
            entries.push(PaletteEntry {
                red: chunk[0],
                green: chunk[1],
                blue: chunk[2],
                flags: chunk[3],
            });
        }
        Some(Self {
            first_entry,
            num_entries,
            flags,
            entries,
        })
    }

    /// Encode the typed shape back into a raw `xxpc` chunk body
    /// suitable for [`crate::muxer::AviMuxer::write_palette_change`].
    /// Output layout matches [`Self::parse`]'s expectations exactly:
    /// 1-byte `first_entry`, 1-byte `num_entries`, 2-byte LE `flags`,
    /// then `entries.len() * 4` bytes of `PALETTEENTRY` quads. Output
    /// length is always even (header + 4-aligned tail) so no muxer-side
    /// pad byte is needed.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.entries.len() * 4);
        out.push(self.first_entry);
        out.push(self.num_entries);
        out.extend_from_slice(&self.flags.to_le_bytes());
        for e in &self.entries {
            out.push(e.red);
            out.push(e.green);
            out.push(e.blue);
            out.push(e.flags);
        }
        out
    }
}

/// Lazy iterator returned by [`AviDemuxer::palette_change_typed_iter`]
/// (round-14 candidate 3). Yields one `Result<PaletteChange>` per
/// `xxpc` chunk for the requested stream, decoding the typed shape on
/// demand. See the parent accessor's docs for the iteration contract.
pub struct PaletteChangeTypedIter<'a> {
    bodies: &'a [Vec<u8>],
    next: usize,
}

impl<'a> Iterator for PaletteChangeTypedIter<'a> {
    type Item = Result<PaletteChange>;

    fn next(&mut self) -> Option<Self::Item> {
        let body = self.bodies.get(self.next)?;
        self.next += 1;
        match PaletteChange::parse(body) {
            Some(pc) => Some(Ok(pc)),
            None => Some(Err(Error::invalid(format!(
                "AVI: xxpc body #{} ({} bytes) failed to decode as PaletteChange",
                self.next - 1,
                body.len()
            )))),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.bodies.len().saturating_sub(self.next);
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for PaletteChangeTypedIter<'a> {}

/// Decoded `vprp` (Video Properties Header) per OpenDML 2.0 §5.0.
///
/// The 9 fixed DWORDs at the start of a `vprp` body, plus the
/// trailing `VIDEO_FIELD_DESC FieldInfo[nbFieldPerFrame]` array (one
/// 8-DWORD record per field). Round-9 candidate 1: prior rounds
/// dropped the per-field-rect tail; both are now exposed via
/// `Demuxer::metadata()` under the `avi:vprp.*` namespace and via the
/// typed [`AviDemuxer::vprp_field_descs`] accessor.
#[derive(Clone, Debug, Default)]
struct VprpHeader {
    /// `VideoFormatToken` — typically one of `FORMAT_PAL_SQUARE`,
    /// `FORMAT_NTSC_CCIR_601`, etc. `0` means `FORMAT_UNKNOWN` and the
    /// remaining fields hold special / arbitrary values.
    video_format_token: u32,
    /// `VideoStandard` — one of `STANDARD_UNKNOWN`, `STANDARD_PAL`,
    /// `STANDARD_NTSC`, `STANDARD_SECAM`.
    video_standard: u32,
    /// `dwVerticalRefreshRate` — Hz; conventionally 60 for NTSC, 50
    /// for PAL.
    vertical_refresh_rate: u32,
    /// `dwHTotalInT` — total horizontal samples per line.
    h_total_in_t: u32,
    /// `dwVTotalInLines` — total vertical lines per frame.
    v_total_in_lines: u32,
    /// `dwFrameAspectRatio` — packed (X << 16) | Y. e.g. 0x0004_0003
    /// = 4:3, 0x0010_0009 = 16:9.
    frame_aspect_ratio: u32,
    /// `dwFrameWidthInPixels` — active frame width.
    frame_width_in_pixels: u32,
    /// `dwFrameHeightInLines` — active frame height.
    frame_height_in_lines: u32,
    /// `nbFieldPerFrame` — 1 (progressive) or 2 (interlaced).
    nb_field_per_frame: u32,
    /// Trailing `VIDEO_FIELD_DESC` records (round-9 candidate 1). One
    /// per field; capped at `nb_field_per_frame` and at the chunk's
    /// remaining body length so a truncated tail produces a short
    /// vector rather than an error.
    field_descs: Vec<VprpFieldDesc>,
}

/// One `VIDEO_FIELD_DESC` record from a `vprp` chunk per OpenDML 2.0
/// §5.0 (round-9 candidate 1). 8 DWORDs = 32 bytes describing one
/// field's compressed extent + active rectangle within the frame.
///
/// Stamped on the typed [`AviDemuxer::vprp_field_descs`] accessor so
/// callers wanting per-field rendering (interlaced PAL/NTSC, EDV-style
/// half-height previews) don't have to re-parse the raw vprp body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VprpFieldDesc {
    /// `CompressedBMHeight` — height in lines of the compressed bitmap
    /// for this field. For progressive (1 field/frame) this equals the
    /// full frame height; for interlaced (2 fields/frame) it's
    /// half-frame.
    pub compressed_bm_height: u32,
    /// `CompressedBMWidth` — compressed bitmap width in pixels.
    pub compressed_bm_width: u32,
    /// `ValidBMHeight` — height in lines of the *valid* (= visible)
    /// portion of the compressed bitmap. May be less than
    /// `compressed_bm_height` when the encoder pads.
    pub valid_bm_height: u32,
    /// `ValidBMWidth` — valid bitmap width in pixels.
    pub valid_bm_width: u32,
    /// `ValidBMXOffset` — x-offset of the valid rectangle's top-left
    /// corner inside the compressed bitmap.
    pub valid_bm_x_offset: u32,
    /// `ValidBMYOffset` — y-offset of the valid rectangle's top-left
    /// corner inside the compressed bitmap.
    pub valid_bm_y_offset: u32,
    /// `VideoXOffsetInT` — x-offset of the bitmap inside the video
    /// signal's horizontal active region (in `T` units, see
    /// `dwHTotalInT`).
    pub video_x_offset_in_t: u32,
    /// `VideoYValidStartLine` — first line of the field within the
    /// total `dwVTotalInLines` count.
    pub video_y_valid_start_line: u32,
}

/// One `AVISUPERINDEX_ENTRY` parsed from an `indx` chunk.
///
/// We don't dereference `qw_offset` directly — the `ix##` chunks it
/// points to are picked up by the in-movi scan in `scan_ix_in_movi`,
/// which is more robust when the super-index entries are stale or
/// pointing into the wrong segment (some encoders are sloppy here).
/// The fields are retained for diagnostics / debug-print use.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
struct SuperIndexEntry {
    /// File-absolute offset of the matching `ix##` (`AVISTDINDEX`) chunk.
    qw_offset: u64,
    /// Size of the `AVISTDINDEX` segment in bytes.
    dw_size: u32,
    /// Time span covered by chunks indexed by that `ix##`, in stream ticks.
    dw_duration: u32,
}

/// One `indx` AVISUPERINDEX chunk found inside a `strl` LIST.
///
/// Empty (zero entries, all-zero chunk_id) for streams that don't carry
/// one — tracked alongside [`StreamInfo`] for index-by-stream lookup.
#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
struct SuperIndex {
    /// 4 for AVI 2.0 super-indexes (each entry is 4 DWORDs = 16 bytes).
    /// Captured for diagnostics; not used in seek.
    w_longs_per_entry: u16,
    /// 0 (default) or `AVI_INDEX_SUB_2FIELD`.
    b_index_sub_type: u8,
    /// FourCC of indexed chunks (`00dc` etc.). Tags every `ix##` slot.
    chunk_id: [u8; 4],
    entries: Vec<SuperIndexEntry>,
}

/// One `AVISTDINDEX_ENTRY` parsed from an `ix##` chunk.
#[derive(Clone, Copy, Debug)]
struct StdIndexEntry {
    /// `dwOffset`: byte offset from `StdIndex::qw_base_offset` to the
    /// chunk's data (i.e. just past its 8-byte header). For 2-field
    /// entries this points at the FIRST field's data.
    dw_offset: u32,
    /// `dwSize` with the keyframe-bit cleared: payload size in bytes.
    /// For 2-field entries this is the combined size of both fields.
    dw_size: u32,
    /// True iff the std-index entry's `dwSize` high bit is clear.
    is_keyframe: bool,
    /// Per OpenDML 2.0 §3.0 "AVI Field Index Chunk": offset (relative
    /// to `StdIndex::qw_base_offset`) of the SECOND field's data when
    /// the parent index has `bIndexSubType == AVI_INDEX_2FIELD`. Zero
    /// for default (single-field / progressive) entries. Surfaced via
    /// [`AviDemuxer::field2_offset_for_index`] for callers that want
    /// per-field rendering.
    #[allow(dead_code)]
    dw_offset_field2: u32,
}

/// One `ix##` AVISTDINDEX chunk parsed out of a `movi` LIST.
#[derive(Clone, Debug)]
struct StdIndex {
    /// FourCC of indexed chunks (`00dc` etc.). The two ASCII digits at
    /// `chunk_id[0..2]` give the stream number.
    chunk_id: [u8; 4],
    /// Base offset for `dw_offset` lookups — typically the file offset
    /// of the enclosing `movi` LIST's first chunk header.
    qw_base_offset: u64,
    /// Index sub-type: 0 (default, progressive) or
    /// `AVI_INDEX_SUB_2FIELD` (2-field interlaced).
    #[allow(dead_code)]
    b_index_sub_type: u8,
    entries: Vec<StdIndexEntry>,
}

/// One entry parsed from the `idx1` top-level chunk, normalised to
/// file-absolute offsets and annotated with a stream-local pts.
#[derive(Clone, Copy, Debug)]
struct IdxEntry {
    /// Stream index (0..streams.len()), derived from the first two ASCII
    /// digits of the `ckid` FourCC.
    stream: u32,
    /// Raw flags field; bit 0x10 is `AVIIF_KEYFRAME`.
    flags: u32,
    /// File-absolute offset of the chunk header (8-byte `ckid` + size).
    offset: u64,
    /// Payload size as recorded in idx1.
    #[allow(dead_code)]
    size: u32,
    /// Synthesised PTS at this entry (in the stream's time base). Matches
    /// `per_stream_counter[stream]` right after `next_packet` finishes
    /// returning the packet pointed to by this entry.
    pts: i64,
}

/// `AVIIF_KEYFRAME` bit in an idx1 entry's flags.
const AVIIF_KEYFRAME: u32 = 0x0000_0010;

impl Demuxer for AviDemuxer {
    fn format_name(&self) -> &str {
        "avi"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.per_stream_counter.len() != self.streams.len() {
            self.per_stream_counter = vec![0u64; self.streams.len()];
        }
        loop {
            let current_end = self
                .movi_segments
                .get(self.current_segment)
                .map(|s| s.1)
                .ok_or(Error::Eof)?;
            if self.input.stream_position()? >= current_end {
                // Advance to the next movi segment if there is one; its
                // start is a separate region of the file.
                self.current_segment += 1;
                if let Some(&(next_start, _)) = self.movi_segments.get(self.current_segment) {
                    self.input.seek(SeekFrom::Start(next_start))?;
                    continue;
                }
                return Err(Error::Eof);
            }
            // Lenient header read: a short read at the segment tail
            // (truncated-head AVI; segment_end = file_len) means "stop"
            // rather than "I/O error".
            let hdr = match read_chunk_header_lenient(&mut *self.input)? {
                Some(h) => h,
                None => return Err(Error::Eof),
            };
            // `LIST rec ` is an optional grouping inside movi — some writers
            // cluster chunks this way. Recurse by entering the list body.
            if hdr.id == LIST {
                let _form = read_form_type(&mut *self.input)?; // likely "rec "
                                                               // Continue: next iteration will consume its nested chunks.
                continue;
            }
            // End of movi guard in case sizes disagree.
            let body_end = self.input.stream_position()? + hdr.size as u64;
            if body_end > current_end {
                // Truncated or bad size — stop.
                return Err(Error::Eof);
            }
            if hdr.id == *b"JUNK" || hdr.id == *b"junk" {
                skip_chunk(&mut *self.input, &hdr)?;
                continue;
            }
            // Payload chunk format: "NNsf" where NN is two ASCII digits and
            // sf ∈ {"dc","db","wb","pc","tx"}.
            if let Some(idx) = parse_stream_index(&hdr.id) {
                if (idx as usize) < self.streams.len() {
                    let expected = self.packet_chunk_suffix[idx as usize];
                    let suffix = [hdr.id[2], hdr.id[3]];
                    // Round-8 candidate 3: explicitly recognise `xxpc`
                    // VfW palette-change chunks. They're not regular
                    // video data so we still skip them from the packet
                    // stream, but we bump the per-stream counter
                    // (lazily — the static idx1 scan in `open()`
                    // already covers files with idx1; this catches
                    // idx1-less files where the runtime walk is the
                    // only path that sees these chunks). The cap
                    // doubles as a guard against malformed files
                    // declaring billions of palette changes.
                    if suffix == *b"pc" {
                        let s = idx as usize;
                        if self.palette_change_counts.len() <= s {
                            self.palette_change_counts.resize(s + 1, 0);
                        }
                        self.palette_change_counts[s] =
                            self.palette_change_counts[s].saturating_add(1);
                        // Round-12 C1: also buffer the body so
                        // `palette_change_data(stream)` can return it.
                        // Skip when the eager `idx1` walk in `open()`
                        // already populated the buffer (avoids double-
                        // append on idx1-bearing files where
                        // `next_packet` re-walks the same chunks).
                        if self.sideband_data_loaded {
                            skip_chunk(&mut *self.input, &hdr)?;
                        } else {
                            if self.palette_change_data.len() <= s {
                                self.palette_change_data.resize(s + 1, Vec::new());
                            }
                            match read_body_bounded(&mut *self.input, hdr.size) {
                                Ok(body) => {
                                    skip_pad(&mut *self.input, hdr.size)?;
                                    self.palette_change_data[s].push(body);
                                }
                                Err(_) => {
                                    skip_chunk(&mut *self.input, &hdr)?;
                                }
                            }
                        }
                        continue;
                    }
                    // Round-10 C1: explicitly recognise `xxtx`
                    // text/subtitle chunks per `mmsystem.h`'s
                    // text-stream FourCC family. Same handling shape
                    // as `xxpc` — skip from the packet stream, bump
                    // the per-stream counter so the metadata key
                    // `avi:text_chunk.<stream>` and the typed
                    // [`AviDemuxer::text_chunk_count`] accessor stay
                    // in sync with what the static idx1 scan
                    // produced for files that have an idx1.
                    if suffix == *b"tx" {
                        let s = idx as usize;
                        if self.text_chunk_counts.len() <= s {
                            self.text_chunk_counts.resize(s + 1, 0);
                        }
                        self.text_chunk_counts[s] = self.text_chunk_counts[s].saturating_add(1);
                        // Round-12 C1: same body-buffer hookup as `xxpc`.
                        if self.sideband_data_loaded {
                            skip_chunk(&mut *self.input, &hdr)?;
                        } else {
                            if self.text_chunk_data.len() <= s {
                                self.text_chunk_data.resize(s + 1, Vec::new());
                            }
                            match read_body_bounded(&mut *self.input, hdr.size) {
                                Ok(body) => {
                                    skip_pad(&mut *self.input, hdr.size)?;
                                    self.text_chunk_data[s].push(body);
                                }
                                Err(_) => {
                                    skip_chunk(&mut *self.input, &hdr)?;
                                }
                            }
                        }
                        continue;
                    }
                    let accept = suffix == expected
                        || suffix == *b"dc"
                        || suffix == *b"db"
                        || suffix == *b"wb";
                    if accept {
                        let data = match read_body_bounded(&mut *self.input, hdr.size) {
                            Ok(d) => d,
                            Err(e) if is_unexpected_eof(&e) => {
                                // Truncated tail: drop the partial frame.
                                return Err(Error::Eof);
                            }
                            Err(e) => return Err(e),
                        };
                        skip_pad(&mut *self.input, hdr.size)?;
                        let stream = &self.streams[idx as usize];
                        let counter = self.per_stream_counter[idx as usize];
                        // PTS: for video the counter is a frame index in the
                        // stream's time_base. For audio we advance by the
                        // number of samples in this packet (PCM: block_align
                        // derived from bps*channels; other codecs we just use
                        // the packet counter in units of rate/scale).
                        let pts = counter as i64;
                        let mut pkt = Packet::new(idx, stream.time_base, data);
                        pkt.pts = Some(pts);
                        pkt.dts = Some(pts);
                        pkt.flags.keyframe = true;
                        // Bump counter.
                        let bump = packet_time_delta(stream, pkt.data.len());
                        self.per_stream_counter[idx as usize] = counter + bump;
                        return Ok(pkt);
                    } else {
                        skip_chunk(&mut *self.input, &hdr)?;
                        continue;
                    }
                } else {
                    skip_chunk(&mut *self.input, &hdr)?;
                    continue;
                }
            }
            skip_chunk(&mut *self.input, &hdr)?;
        }
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if (stream_index as usize) >= self.streams.len() {
            return Err(Error::invalid(format!(
                "AVI: stream index {stream_index} out of range"
            )));
        }
        // OpenDML-driven seek: when the AVI 1.0 `idx1` table is missing
        // but OpenDML 2.0 `ix##` standard indexes are present, we can
        // still seek by walking the std-index entries for the matching
        // stream. The std-indexes index every chunk across every RIFF
        // segment, so they're the canonical OpenDML-only seek path.
        if self.idx_table.is_empty() {
            if !self.std_indexes.is_empty() {
                return self.seek_via_std_indexes(stream_index, pts);
            }
            return Err(Error::unsupported(
                "AVI: seek requires idx1 or OpenDML ix## standard indexes",
            ));
        }

        // Find the last keyframe entry for `stream_index` with pts <= target.
        let mut best: Option<&IdxEntry> = None;
        for e in &self.idx_table {
            if e.stream != stream_index || (e.flags & AVIIF_KEYFRAME) == 0 {
                continue;
            }
            if e.pts <= pts {
                best = match best {
                    Some(b) if b.pts >= e.pts => Some(b),
                    _ => Some(e),
                };
            }
        }
        // Fall back to the first keyframe of this stream if nothing matches
        // (e.g. caller asked for a negative pts).
        if best.is_none() {
            for e in &self.idx_table {
                if e.stream == stream_index && (e.flags & AVIIF_KEYFRAME) != 0 {
                    best = Some(e);
                    break;
                }
            }
        }
        let landed = best.ok_or_else(|| {
            Error::unsupported(format!(
                "AVI: no keyframes in idx1 for stream {stream_index}"
            ))
        })?;

        // Seek the input to the landed chunk header. Clamp against the
        // segment the offset lives in (idx1 only covers the primary
        // segment, but we re-locate the matching segment anyway so a
        // future indx/ix##-backed seek can point into later segments).
        let mut target_off = landed.offset;
        if target_off < self.movi_start {
            target_off = self.movi_start;
        }
        let seg = self
            .movi_segments
            .iter()
            .position(|&(s, e)| target_off >= s && target_off < e)
            .ok_or_else(|| Error::invalid("AVI: idx1 entry points past end of movi segments"))?;
        self.current_segment = seg;
        self.input.seek(SeekFrom::Start(target_off))?;

        // Reset per-stream pts counters. For streams we have idx entries
        // for, use the stream-local pts at-or-before `target_off`. For
        // streams we don't, reset to zero (the counter will resynchronise
        // once we next see a packet for that stream — this is imperfect
        // but there's no better signal without a dense index).
        if self.per_stream_counter.len() != self.streams.len() {
            self.per_stream_counter = vec![0u64; self.streams.len()];
        } else {
            for c in self.per_stream_counter.iter_mut() {
                *c = 0;
            }
        }
        for e in &self.idx_table {
            if e.offset > target_off {
                break;
            }
            let s = e.stream as usize;
            if s < self.per_stream_counter.len() {
                // Latest idx entry at-or-before target_off for this stream.
                self.per_stream_counter[s] = e.pts.max(0) as u64;
            }
        }

        Ok(landed.pts)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

impl AviDemuxer {
    /// Per-packet `dwOffsetField2` accessor for OpenDML 2.0 2-field
    /// streams (round-5 candidate 1).
    ///
    /// Until now the field-2 offsets surfaced only as the comma-joined
    /// `avi:ix.<stream>.field2_offsets` metadata value, which forces
    /// callers walking packets to re-parse the demuxer's own metadata
    /// just to associate a field-2 byte position with the packet they
    /// just read. This accessor returns `Some(offset)` when:
    ///
    /// 1. A 2-field `ix##` (`bIndexSubType == AVI_INDEX_2FIELD`) was
    ///    parsed for `stream_index`,
    /// 2. `packet_seq` (the zero-based packet ordinal **for that
    ///    stream** in file order) is within the std-index entry list,
    /// 3. The matching std-index entry has a non-zero
    ///    `dwOffsetField2`.
    ///
    /// Returned offsets are `qwBaseOffset`-relative (i.e. relative to
    /// the first chunk header inside the enclosing `movi` LIST), per
    /// OpenDML 2.0 §3.0 "AVI Field Index Chunk". Callers that want a
    /// file-absolute offset can add the matching segment's
    /// `movi_start` (= `(start, end)` from the segment list, which
    /// the demuxer already exposes via the public `metadata()`
    /// `avi:ix.<stream>.is_2field` key — the public surface
    /// intentionally stays minimal).
    ///
    /// Returns `None` for non-2-field streams, out-of-range
    /// `packet_seq`, or unknown `stream_index`.
    /// `LIST INFO` round-trip read accessor (round-8 candidate 2).
    ///
    /// Returns the FIRST string value associated with the `LIST INFO`
    /// FourCC `id` (e.g. `*b"INAM"` for title, `*b"IART"` for artist),
    /// or `None` when no matching entry was parsed. Mirrors the lookup
    /// shape of [`AviMuxOptions::with_info`] so a muxer→demuxer
    /// round-trip can verify INFO entries written via the builder API
    /// without re-parsing the raw `metadata()` slice.
    ///
    /// Both well-known FourCCs (mapped to canonical keys like
    /// `"title"`, `"artist"`, etc — see `info_id_to_key`) and unknown
    /// FourCCs (surfaced as `"avi:info.<fourcc>"`) are matched
    /// transparently. Use [`AviDemuxer::info_all_for`] to enumerate
    /// every value when a FourCC appears multiple times (the
    /// `LIST INFO` registry permits duplicates and our parser
    /// preserves order).
    pub fn info_for(&self, id: [u8; 4]) -> Option<&str> {
        let canonical = info_id_to_key(&id);
        let avi_namespaced = if id.iter().all(|b| b.is_ascii_graphic()) {
            std::str::from_utf8(&id)
                .ok()
                .map(|s| format!("avi:info.{s}"))
        } else {
            Some(format!(
                "avi:info.tag_{:02x}{:02x}{:02x}{:02x}",
                id[0], id[1], id[2], id[3]
            ))
        };
        for (k, v) in &self.metadata {
            if let Some(canon) = canonical {
                if k == canon {
                    return Some(v.as_str());
                }
            }
            if let Some(ns) = avi_namespaced.as_deref() {
                if k == ns {
                    return Some(v.as_str());
                }
            }
        }
        None
    }

    /// String-keyed sibling of [`Self::info_all_for`] (round-12
    /// candidate 3). Accepts the 4-byte `LIST INFO` FourCC as a `&str`
    /// (e.g. `"INAM"`, `"IART"`, `"ICMT"`) instead of a `[u8; 4]`
    /// literal so callers that already have FourCCs as strings —
    /// from JSON, command-line flags, or metadata mapping tables —
    /// don't have to convert. Non-4-character keys return an empty
    /// Vec (no canonical-key fallback: this is the strict-FourCC
    /// surface, not the canonical-name lookup).
    ///
    /// Returns every matching value in file order. For multi-entry
    /// FourCCs (e.g. two `IART` for two artists) returns both. Empty
    /// Vec means no `LIST INFO` entry was parsed for that FourCC.
    pub fn all_info_for(&self, fourcc: &str) -> Vec<&str> {
        let bytes = fourcc.as_bytes();
        if bytes.len() != 4 {
            return Vec::new();
        }
        let id = [bytes[0], bytes[1], bytes[2], bytes[3]];
        self.info_all_for(id)
    }

    /// `LIST INFO` round-trip read accessor for repeating FourCCs
    /// (round-8 candidate 2). The `LIST INFO` registry is a flat
    /// list, so a single `id` may appear multiple times (e.g. two
    /// `IART` entries for "Artist 1" / "Artist 2"). This accessor
    /// returns ALL values in file order; the empty Vec means no
    /// matching entry was parsed.
    pub fn info_all_for(&self, id: [u8; 4]) -> Vec<&str> {
        let canonical = info_id_to_key(&id);
        let avi_namespaced = if id.iter().all(|b| b.is_ascii_graphic()) {
            std::str::from_utf8(&id)
                .ok()
                .map(|s| format!("avi:info.{s}"))
        } else {
            Some(format!(
                "avi:info.tag_{:02x}{:02x}{:02x}{:02x}",
                id[0], id[1], id[2], id[3]
            ))
        };
        let mut out: Vec<&str> = Vec::new();
        for (k, v) in &self.metadata {
            let matches_canonical = canonical.is_some_and(|c| k == c);
            let matches_ns = avi_namespaced.as_deref().is_some_and(|ns| k == ns);
            if matches_canonical || matches_ns {
                out.push(v.as_str());
            }
        }
        out
    }

    /// Per-stream count of `xxpc` palette-change chunks seen during
    /// the `movi` walk (round-8 candidate 3).
    ///
    /// VfW palette-change chunks (`NNpc` per `aviriff.h`'s
    /// `cktypePALchange` constant) carry retroactive `BITMAPINFO`-
    /// style palette updates for indexed-colour video streams. The
    /// demuxer skips them from the regular packet stream (they're
    /// not video data per se) but counts them per stream. A non-zero
    /// count indicates the file carries palette animation. The same
    /// data also surfaces under the `avi:palette_change.<stream>`
    /// metadata key.
    ///
    /// Returns `0` when no `xxpc` chunks were seen for that stream
    /// or `stream_index` is out of range.
    pub fn palette_change_count(&self, stream_index: u32) -> u32 {
        self.palette_change_counts
            .get(stream_index as usize)
            .copied()
            .unwrap_or(0)
    }

    /// Per-stream count of `xxtx` text/subtitle chunks (round-10
    /// candidate 1).
    ///
    /// Text chunks (`NNtx` per `mmsystem.h`'s text-stream FourCC
    /// family) carry caption / subtitle / cuepoint payloads attached
    /// to a stream. Like palette-change chunks they're skipped from
    /// the regular packet stream; this accessor mirrors
    /// [`Self::palette_change_count`] for the text family. Same data
    /// also surfaces under `avi:text_chunk.<stream>` metadata.
    ///
    /// Returns `0` when no `xxtx` chunks were seen for that stream
    /// or `stream_index` is out of range.
    pub fn text_chunk_count(&self, stream_index: u32) -> u32 {
        self.text_chunk_counts
            .get(stream_index as usize)
            .copied()
            .unwrap_or(0)
    }

    /// Per-stream `xxpc` palette-change chunk bodies in file order
    /// (round-12 candidate 1). Returns the raw payloads written by
    /// [`crate::muxer::AviMuxer::write_palette_change`] (or any other
    /// AVI 1.0 writer) — typically a `BITMAPINFO`-style palette delta:
    /// 1-byte `bFirstEntry`, 1-byte `bNumEntries`, 2-byte `wFlags`,
    /// then `bNumEntries * 4`-byte palette quads. Closes the round-trip
    /// pair with the round-11 C3 muxer write helper so callers can
    /// verify byte-equality across mux→demux without re-reading the
    /// raw file.
    ///
    /// For files that carry an `idx1`, the bodies are populated
    /// eagerly at `open()` and available before the first
    /// [`oxideav_core::Demuxer::next_packet`] call. For `idx1`-less
    /// (OpenDML-only) files, `xxpc` chunks land in the buffer as the
    /// `next_packet` walk encounters them (the demuxer skips them from
    /// the regular packet stream but reads their body for this
    /// accessor).
    ///
    /// Returns an empty slice for unknown `stream_index`. The returned
    /// slice's length matches [`Self::palette_change_count`] when the
    /// data path is fully populated.
    pub fn palette_change_data(&self, stream_index: u32) -> &[Vec<u8>] {
        self.palette_change_data
            .get(stream_index as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Typed [`PaletteChange`] decode of every `xxpc` chunk attached
    /// to `stream_index` (round-13 candidate 1).
    ///
    /// Composes the round-12 raw [`Self::palette_change_data`]
    /// accessor with [`PaletteChange::parse`] so callers don't have to
    /// hand-decode the AVI 1.0 `BITMAPINFO`-style palette delta. Each
    /// entry corresponds 1:1 with the same-indexed raw payload; bodies
    /// that fail to parse (shorter than the 4-byte fixed header or with
    /// a non-4-multiple `PALETTEENTRY` tail) are dropped from the
    /// returned `Vec` rather than aborting the call. Returns an empty
    /// `Vec` for unknown `stream_index`.
    ///
    /// Pairs with [`crate::muxer::AviMuxer::with_palette_change_typed`]
    /// for the typed muxer side; a writer → reader cycle preserves
    /// `first_entry` / `num_entries` / `flags` / every
    /// [`PaletteEntry`] quad.
    pub fn palette_change_typed(&self, stream_index: u32) -> Vec<PaletteChange> {
        self.palette_change_data
            .get(stream_index as usize)
            .map(|v| v.iter().filter_map(|b| PaletteChange::parse(b)).collect())
            .unwrap_or_default()
    }

    /// Lazy [`PaletteChange`] iterator over every `xxpc` chunk attached
    /// to `stream_index` (round-14 candidate 3).
    ///
    /// Mirrors [`Self::palette_change_typed`] but yields one
    /// `Result<PaletteChange>` per `next()` call instead of materialising
    /// the full `Vec` up front. Useful for palette-animated screen
    /// captures where each second of footage may carry hundreds or
    /// thousands of palette deltas — the eager `Vec` form clones every
    /// `Vec<PaletteEntry>` even when the consumer only needs to walk
    /// once.
    ///
    /// Each `next()` returns:
    /// - `Some(Ok(pc))` for a successfully decoded palette delta,
    /// - `Some(Err(_))` for a body that failed to parse (shorter than
    ///   the 4-byte fixed header or with a non-4-multiple
    ///   `PALETTEENTRY` tail) — the iterator advances past the bad body
    ///   so subsequent `next()` calls keep yielding,
    /// - `None` once every chunk for the requested stream is consumed
    ///   (or immediately for an unknown `stream_index`).
    ///
    /// The iterator borrows the raw body slice from the demuxer (no
    /// extra allocation per chunk for the body itself); only the
    /// successfully-decoded `PaletteChange` allocates its own
    /// `Vec<PaletteEntry>`.
    pub fn palette_change_typed_iter(&self, stream_index: u32) -> PaletteChangeTypedIter<'_> {
        let bodies = self
            .palette_change_data
            .get(stream_index as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        PaletteChangeTypedIter { bodies, next: 0 }
    }

    /// `avih.dwSuggestedBufferSize` accessor (round-13 candidate 2).
    ///
    /// Per AVI 1.0 §3.1, the avih's `dwSuggestedBufferSize` is the
    /// largest single chunk a player should expect to read in one
    /// shot — the recommended read-ahead allocation hint. Conformant
    /// muxers populate it with the maximum chunk-body size across all
    /// streams (see [`crate::muxer::AviMuxOptions::with_suggested_buffer_size`]
    /// for the writer override). The same value also surfaces under the
    /// `avi:suggested_buffer_size` metadata key.
    ///
    /// Returns `0` when the field was zero on disk (some legacy writers
    /// leave it unpopulated) or the file had no parsable `avih`.
    pub fn avih_suggested_buffer_size(&self) -> u32 {
        self.avih_suggested_buffer_size
    }

    /// Per-stream `xxtx` text/subtitle chunk bodies in file order
    /// (round-12 candidate 1). Mirror of [`Self::palette_change_data`]
    /// for the text family — returns the verbatim payloads as written
    /// by [`crate::muxer::AviMuxer::write_text_chunk`] (typically a
    /// caption / subtitle / cuepoint string per `mmsystem.h`'s
    /// `ckidAVITextSF` convention).
    ///
    /// Same population rules as `palette_change_data`: eagerly cached
    /// from `idx1` at `open()` when present, else populated lazily by
    /// the `next_packet` walk for OpenDML-only files. The slice length
    /// matches [`Self::text_chunk_count`] when fully populated.
    pub fn text_chunk_data(&self, stream_index: u32) -> &[Vec<u8>] {
        self.text_chunk_data
            .get(stream_index as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Typed [`AvihFlags`] decode of `AVIMAINHEADER.dwFlags` (round-10
    /// candidate 3).
    ///
    /// Returns the per-bit booleans for the documented `AVIF_*`
    /// flags from Microsoft's `vfw.h`:
    /// `AVIF_HASINDEX` / `AVIF_MUSTUSEINDEX` / `AVIF_ISINTERLEAVED` /
    /// `AVIF_TRUSTCKTYPE` / `AVIF_WASCAPTUREFILE` / `AVIF_COPYRIGHTED`
    /// plus the raw u32 `bits` for callers wanting to inspect
    /// undocumented vendor-extension bits. Same data also surfaces as
    /// the `avi:flags` hex-string metadata key.
    pub fn avih_flags(&self) -> AvihFlags {
        AvihFlags::from_bits(self.avih_flags)
    }

    /// Per-packet idx1 flags accessor (round-6 candidate 1).
    ///
    /// Returns `Some(flags)` for the `packet_seq`-th idx1 entry (zero-
    /// based, in idx1 file order) belonging to `stream_index`, or
    /// `None` for an out-of-range index or unknown stream. Flags
    /// follow vfw.h conventions: `AVIIF_KEYFRAME` (0x10),
    /// `AVIIF_FIRSTPART` (0x20), `AVIIF_LASTPART` (0x40). The muxer
    /// sets `AVIIF_FIRSTPART | AVIIF_LASTPART` (0x60) on every idx1
    /// entry for a 2-field interlaced stream, so a reader that only
    /// has idx1 (no `ix##`) can still detect 2-field carriage by
    /// checking these bits.
    pub fn idx1_flags_for_packet(&self, stream_index: u32, packet_seq: usize) -> Option<u32> {
        // Round-8 candidate 1: O(1) lookup via the pre-computed
        // per-stream flags table. The legacy O(N) walk over
        // `idx_table` is gone; the cache is built once at `open()`
        // (see `idx1_flags_per_stream`).
        self.idx1_flags_per_stream
            .get(stream_index as usize)?
            .get(packet_seq)
            .copied()
    }

    pub fn field2_offset_for_packet(&self, stream_index: u32, packet_seq: usize) -> Option<u32> {
        // Walk std_indexes in file order and pick the per-stream
        // `packet_seq`-th entry whose parent index carries
        // AVI_INDEX_SUB_2FIELD.
        let mut seen = 0usize;
        for ix in &self.std_indexes {
            let stream = parse_stream_index(&ix.chunk_id)?;
            if stream != stream_index {
                continue;
            }
            if ix.b_index_sub_type != AVI_INDEX_SUB_2FIELD {
                // Non-2-field index for this stream: still advances
                // the per-stream packet ordinal so callers can use a
                // single counter even for streams whose carriage
                // changes mid-file.
                seen = seen.saturating_add(ix.entries.len());
                continue;
            }
            let local = packet_seq.checked_sub(seen)?;
            if local < ix.entries.len() {
                let v = ix.entries[local].dw_offset_field2;
                return if v == 0 { None } else { Some(v) };
            }
            seen = seen.saturating_add(ix.entries.len());
        }
        None
    }

    /// OpenDML 2.0 §5.0 `dmlh.dwTotalFrames` (round-9 candidate 3).
    ///
    /// Returns `Some(total)` when the file declares a `LIST odml dmlh`
    /// extended header — typical for OpenDML 2.0 multi-RIFF files
    /// where `avih.dwTotalFrames` only carries the primary segment's
    /// frame count and the cross-segment truth lives here. Returns
    /// `None` for files without OpenDML extensions.
    ///
    /// Widened to `u64` because the dword value is unsigned and a
    /// signed `i64` is what most callers want for arithmetic against
    /// pts/duration fields (`u32::MAX` ≈ 47 days @ 30 fps which is
    /// well past anything the spec contemplates but the wider type is
    /// future-proof).
    pub fn dmlh_total_frames(&self) -> Option<u64> {
        self.dmlh_total_frames.map(|v| v as u64)
    }

    /// Per-stream `vprp` `VIDEO_FIELD_DESC` records (round-9
    /// candidate 1).
    ///
    /// Returns the trailing per-field-rect array parsed from the
    /// stream's `vprp` chunk, or an empty slice when the stream
    /// didn't declare a `vprp` (or declared one with `nbFieldPerFrame
    /// = 0` / a truncated tail). The slice length is at most
    /// `nb_field_per_frame` (1 progressive, 2 interlaced) and capped
    /// at the body's actual remaining bytes — see `parse_vprp`.
    ///
    /// Each [`VprpFieldDesc`] carries the 8 DWORDs of the spec's
    /// per-field record: compressed bitmap dims, valid (visible)
    /// rectangle dims + offset, and the signal-domain x-offset /
    /// y-start-line. Callers wanting per-field rendering of an
    /// interlaced stream can pull these out without re-parsing the
    /// raw `vprp` body or walking metadata strings.
    pub fn vprp_field_descs(&self, stream_index: u32) -> &[VprpFieldDesc] {
        match self.vprps.get(stream_index as usize) {
            Some(vp) => &vp.field_descs,
            None => &[],
        }
    }

    /// Backward-walking strict keyframe seek (round-9 candidate 4).
    ///
    /// Locates the last keyframe at-or-before `target_pts` in
    /// `stream_index`'s seek table — the same landing point [`Demuxer::seek_to`]
    /// would pick — but returns a structured [`KeyframeSeekResult`]
    /// that exposes the gap between `target_pts` and the keyframe the
    /// demuxer actually landed on. Callers can use the gap to:
    ///
    /// 1. Decide whether the file's GOP structure makes the seek
    ///    practical (a 100-frame gap means decoding 100 P-frames to
    ///    reach the wanted PTS),
    /// 2. Plan a decode-and-discard loop after the seek to land at
    ///    the originally-requested PTS,
    /// 3. Detect mid-GOP requests vs. keyframe-aligned ones (gap == 0).
    ///
    /// Operates on the same indexes as `seek_to` (idx1 first, then
    /// OpenDML std-indexes). Returns the same errors when neither
    /// index is present or no keyframe ≤ target exists. Does *not*
    /// mutate the demuxer state — the input is repositioned exactly
    /// the same way `seek_to` does it, but you can call
    /// [`Demuxer::seek_to`] separately afterwards if you only want
    /// the side-effect.
    pub fn seek_to_keyframe_strict(
        &mut self,
        stream_index: u32,
        target_pts: i64,
    ) -> Result<KeyframeSeekResult> {
        let landed_pts = <Self as Demuxer>::seek_to(self, stream_index, target_pts)?;
        // gop_distance is the number of stream ticks the caller would
        // have to advance from the landed keyframe to reach the
        // originally-requested target. Saturating keeps the math sane
        // when `target_pts < landed_pts` (rare; only happens when the
        // caller asks for a negative pts and the first keyframe is at
        // pts >= 0).
        let gop_distance = target_pts.saturating_sub(landed_pts).max(0);
        Ok(KeyframeSeekResult {
            target_pts,
            landed_pts,
            gop_distance,
        })
    }

    /// OpenDML-only strict keyframe seek (round-11 candidate 2).
    ///
    /// Mirror of [`Self::seek_to_keyframe_strict`] that always walks
    /// the OpenDML 2.0 `ix##` standard-index collection — bypassing
    /// the AVI 1.0 `idx1` table even when one is present. Returns
    /// the same [`KeyframeSeekResult`] shape so callers can interrogate
    /// `gop_distance` to plan a decode-and-discard loop.
    ///
    /// Use this variant when:
    /// - The file has BOTH `idx1` and `ix##` and you want to verify
    ///   that the std-index seek lands on the same keyframe (a
    ///   sanity check on muxer fidelity), or
    /// - You're working with an OpenDML-only file (no `idx1` chunk
    ///   at all) and you want a compile-time guarantee the seek
    ///   used the std-index path rather than failing through the
    ///   `seek_to` dispatcher.
    ///
    /// Returns `Error::Unsupported` when the file has no `ix##`
    /// chunks, or no keyframe entry for `stream_index` exists in
    /// the std-index collection.
    pub fn seek_to_keyframe_strict_via_std_index(
        &mut self,
        stream_index: u32,
        target_pts: i64,
    ) -> Result<KeyframeSeekResult> {
        if (stream_index as usize) >= self.streams.len() {
            return Err(Error::invalid(format!(
                "AVI: stream index {stream_index} out of range"
            )));
        }
        if self.std_indexes.is_empty() {
            return Err(Error::unsupported(
                "AVI: seek_to_keyframe_strict_via_std_index requires OpenDML ix## standard indexes",
            ));
        }
        let landed_pts = self.seek_via_std_indexes(stream_index, target_pts)?;
        let gop_distance = target_pts.saturating_sub(landed_pts).max(0);
        Ok(KeyframeSeekResult {
            target_pts,
            landed_pts,
            gop_distance,
        })
    }

    /// OpenDML 2.0 fallback for `seek_to` when no AVI 1.0 `idx1` table
    /// is present.
    ///
    /// Walks the in-memory `StdIndex` collection (one per (stream,
    /// segment) pair, parsed from the `ix##` chunks during `open()`)
    /// and lands on the last keyframe entry for `stream_index` whose
    /// running pts is ≤ `target_pts`. Each entry's pts is synthesised
    /// the same way `build_idx_table` does it for `idx1`: walk the
    /// per-stream entries in file order, advancing per-stream pts by
    /// `packet_time_delta(stream, size)` per chunk.
    ///
    /// Per-stream PTS counters are reset to the landed entry's value so
    /// `next_packet` resumes synthesising correct PTS post-seek.
    fn seek_via_std_indexes(&mut self, stream_index: u32, target_pts: i64) -> Result<i64> {
        // Collect every entry for this stream from `std_indexes`,
        // tagged with the running per-stream pts, the file offset of
        // the chunk header, and the keyframe flag. Std-indexes appear
        // in file order so the running pts is monotonic across them.
        let mut per_stream_entries: Vec<(u64, i64, bool)> = Vec::new();
        let mut running_pts: i64 = 0;
        for ix in &self.std_indexes {
            let stream = match parse_stream_index(&ix.chunk_id) {
                Some(s) => s,
                None => continue,
            };
            if stream != stream_index {
                continue;
            }
            let s = stream as usize;
            for e in &ix.entries {
                let abs_off = ix.qw_base_offset.saturating_add(e.dw_offset as u64);
                // The std-index dwOffset points at the chunk *data*
                // (just past the 8-byte header). Our `next_packet`
                // expects to land on the chunk header, so back off 8.
                let header_off = abs_off.saturating_sub(8);
                per_stream_entries.push((header_off, running_pts, e.is_keyframe));
                let bump = packet_time_delta(&self.streams[s], e.dw_size as usize) as i64;
                running_pts = running_pts.saturating_add(bump);
            }
        }
        if per_stream_entries.is_empty() {
            return Err(Error::unsupported(format!(
                "AVI: no OpenDML std-index entries for stream {stream_index}"
            )));
        }
        // Find last keyframe entry with pts <= target_pts.
        let mut best: Option<(u64, i64)> = None;
        for &(off, pts, kf) in &per_stream_entries {
            if !kf {
                continue;
            }
            if pts <= target_pts {
                best = Some(match best {
                    Some(b) if b.1 >= pts => b,
                    _ => (off, pts),
                });
            }
        }
        // Fall back to the first keyframe if nothing matches.
        if best.is_none() {
            for &(off, pts, kf) in &per_stream_entries {
                if kf {
                    best = Some((off, pts));
                    break;
                }
            }
        }
        let (target_off, landed_pts) = best.ok_or_else(|| {
            Error::unsupported(format!(
                "AVI: no keyframes in std-index for stream {stream_index}"
            ))
        })?;
        // Find which segment hosts this offset.
        let seg = self
            .movi_segments
            .iter()
            .position(|&(s, e)| target_off >= s && target_off < e)
            .ok_or_else(|| Error::invalid("AVI: ix## entry points outside of any movi segment"))?;
        self.current_segment = seg;
        self.input.seek(SeekFrom::Start(target_off))?;

        // Reset per-stream PTS counters. Walk every std-index entry in
        // file order and assign each stream's running pts to the value
        // at-the-entry whose offset is the latest at-or-before
        // target_off. `next_packet` then resumes synthesising correct
        // timestamps because it picks up from per_stream_counter[s]
        // and only bumps after returning each packet.
        if self.per_stream_counter.len() != self.streams.len() {
            self.per_stream_counter = vec![0u64; self.streams.len()];
        } else {
            for c in self.per_stream_counter.iter_mut() {
                *c = 0;
            }
        }
        // Per-stream running pts threaded across every ix-block so
        // boundary entries carry over correctly. (A naive
        // re-initialisation from per_stream_counter[s] at the start
        // of every ix block would drop one tick each time because
        // we assign-before-bump and the previous block's tail bump
        // is in a local that doesn't survive the boundary.) For each
        // entry, we always advance running_pts; we only stamp
        // per_stream_counter when the entry sits at-or-before
        // target_off — that way per_stream_counter ends up holding
        // the pts of the latest qualifying entry per stream.
        let mut running_pts: Vec<u64> = vec![0u64; self.streams.len()];
        for ix in &self.std_indexes {
            let stream = match parse_stream_index(&ix.chunk_id) {
                Some(s) => s,
                None => continue,
            };
            let s = stream as usize;
            if s >= self.per_stream_counter.len() {
                continue;
            }
            for e in &ix.entries {
                let abs_off = ix.qw_base_offset.saturating_add(e.dw_offset as u64);
                let header_off = abs_off.saturating_sub(8);
                if header_off <= target_off {
                    self.per_stream_counter[s] = running_pts[s];
                }
                let bump = packet_time_delta(&self.streams[s], e.dw_size as usize);
                running_pts[s] = running_pts[s].saturating_add(bump);
            }
        }
        Ok(landed_pts)
    }
}

/// Parse "NNsf" where NN is two ASCII digits into the stream index.
fn parse_stream_index(name: &[u8; 4]) -> Option<u32> {
    let h = ascii_hex(name[0])?;
    let l = ascii_hex(name[1])?;
    Some((h as u32) * 16 + l as u32)
}

/// Decode a single ASCII hex digit (0-9, a-f, A-F).
fn ascii_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn packet_time_delta(stream: &StreamInfo, payload_len: usize) -> u64 {
    match stream.params.media_type {
        MediaType::Video => 1,
        MediaType::Audio => {
            // PCM: duration = frames = payload / block_align. Non-PCM: one
            // tick per packet is a reasonable fallback.
            let block_align = stream
                .params
                .channels
                .zip(stream.params.sample_format)
                .map(|(c, f)| (c as usize) * f.bytes_per_sample())
                .filter(|&v| v > 0)
                .unwrap_or(0);
            payload_len.checked_div(block_align).unwrap_or(1) as u64
        }
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_index_parses() {
        assert_eq!(parse_stream_index(b"00dc"), Some(0));
        assert_eq!(parse_stream_index(b"01wb"), Some(1));
        assert_eq!(parse_stream_index(b"0adb"), Some(10));
        assert_eq!(parse_stream_index(b"XXXX"), None);
    }

    #[test]
    fn parse_ix_chunk_default_subtype_8b_entries() {
        // Hand-build an `ix##` body: wLongsPerEntry = 2, subType = 0,
        // bIndexType = 0x01, 2 entries. Each entry is 8 B
        // (dwOffset, dwSize/flags). Verify the parser surfaces the
        // entries with the keyframe bit decoded.
        let mut body = Vec::new();
        body.extend_from_slice(&2u16.to_le_bytes()); // wLongsPerEntry
        body.push(0); // bIndexSubType
        body.push(0x01); // bIndexType = AVI_INDEX_OF_CHUNKS
        body.extend_from_slice(&2u32.to_le_bytes()); // nEntriesInUse
        body.extend_from_slice(b"00dc"); // dwChunkId
        body.extend_from_slice(&0x1000u64.to_le_bytes()); // qwBaseOffset
        body.extend_from_slice(&0u32.to_le_bytes()); // dwReserved3
                                                     // Entry 0: keyframe.
        body.extend_from_slice(&0x100u32.to_le_bytes()); // dwOffset
        body.extend_from_slice(&512u32.to_le_bytes()); // dwSize (high bit clear → kf)
                                                       // Entry 1: delta frame.
        body.extend_from_slice(&0x300u32.to_le_bytes());
        body.extend_from_slice(&((512u32) | 0x8000_0000).to_le_bytes());

        let parsed = parse_ix_chunk(&body).unwrap();
        assert_eq!(&parsed.chunk_id, b"00dc");
        assert_eq!(parsed.qw_base_offset, 0x1000);
        assert_eq!(parsed.b_index_sub_type, 0);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].dw_offset, 0x100);
        assert_eq!(parsed.entries[0].dw_size, 512);
        assert!(parsed.entries[0].is_keyframe);
        assert_eq!(parsed.entries[0].dw_offset_field2, 0);
        assert_eq!(parsed.entries[1].dw_offset, 0x300);
        assert_eq!(parsed.entries[1].dw_size, 512);
        assert!(!parsed.entries[1].is_keyframe);
    }

    #[test]
    fn parse_ix_chunk_2field_subtype_12b_entries() {
        // 2-field index per OpenDML 2.0 §3.0 "AVI Field Index Chunk":
        //   wLongsPerEntry = 3, bIndexSubType = AVI_INDEX_2FIELD,
        //   each entry is (dwOffset, dwSize, dwOffsetField2) = 12 B.
        let mut body = Vec::new();
        body.extend_from_slice(&3u16.to_le_bytes()); // wLongsPerEntry = 3
        body.push(AVI_INDEX_SUB_2FIELD); // 1
        body.push(0x01); // bIndexType = AVI_INDEX_OF_CHUNKS
        body.extend_from_slice(&1u32.to_le_bytes()); // nEntriesInUse
        body.extend_from_slice(b"00dc");
        body.extend_from_slice(&0x2000u64.to_le_bytes()); // qwBaseOffset
        body.extend_from_slice(&0u32.to_le_bytes()); // dwReserved3
                                                     // Entry 0: 2-field interlaced video; field-2 offset follows.
        body.extend_from_slice(&0x40u32.to_le_bytes()); // dwOffset (field 1)
        body.extend_from_slice(&1024u32.to_le_bytes()); // dwSize (whole frame)
        body.extend_from_slice(&0x80u32.to_le_bytes()); // dwOffsetField2

        let parsed = parse_ix_chunk(&body).expect("2-field index must parse");
        assert_eq!(parsed.b_index_sub_type, AVI_INDEX_SUB_2FIELD);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].dw_offset, 0x40);
        assert_eq!(parsed.entries[0].dw_size, 1024);
        assert_eq!(
            parsed.entries[0].dw_offset_field2, 0x80,
            "field-2 offset must round-trip from the 12-byte entry layout"
        );
        assert!(parsed.entries[0].is_keyframe);
    }

    #[test]
    fn parse_vprp_extracts_fixed_dwords() {
        // Hand-build a vprp body with all 9 fixed DWORDs populated;
        // skip the trailing per-field-rect array (the parser tolerates
        // its absence).
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_le_bytes()); // VideoFormatToken (FORMAT_NTSC_SQUARE-ish)
        body.extend_from_slice(&2u32.to_le_bytes()); // VideoStandard = STANDARD_NTSC
        body.extend_from_slice(&60u32.to_le_bytes()); // dwVerticalRefreshRate
        body.extend_from_slice(&780u32.to_le_bytes()); // dwHTotalInT
        body.extend_from_slice(&525u32.to_le_bytes()); // dwVTotalInLines
        body.extend_from_slice(&((4u32 << 16) | 3u32).to_le_bytes()); // dwFrameAspectRatio = 4:3
        body.extend_from_slice(&640u32.to_le_bytes()); // dwFrameWidthInPixels
        body.extend_from_slice(&480u32.to_le_bytes()); // dwFrameHeightInLines
        body.extend_from_slice(&2u32.to_le_bytes()); // nbFieldPerFrame = 2 (interlaced)

        let v = parse_vprp(&body).expect("vprp must parse");
        assert_eq!(v.video_format_token, 3);
        assert_eq!(v.video_standard, 2);
        assert_eq!(v.vertical_refresh_rate, 60);
        assert_eq!(v.h_total_in_t, 780);
        assert_eq!(v.v_total_in_lines, 525);
        assert_eq!(v.frame_aspect_ratio, (4u32 << 16) | 3);
        assert_eq!(v.frame_width_in_pixels, 640);
        assert_eq!(v.frame_height_in_lines, 480);
        assert_eq!(v.nb_field_per_frame, 2);
        // Round-9 candidate 1: tail-truncated body → no per-field
        // descs, but the fixed preamble still parses.
        assert!(
            v.field_descs.is_empty(),
            "no rect tail in the body → no field_descs"
        );
    }

    #[test]
    fn parse_vprp_extracts_two_field_rects() {
        // Round-9 candidate 1: parse a vprp body with two
        // VIDEO_FIELD_DESC records appended (interlaced PAL-ish: top
        // field starts at line 23, bottom at line 335; both
        // 720×288).
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes()); // VideoFormatToken = PAL_CCIR_601
        body.extend_from_slice(&1u32.to_le_bytes()); // VideoStandard = STANDARD_PAL
        body.extend_from_slice(&50u32.to_le_bytes()); // dwVerticalRefreshRate
        body.extend_from_slice(&864u32.to_le_bytes()); // dwHTotalInT
        body.extend_from_slice(&625u32.to_le_bytes()); // dwVTotalInLines
        body.extend_from_slice(&((4u32 << 16) | 3u32).to_le_bytes()); // dwFrameAspectRatio
        body.extend_from_slice(&720u32.to_le_bytes()); // dwFrameWidthInPixels
        body.extend_from_slice(&576u32.to_le_bytes()); // dwFrameHeightInLines
        body.extend_from_slice(&2u32.to_le_bytes()); // nbFieldPerFrame = 2
                                                     // Field 0 (top).
        body.extend_from_slice(&288u32.to_le_bytes()); // CompressedBMHeight
        body.extend_from_slice(&720u32.to_le_bytes()); // CompressedBMWidth
        body.extend_from_slice(&288u32.to_le_bytes()); // ValidBMHeight
        body.extend_from_slice(&720u32.to_le_bytes()); // ValidBMWidth
        body.extend_from_slice(&0u32.to_le_bytes()); // ValidBMXOffset
        body.extend_from_slice(&0u32.to_le_bytes()); // ValidBMYOffset
        body.extend_from_slice(&0u32.to_le_bytes()); // VideoXOffsetInT
        body.extend_from_slice(&23u32.to_le_bytes()); // VideoYValidStartLine (top)
                                                      // Field 1 (bottom).
        body.extend_from_slice(&288u32.to_le_bytes());
        body.extend_from_slice(&720u32.to_le_bytes());
        body.extend_from_slice(&288u32.to_le_bytes());
        body.extend_from_slice(&720u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&335u32.to_le_bytes()); // VideoYValidStartLine (bottom)

        let v = parse_vprp(&body).expect("vprp must parse");
        assert_eq!(v.field_descs.len(), 2);
        assert_eq!(v.field_descs[0].compressed_bm_height, 288);
        assert_eq!(v.field_descs[0].compressed_bm_width, 720);
        assert_eq!(v.field_descs[0].valid_bm_height, 288);
        assert_eq!(v.field_descs[0].valid_bm_width, 720);
        assert_eq!(v.field_descs[0].video_y_valid_start_line, 23);
        assert_eq!(v.field_descs[1].video_y_valid_start_line, 335);
    }

    #[test]
    fn parse_vprp_truncated_tail_clamps_field_descs() {
        // Round-9 candidate 1: nbFieldPerFrame=2 but only one rect's
        // worth of bytes is appended → return one descriptor.
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes()); // VideoFormatToken
        body.extend_from_slice(&0u32.to_le_bytes()); // VideoStandard
        body.extend_from_slice(&50u32.to_le_bytes()); // dwVerticalRefreshRate
        body.extend_from_slice(&864u32.to_le_bytes());
        body.extend_from_slice(&625u32.to_le_bytes());
        body.extend_from_slice(&((4u32 << 16) | 3u32).to_le_bytes());
        body.extend_from_slice(&720u32.to_le_bytes());
        body.extend_from_slice(&576u32.to_le_bytes());
        body.extend_from_slice(&2u32.to_le_bytes()); // nbFieldPerFrame = 2 …
                                                     // … but only one rect follows.
        body.extend_from_slice(&288u32.to_le_bytes());
        body.extend_from_slice(&720u32.to_le_bytes());
        body.extend_from_slice(&288u32.to_le_bytes());
        body.extend_from_slice(&720u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&23u32.to_le_bytes());

        let v = parse_vprp(&body).expect("vprp must parse");
        assert_eq!(
            v.field_descs.len(),
            1,
            "truncated tail → only the descs that fit"
        );
    }

    #[test]
    fn parse_vprp_short_returns_none() {
        // < 36 bytes → can't decode the 9 fixed DWORDs.
        let body = vec![0u8; 16];
        assert!(parse_vprp(&body).is_none());
    }
}
