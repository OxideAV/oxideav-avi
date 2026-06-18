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
use crate::stream_format::{
    parse_bitmap_info_header, parse_waveformatex, parse_waveformatextensible, subformat_codec_hint,
    ChannelLayout, ChannelMask, Guid, WAVE_FORMAT_EXTENSIBLE,
};

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
    open_avi_inner(
        input, codecs, /* lenient */ false, /* strict_cross_validate */ false,
    )
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
    open_avi_inner(
        input, codecs, /* lenient */ true, /* strict_cross_validate */ false,
    )
}

/// Open an AVI demuxer with strict idx1 ↔ ix## cross-validation
/// (round-18 candidate 3).
///
/// Behaves like [`open_avi`] except: when both an `idx1` table and
/// per-segment `ix##` standard indexes are present and they disagree
/// on a packet's `(file-offset, payload-size)`, the demuxer fails
/// fast with [`Error::InvalidData`] carrying the divergent sequence
/// number and both candidate offsets — rather than surfacing the
/// disagreement as the lenient `avi:idx1.<n>.divergent_offsets`
/// metadata key. Use this when the caller wants the canonical
/// "OpenDML ix## is more reliable than idx1" handoff to abort on a
/// mismatch (e.g. validating a freshly muxed file before shipping
/// it, or refusing to play a recovered capture whose stale idx1
/// disagrees with reality). All other open-time checks still run —
/// the round-14 C2 audio sample-size VBR/CBR validator stays armed
/// and surfaces its own [`Error::Validation`] on violation.
pub fn open_avi_strict(input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<AviDemuxer> {
    open_avi_inner(
        input, codecs, /* lenient */ false, /* strict_cross_validate */ true,
    )
}

fn open_avi_inner(
    mut input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
    lenient: bool,
    strict_cross_validate: bool,
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
    // Per-stream video BMIH side-info (top-down + BI_BITFIELDS masks)
    // — round-19 C1+C2. Parallel to `streams`: `Some` for video,
    // `None` otherwise.
    let mut video_strfs: Vec<Option<VideoStrfInfo>> = Vec::new();
    // Per-stream audio WAVEFORMATEX(TENSIBLE) side-info (channel
    // mask, valid-bits-per-sample, SubFormat GUID) — round-75
    // WAVEFORMATEXTENSIBLE landing. Parallel to `streams`: `Some`
    // for audio, `None` otherwise.
    let mut audio_strfs: Vec<Option<AudioStrfInfo>> = Vec::new();
    // Per-stream optional name captured from the `strn` chunk per AVI
    // 1.0 §"AVI Stream Headers" (round-80). Parallel to `streams`:
    // `Some(name)` when the strl carried a `strn` chunk, `None`
    // otherwise. Empty-payload strn chunks parse as `None` so the
    // accessor distinguishes "no name declared" from "empty name".
    let mut stream_names: Vec<Option<String>> = Vec::new();
    // Per-stream optional codec-driver `strd` blob captured from the
    // `strd` chunk per AVI 1.0 §"AVI Stream Headers" (round-89). Parallel
    // to `streams`: `Some(bytes)` when the strl carried a `strd` chunk,
    // `None` otherwise. Empty-payload `strd` (cb=0) parses as
    // `Some(Vec::new())` so an empty driver blob stays distinguishable
    // from "no strd chunk at all". The spec defines this body as opaque
    // codec-driver configuration bytes — the demuxer does not interpret
    // them.
    let mut stream_header_data: Vec<Option<Vec<u8>>> = Vec::new();
    // Per-stream `strh.rcFrame` destination rectangle captured from the
    // 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-115).
    // Parallel to `streams`: `Some([left, top, right, bottom])` when the
    // strh declared a non-zero rect, `None` when it was absent (48-byte
    // header) or the all-zero "whole movie rectangle" writer default, so
    // the accessor distinguishes "explicit sub-rectangle" from "default /
    // unspecified".
    let mut stream_frame_rects: Vec<Option<[i16; 4]>> = Vec::new();
    // Round-119: per-stream `strh.wLanguage` LANGID captured from byte
    // offset 14 of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER".
    // Parallel to `streams`: `Some(langid)` when the strh declared a
    // non-zero language tag, `None` when it carried the `0`
    // ("LANG_NEUTRAL / SUBLANG_NEUTRAL", the writer-skips-it default)
    // so the accessor distinguishes "explicit language tag" from
    // "unspecified / absent" — mirroring the round-115 `rcFrame` and
    // round-80 `strn` convention.
    let mut stream_languages: Vec<Option<u16>> = Vec::new();
    // Round-153: per-stream `strh.dwInitialFrames` captured from byte
    // offset 16 of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    // (`docs/container/riff/avi-riff-file-reference.md`, `dwInitialFrames`
    // row): *"How far audio data is skewed ahead of the video frames in
    // interleaved files. Typically, this is about 0.75 seconds. If
    // creating interleaved files, set the value of this member to the
    // number of frames in the file prior to the initial frame of the
    // AVI sequence."* Parallel to `streams`: `Some(frames)` when the
    // strh declared a non-zero skew, `None` when it carried the `0`
    // writer default ("noninterleaved file" per AVIMAINHEADER §
    // `dwInitialFrames`: *"Noninterleaved files should specify zero"*)
    // so an unspecified skew reads the same as an absent one, mirroring
    // the round-119 `wLanguage` / round-115 `rcFrame` "default == absent"
    // convention.
    let mut stream_initial_frames: Vec<Option<u32>> = Vec::new();
    // Round-176: per-stream `strh.dwQuality` indicator from byte offset 40
    // of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwQuality`
    // row in `docs/container/riff/avi-riff-file-reference.md`, line 246).
    // Parallel to `streams`: `Some(quality)` when the strh declared a
    // value in `[0, u32::MAX - 1]`, `None` when it carried the documented
    // `-1` (= `0xFFFF_FFFF` u32) "use default driver quality" sentinel
    // (the legacy muxer default) so an unspecified quality reads the
    // same as an absent one, mirroring the round-153 `dwInitialFrames`
    // "default == absent" convention.
    let mut stream_qualities: Vec<Option<u32>> = Vec::new();
    // Round-182: per-stream `strh.wPriority` selection hint from byte
    // offset 12 of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    // (`wPriority` row in `docs/container/riff/avi-riff-file-reference.md`,
    // Appendix B line 238: *"Priority of a stream type. For example, in a
    // file with multiple audio streams, the one with the highest priority
    // might be the default stream."*). Parallel to `streams`:
    // `Some(priority)` when the strh declared a non-zero hint, `None`
    // when it carried the legacy `0` writer default so an unspecified
    // priority reads the same as an absent one, mirroring the
    // round-119 `wLanguage` / round-176 `dwQuality` "default == absent"
    // convention.
    let mut stream_priorities: Vec<Option<u16>> = Vec::new();
    // Round-203: per-stream `strh.dwStart` starting time from byte
    // offset 28 of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    // (`dwStart` row in `docs/container/riff/avi-riff-file-reference.md`
    // line 243: *"Starting time for this stream. The units are defined
    // by the dwRate and dwScale members in the main file header.
    // Usually, this is zero, but it can specify a delay time for a
    // stream that does not start concurrently with the file."*).
    // Parallel to `streams`: `Some(start)` when the strh declared a
    // non-zero start, `None` when it carried the legacy `0` writer
    // default (the spec-documented "starts concurrently with the file"
    // value) so an unspecified start reads the same as an absent one,
    // mirroring the round-182 `wPriority` / round-176 `dwQuality` /
    // round-153 `dwInitialFrames` "default == absent" convention.
    let mut stream_starts: Vec<Option<u32>> = Vec::new();
    // Round-210: per-stream `strh.fccHandler` driver hint from byte
    // offset 4 of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    // (`fccHandler` row in `docs/container/riff/avi-riff-file-reference.md`,
    // Appendix B line 236: *"An optional FOURCC that identifies a
    // specific data handler. The data handler is the preferred handler
    // for the stream. For audio and video streams, this specifies the
    // codec for decoding the stream."*). Parallel to `streams`:
    // `Some([f0,f1,f2,f3])` when the strh declared a non-zero FourCC,
    // `None` when it carried the all-zero `\0\0\0\0` "no preferred
    // handler" default (the spec uses the *optional* qualifier and
    // audio-stream writers in the wild routinely leave the field zero)
    // so an unspecified driver hint reads the same as an absent one,
    // mirroring the round-203 `dwStart` / round-182 `wPriority` /
    // round-176 `dwQuality` / round-153 `dwInitialFrames` /
    // round-119 `wLanguage` / round-115 `rcFrame` "default == absent"
    // convention.
    let mut stream_handlers: Vec<Option<[u8; 4]>> = Vec::new();
    // Round-217: per-stream `strh.dwSuggestedBufferSize` read-ahead hint
    // from byte offset 36 of the AVISTREAMHEADER per AVI 1.0
    // §"AVISTREAMHEADER" (`dwSuggestedBufferSize` row in
    // `docs/container/riff/avi-riff-file-reference.md` line 245: *"How
    // large a buffer should be used to read this stream. Typically, this
    // contains a value corresponding to the largest chunk present in the
    // stream. Using the correct buffer size makes playback more efficient.
    // Use zero if you do not know the correct buffer size."*). Parallel
    // to `streams`: `Some(n)` when the strh declared a non-zero hint,
    // `None` when it carried the spec-documented `0` "do not know"
    // sentinel — so an unspecified hint reads the same as an absent one,
    // mirroring the round-210 `fccHandler` / round-203 `dwStart` /
    // round-182 `wPriority` / round-176 `dwQuality` / round-153
    // `dwInitialFrames` / round-119 `wLanguage` / round-115 `rcFrame`
    // "default == absent" convention.
    let mut stream_suggested_buffer_sizes: Vec<Option<u32>> = Vec::new();
    // Round-229: per-stream `strh.dwLength` from byte offset 32 of the
    // AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwLength` row in
    // `docs/container/riff/avi-riff-file-reference.md` line 244: *"Length
    // of this stream. The units are defined by the dwRate and dwScale
    // members of the stream's header."*). Parallel to `streams`: `Some(n)`
    // when the strh declared a non-zero length, `None` when it carried the
    // `0` "no length declared" value (typical for half-written capture
    // dumps and the case the long-standing internal `length > 0` duration
    // guard already treated as absent) — so a zero-length / unspecified
    // stream reads the same as an absent one, mirroring the round-222
    // `dwSampleSize` / round-217 `dwSuggestedBufferSize` / round-210
    // `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
    // round-176 `dwQuality` / round-153 `dwInitialFrames` / round-119
    // `wLanguage` / round-115 `rcFrame` "default == absent" convention.
    // The unit is the stream's own `(dwRate / dwScale)` tick and the
    // demuxer surfaces the raw u32 verbatim with no rate-conversion.
    let mut stream_lengths: Vec<Option<u32>> = Vec::new();
    // Round-222: per-stream `strh.dwSampleSize` indicator from byte
    // offset 44 of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    // (`dwSampleSize` row in
    // `docs/container/riff/avi-riff-file-reference.md` line 247: *"The
    // size of a single sample of data. This is set to zero if the samples
    // can vary in size. … For video streams, this number is typically
    // zero, although it can be nonzero if all video frames are the same
    // size. For audio streams, this number should be the same as the
    // nBlockAlign member of the WAVEFORMATEX structure describing the
    // audio."*). Parallel to `streams`: `Some(n)` when the strh declared
    // a non-zero size, `None` when it carried the spec-documented `0`
    // "samples can vary in size" sentinel — so an unspecified hint reads
    // the same as an absent one, mirroring the round-217
    // `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    // `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    // round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
    // `rcFrame` "default == absent" convention.
    let mut stream_sample_sizes: Vec<Option<u32>> = Vec::new();
    // Round-247: per-stream `strh.dwFlags` raw u32 from byte offset 8 of
    // the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwFlags` row
    // in `docs/container/riff/avi-riff-file-reference.md` line 237 +
    // the *dwFlags values* table at lines 252–255 carrying
    // `AVISF_DISABLED` (`0x0000_0001`) and `AVISF_VIDEO_PALCHANGES`
    // (`0x0001_0000`)). Parallel to `streams`: `Some(bits)` when the
    // strh declared a non-zero flag field, `None` when it carried the
    // `0` legacy "no flags set" writer default — so an unspecified
    // flag DWORD reads the same as an absent one, mirroring the
    // round-229 `dwLength` / round-222 `dwSampleSize` / round-217
    // `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    // `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    // round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
    // `rcFrame` "default == absent" convention. Non-zero values
    // surface verbatim — the demuxer does NOT mask undocumented bits
    // (some legacy capture filters pack driver-private flags in the
    // upper half-DWORD outside the spec's two documented bits).
    let mut stream_flags: Vec<Option<u32>> = Vec::new();
    // Round-249: per-stream `(strh.dwScale, strh.dwRate)` timebase pair
    // captured verbatim from byte offsets 20 + 24 of the AVISTREAMHEADER
    // per AVI 1.0 §"AVISTREAMHEADER" (`dwScale` row line 241:
    // *"Used with dwRate to specify the time scale that this stream will
    // use. Dividing dwRate by dwScale gives the number of samples per
    // second. For video streams, this is the frame rate. For audio
    // streams, this rate corresponds to the time needed to play
    // nBlockAlign bytes of audio, which for PCM audio is the just the
    // sample rate."* + the `dwRate` cross-reference line 242).
    // Parallel to `streams`: `Some((scale, rate))` when both raw DWORDs
    // were non-zero, `None` when either was zero (a writer-skips-it /
    // mathematically-undefined `rate/scale` ratio). Note the internal
    // `time_base` derivation still applies `.max(1)` to each member so
    // a degenerate file remains decodable; the raw-DWORD surface keeps
    // the on-disk byte pattern observable for round-trip parity.
    let mut stream_rates: Vec<Option<(u32, u32)>> = Vec::new();
    // Round-253: per-stream `strh.fccType` raw FOURCC captured verbatim
    // from byte offset 0 of the AVISTREAMHEADER per AVI 1.0
    // §"AVISTREAMHEADER" (`fccType` row in
    // `docs/container/riff/avi-riff-file-reference.md`, Appendix B line
    // 235 + the `fcc` row at line 234: *"A FOURCC code that specifies
    // the type of data contained in the stream. The following standard
    // AVI values are defined: `auds` (audio stream), `mids` (MIDI
    // stream), `txts` (text stream), `vids` (video stream)."*).
    // Parallel to `streams`: `Some(fcc)` when the strh declared a
    // non-zero FOURCC, `None` when it carried the all-zero
    // `[0, 0, 0, 0]` sentinel so an unspecified type reads the same as
    // an absent one — mirroring the round-249 `(dwScale, dwRate)` /
    // round-247 `dwFlags` / round-229 `dwLength` "default == absent"
    // convention. The 4 bytes surface verbatim and are NOT validated
    // against the spec-documented `{auds, mids, txts, vids}` set —
    // the spec does not pin a closed registry, and vendor-specific
    // FOURCCs are surfaced for the caller to interpret.
    let mut stream_fcc_types: Vec<Option<[u8; 4]>> = Vec::new();
    // Round-107: the optional `IDIT` digitization-date text chunk inside
    // `LIST hdrl` (RIFF *Hdrl Tags* namespace; `DateTimeOriginal`).
    // `None` until/unless the primary RIFF's `hdrl` carries an `IDIT`
    // chunk with a non-empty (after NUL/whitespace trim) body.
    let mut digitization_date: Option<String> = None;
    // Round-112: the optional `ISMP` SMPTE-timecode text chunk inside
    // `LIST hdrl` (RIFF *Hdrl Tags* namespace; `TimeCode`).
    // `None` until/unless the primary RIFF's `hdrl` carries an `ISMP`
    // chunk with a non-empty (after NUL/whitespace trim) body.
    let mut smpte_timecode: Option<String> = None;

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
        &mut video_strfs,
        &mut audio_strfs,
        &mut stream_names,
        &mut stream_header_data,
        &mut stream_frame_rects,
        &mut stream_languages,
        &mut stream_initial_frames,
        &mut stream_qualities,
        &mut stream_priorities,
        &mut stream_starts,
        &mut stream_handlers,
        &mut stream_suggested_buffer_sizes,
        &mut stream_sample_sizes,
        &mut stream_lengths,
        &mut stream_flags,
        &mut stream_rates,
        &mut stream_fcc_types,
        &mut digitization_date,
        &mut smpte_timecode,
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
                    &mut video_strfs,
                    &mut audio_strfs,
                    &mut stream_names,
                    &mut stream_header_data,
                    &mut stream_frame_rects,
                    &mut stream_languages,
                    &mut stream_initial_frames,
                    &mut stream_qualities,
                    &mut stream_priorities,
                    &mut stream_starts,
                    &mut stream_handlers,
                    &mut stream_suggested_buffer_sizes,
                    &mut stream_sample_sizes,
                    &mut stream_lengths,
                    &mut stream_flags,
                    &mut stream_rates,
                    &mut stream_fcc_types,
                    &mut digitization_date,
                    &mut smpte_timecode,
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

    // Round-96: derive the per-stream CBR-audio `nBlockAlign` lookup
    // for the `ix##` standard-index block-alignment validator. Only
    // streams the sample-size invariant pins as CBR (PCM / A-law /
    // µ-law / IMA-ADPCM) with a nonzero `nBlockAlign` get a
    // `Some(block_align)`; everything else is `None`. Built here while
    // `audio_infos` is still in scope (it's parallel to `streams`).
    let audio_cbr_block_aligns: Vec<Option<u16>> = audio_infos
        .iter()
        .map(|ai| {
            ai.and_then(|info| match classify_audio_sample_size(info.format_tag) {
                // `Some(false)` ⇒ CBR; only validate when nBlockAlign is
                // meaningful (>1; a 1-byte block can never be misaligned).
                Some(false) if info.block_align > 1 => Some(info.block_align),
                _ => None,
            })
        })
        .collect();

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
        // Round-92: surface `dwPaddingGranularity` so a downstream
        // tool can detect a stream-aligned remux and (e.g.) avoid
        // re-aligning on a subsequent transcode. Zero (the legacy
        // sentinel) is omitted from the metadata Vec so the key is
        // observable only when the muxer actually opted in.
        if h.padding_granularity > 0 {
            metadata.push((
                "avi:padding_granularity".into(),
                h.padding_granularity.to_string(),
            ));
        }
        // Round-157: surface the file-global `avih.dwInitialFrames` so a
        // downstream tool can detect an interleaved-file leading-frame
        // skew without re-parsing the AVIMAINHEADER. The `0` writer
        // default ("noninterleaved file" per AVI 1.0 §"AVIMAINHEADER"
        // line 200: *"Noninterleaved files should specify zero"*) is
        // omitted from the metadata Vec so the key is observable only
        // when the muxer actually opted in, mirroring the
        // `avi:padding_granularity` / `avi:strh.<n>.initial_frames`
        // conventions.
        if h.initial_frames > 0 {
            metadata.push(("avi:initial_frames".into(), h.initial_frames.to_string()));
        }
        // Round-256: surface the file-global `avih.dwMicroSecPerFrame`
        // so a downstream tool can detect the writer's stamped
        // frame-period without re-parsing the AVIMAINHEADER. The `0`
        // writer-skips-it sentinel ("frame period unspecified" — most
        // legitimate AVIs stamp a non-zero value derived from the first
        // video stream's `(scale, rate)` pair, but a capture-card crash
        // dump or hand-edited fixture may leave it 0) is omitted from
        // the metadata Vec so the key is observable only when the value
        // is actually present, mirroring the `avi:padding_granularity` /
        // `avi:initial_frames` conventions.
        if h.micro_sec_per_frame > 0 {
            metadata.push((
                "avi:micro_sec_per_frame".into(),
                h.micro_sec_per_frame.to_string(),
            ));
        }
        // Round-268: surface the file-global `avih.dwTotalFrames` so a
        // downstream tool can inspect the writer's stamped frame count
        // without re-parsing the AVIMAINHEADER. Per AVI 1.0
        // §"AVIMAINHEADER" (line 199): *"Total number of frames of
        // data in the file."* For a multi-segment OpenDML file this
        // only carries the primary segment's count (per OpenDML 2.0
        // §5.0); `avi:total_frames_all_segments` (from `dmlh`) carries
        // the cross-segment truth. The `0` writer-skips-it /
        // empty-file sentinel is omitted from the metadata Vec so the
        // key is observable only when the writer actually stamped a
        // count, mirroring the `avi:micro_sec_per_frame` /
        // `avi:max_bytes_per_sec` / `avi:initial_frames` conventions.
        if h.total_frames > 0 {
            metadata.push(("avi:total_frames".into(), h.total_frames.to_string()));
        }
        // Round-330: surface the file-global `avih.dwReserved[4]` array
        // (offsets 40..56 of the 56-byte body) whenever any of the four
        // trailing DWORDs is non-zero. Per AVI 1.0 §"AVIMAINHEADER"
        // (line 205): *"Reserved. Set this array to zero."* A
        // spec-conformant writer leaves all four `0`, so the key is
        // emitted only for a non-conformant header — a hand-edited /
        // capture-card / vendor-extended file that stamped data into the
        // reserved slot — keeping the spec-default absence observable,
        // mirroring the `avi:padding_granularity` / `avi:initial_frames`
        // / `avi:total_frames` conventions. The value is the four DWORDs
        // rendered as comma-joined lower-case `0x`-prefixed hex in array
        // order, e.g. `"0x00000000,0xDEADBEEF,0x00000000,0x00000000"`.
        if h.reserved.iter().any(|&w| w != 0) {
            let joined = h
                .reserved
                .iter()
                .map(|w| format!("0x{w:08X}"))
                .collect::<Vec<_>>()
                .join(",");
            metadata.push(("avi:reserved".into(), joined));
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
    // `avi:total_frames` (from `avih.total_frames`, emitted above as
    // of round-268) stays single-segment for legacy callers, while
    // `avi:total_frames_all_segments` carries the OpenDML truth.
    if let Some(total) = dmlh_total_frames {
        metadata.push(("avi:total_frames_all_segments".into(), total.to_string()));
    }

    // Round-107: surface the `IDIT` digitization-date string under
    // `avi:idit` when the `hdrl` carried a (non-empty) `IDIT` chunk.
    // The key is omitted entirely when no IDIT chunk was present so its
    // absence is observable in the metadata Vec (mirroring the
    // `avi:strn.<index>` / `avi:padding_granularity` conventions). The
    // value is the trimmed text verbatim — the staged docs do not pin a
    // canonical format, so no normalisation is applied here.
    if let Some(ref idit) = digitization_date {
        metadata.push(("avi:idit".into(), idit.clone()));
    }

    // Round-112: surface the `ISMP` SMPTE-timecode string under
    // `avi:ismp` when the `hdrl` carried a (non-empty) `ISMP` chunk.
    // Like `avi:idit` the key is omitted entirely when no `ISMP` chunk
    // was present so its absence is observable in the metadata Vec
    // (mirroring the `avi:strn.<index>` / `avi:padding_granularity`
    // conventions). The value is the trimmed text verbatim — the
    // staged docs do not pin a canonical SMPTE-timecode format string,
    // so no normalisation is applied here.
    if let Some(ref ismp) = smpte_timecode {
        metadata.push(("avi:ismp".into(), ismp.clone()));
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

    // Round-19 candidates 1+2: surface BMIH side-info per video stream.
    // `avi:vids.<n>.top_down = "true"` whenever the on-wire `biHeight`
    // was negative (top-down DIB origin upper-left, per VfW `wingdi.h`
    // §"biHeight sign rules"). `avi:vids.<n>.bitfields = "r=<hex>,
    // g=<hex>,b=<hex>"` when `biCompression == BI_BITFIELDS` and the
    // three trailing color masks parsed cleanly.
    for (i, vs_opt) in video_strfs.iter().enumerate() {
        let vs = match vs_opt {
            Some(v) => v,
            None => continue,
        };
        if vs.top_down {
            metadata.push((format!("avi:vids.{i}.top_down"), "true".into()));
        }
        if let Some((r, g, b)) = vs.bitfields_masks {
            metadata.push((
                format!("avi:vids.{i}.bitfields"),
                format!("r=0x{r:08X},g=0x{g:08X},b=0x{b:08X}"),
            ));
        }
    }

    // Round-75: WAVEFORMATEXTENSIBLE side-info per audio stream.
    // Mirrors the round-19 video_strf metadata-key pattern. Keys are
    // only emitted for streams whose `wFormatTag` was
    // `WAVE_FORMAT_EXTENSIBLE` (0xFFFE) and whose extension parsed
    // cleanly — absence is observable.
    for (i, as_opt) in audio_strfs.iter().enumerate() {
        let asi = match as_opt {
            Some(a) => a,
            None => continue,
        };
        if asi.format_tag != WAVE_FORMAT_EXTENSIBLE {
            continue;
        }
        if let Some(valid) = asi.valid_bits_per_sample {
            metadata.push((
                format!("avi:auds.{i}.valid_bits_per_sample"),
                valid.to_string(),
            ));
        }
        if let Some(mask) = asi.channel_mask {
            metadata.push((
                format!("avi:auds.{i}.channel_mask"),
                format!("0x{mask:08X}"),
            ));
            // Round 163: typed channel-mask decode per docs README
            // `docs/container/riff/waveformatextensible/README.md`
            // "Channel-mask channel ordering" + "Standard layouts"
            // tables. Surface a comma-joined PCM-channel-order list
            // of `SPEAKER_*` abbreviations and, when the mask matches
            // one of the seven docs-table named layouts, the layout
            // label. Both keys are omitted entirely when there are no
            // documented bits set so absence stays observable
            // (mirrors the `avi:strn` / `avi:strd` / `avi:idit`
            // "default == absent" convention).
            let cm = ChannelMask::from_raw(mask);
            if !cm.is_empty() {
                let speakers: Vec<&'static str> = cm.iter_speakers().map(|s| s.abbrev()).collect();
                metadata.push((format!("avi:auds.{i}.channel_speakers"), speakers.join(",")));
            }
            if let Some(layout) = cm.layout() {
                metadata.push((
                    format!("avi:auds.{i}.channel_layout"),
                    layout.label().to_string(),
                ));
            }
        }
        if let Some(guid) = asi.subformat {
            metadata.push((format!("avi:auds.{i}.subformat"), guid.display()));
            if let Some(tag) = guid.ksdataformat_tag() {
                metadata.push((
                    format!("avi:auds.{i}.subformat_wformat_tag"),
                    format!("0x{tag:04X}"),
                ));
            }
        }
    }

    // Round-80: AVI 1.0 §"AVI Stream Headers" optional `strn` chunk
    // — per-stream human-readable name surfaced under
    // `avi:strn.<index>`. Only present-and-non-empty entries are
    // surfaced so absence remains observable via the typed
    // [`AviDemuxer::stream_name`] accessor.
    for (i, name_opt) in stream_names.iter().enumerate() {
        if let Some(name) = name_opt {
            if !name.is_empty() {
                metadata.push((format!("avi:strn.{i}"), name.clone()));
            }
        }
    }

    // Round-89: AVI 1.0 §"AVI Stream Headers" optional `strd` chunk
    // — opaque per-stream codec-driver configuration blob ("The format
    // and content of this chunk are defined by the codec driver.
    // Typically, drivers use this information for configuration.
    // Applications that read and write AVI files do not need to
    // interpret this information; they simple transfer it to and from
    // the driver as a memory block."). The demuxer surfaces only the
    // length via the metadata key `avi:strd.<index>.len = "<bytes>"`
    // (so a downstream tool can see the chunk is present without
    // hexdumping arbitrary driver bytes into a String); the raw bytes
    // are accessible via the typed
    // [`AviDemuxer::stream_header_data`] accessor.
    for (i, sh_opt) in stream_header_data.iter().enumerate() {
        if let Some(bytes) = sh_opt {
            metadata.push((format!("avi:strd.{i}.len"), bytes.len().to_string()));
        }
    }

    // Round-115: AVI 1.0 §"AVISTREAMHEADER" `rcFrame` destination
    // rectangle. Surfaced as `avi:strh.<index>.frame_rect =
    // "left,top,right,bottom"` for every stream whose 56-byte strh
    // declared a non-zero rect (the all-zero "whole movie rectangle"
    // default is `None` in `stream_frame_rects` so the key is omitted —
    // its absence stays observable, mirroring the `avi:strn` / `avi:strd`
    // conventions). The raw four-WORD tuple is also reachable via the
    // typed [`AviDemuxer::stream_frame_rect`] accessor.
    for (i, rc_opt) in stream_frame_rects.iter().enumerate() {
        if let Some([l, t, r, b]) = rc_opt {
            metadata.push((
                format!("avi:strh.{i}.frame_rect"),
                format!("{l},{t},{r},{b}"),
            ));
        }
    }

    // Round-119: AVI 1.0 §"AVISTREAMHEADER" `wLanguage` field. Surfaced
    // as `avi:strh.<index>.language = "<u16>"` for every stream whose
    // strh declared a non-zero LANGID. The `0`
    // ("LANG_NEUTRAL / SUBLANG_NEUTRAL") writer default is `None` in
    // `stream_languages` so the key is omitted — its absence stays
    // observable, mirroring the `avi:strn` / `avi:strh.<n>.frame_rect`
    // conventions. The raw 16-bit value is also reachable via the typed
    // [`AviDemuxer::stream_language`] accessor; the staged docs do not
    // pin a registry (Microsoft conventions decode it as a LANGID
    // `(LANG_PRIMARY, SUBLANG)` pair, but non-MS writers may pack
    // different values), so callers interpret the integer per their
    // workflow rather than the demuxer normalising it.
    for (i, lang_opt) in stream_languages.iter().enumerate() {
        if let Some(lang) = lang_opt {
            metadata.push((format!("avi:strh.{i}.language"), lang.to_string()));
        }
    }

    // Round-153: AVI 1.0 §"AVISTREAMHEADER" `dwInitialFrames` field at
    // byte offset 16 of the strh. Surfaced as
    // `avi:strh.<index>.initial_frames = "<u32>"` for every stream whose
    // strh declared a non-zero skew. The `0` writer default
    // ("noninterleaved file" per AVIMAINHEADER §`dwInitialFrames`:
    // *"Noninterleaved files should specify zero"*) is `None` in
    // `stream_initial_frames` so the key is omitted — its absence stays
    // observable, mirroring the `avi:strh.<n>.language` / `frame_rect`
    // conventions. The raw 32-bit value is also reachable via the typed
    // [`AviDemuxer::stream_initial_frames`] accessor; per AVI 1.0
    // §"AVISTREAMHEADER" (`dwInitialFrames` row in
    // `docs/container/riff/avi-riff-file-reference.md`) the value is
    // "how far audio data is skewed ahead of the video frames in
    // interleaved files" — the demuxer surfaces the raw u32 verbatim
    // and leaves interpretation (in stream ticks per the stream's
    // `dwRate`/`dwScale`) to the caller.
    for (i, init_opt) in stream_initial_frames.iter().enumerate() {
        if let Some(init) = init_opt {
            metadata.push((format!("avi:strh.{i}.initial_frames"), init.to_string()));
        }
    }

    // Round-176: AVI 1.0 §"AVISTREAMHEADER" `dwQuality` field at byte
    // offset 40 of the strh. Surfaced as
    // `avi:strh.<index>.quality = "<u32>"` for every stream whose strh
    // declared a value other than the `-1` (`0xFFFF_FFFF` u32) writer
    // default ("use default driver quality" per AVI 1.0 §"AVISTREAMHEADER"
    // `dwQuality` row: *"If set to -1, drivers use the default quality
    // value."*) so an unspecified quality reads the same as an absent
    // one (mirroring the `avi:strh.<n>.initial_frames` / `language` /
    // `frame_rect` conventions). The raw 32-bit value is also reachable
    // via the typed [`AviDemuxer::stream_quality`] accessor; per the
    // spec the documented range is `[0, 10_000]` (where the value is
    // "represented as a number between 0 and 10,000"; for compressed
    // streams it typically reflects "the value of the quality parameter
    // passed to the compression software") but the demuxer surfaces the
    // raw u32 verbatim and does not clamp or normalise, so anomalous
    // out-of-range writers round-trip exactly.
    for (i, quality_opt) in stream_qualities.iter().enumerate() {
        if let Some(q) = quality_opt {
            metadata.push((format!("avi:strh.{i}.quality"), q.to_string()));
        }
    }

    // Round-182: AVI 1.0 §"AVISTREAMHEADER" `wPriority` field at byte
    // offset 12 of the strh. Surfaced as
    // `avi:strh.<index>.priority = "<u16>"` for every stream whose strh
    // declared a non-zero selection hint ("Priority of a stream type.
    // For example, in a file with multiple audio streams, the one with
    // the highest priority might be the default stream." per AVI 1.0
    // §"AVISTREAMHEADER" Appendix B `wPriority` row). The legacy `0`
    // writer default is omitted so an unspecified priority reads the
    // same as an absent one, mirroring the `avi:strh.<n>.quality` /
    // `language` / `initial_frames` conventions. The raw 16-bit value
    // is also reachable via the typed [`AviDemuxer::stream_priority`]
    // accessor; the demuxer surfaces it verbatim and does not pin a
    // value range — the spec describes a selection hint, not a
    // sortable global priority.
    for (i, priority_opt) in stream_priorities.iter().enumerate() {
        if let Some(p) = priority_opt {
            metadata.push((format!("avi:strh.{i}.priority"), p.to_string()));
        }
    }

    // Round-203: AVI 1.0 §"AVISTREAMHEADER" `dwStart` field at byte
    // offset 28 of the strh. Surfaced as
    // `avi:strh.<index>.start = "<u32>"` for every stream whose strh
    // declared a non-zero starting time ("Starting time for this
    // stream. The units are defined by the dwRate and dwScale members
    // in the main file header. Usually, this is zero, but it can
    // specify a delay time for a stream that does not start
    // concurrently with the file." per AVI 1.0 §"AVISTREAMHEADER"
    // `dwStart` row, line 243). The legacy `0` writer default
    // ("starts concurrently with the file") is omitted so an
    // unspecified start reads the same as an absent one, mirroring
    // the `avi:strh.<n>.priority` / `quality` / `initial_frames` /
    // `language` conventions. The raw 32-bit value is also reachable
    // via the typed [`AviDemuxer::stream_start`] accessor; the unit
    // is the stream's own `(dwRate / dwScale)` tick and the demuxer
    // surfaces the value verbatim with no rate-conversion.
    for (i, start_opt) in stream_starts.iter().enumerate() {
        if let Some(s) = start_opt {
            metadata.push((format!("avi:strh.{i}.start"), s.to_string()));
        }
    }

    // Round-253: AVI 1.0 §"AVISTREAMHEADER" `fccType` field at byte
    // offset 0 of the strh. Surfaced as
    // `avi:strh.<index>.fcc_type = "<fourcc-or-hex>"` for every
    // stream whose strh declared a non-zero type FOURCC ("A FOURCC
    // code that specifies the type of data contained in the stream.
    // The following standard AVI values are defined: `auds` (audio
    // stream), `mids` (MIDI stream), `txts` (text stream), `vids`
    // (video stream)." per AVI 1.0 §"AVISTREAMHEADER" Appendix B
    // `fccType` row line 235 + the `fcc` row at line 234). The
    // all-zero `\0\0\0\0` "no declared type" sentinel is omitted so
    // an unspecified type reads the same as an absent one,
    // mirroring the `avi:strh.<n>.handler` / `scale` / `rate` /
    // `flags` / `length` / `sample_size` / `suggested_buffer_size` /
    // `start` / `priority` / `quality` / `initial_frames` /
    // `language` conventions. The raw 4-byte value is also reachable
    // via the typed [`AviDemuxer::stream_fcc_type`] accessor; the
    // demuxer surfaces the bytes verbatim. The metadata-string form
    // renders as four printable ASCII characters when every byte is
    // in the `0x20..=0x7e` printable range (so e.g. `vids` /
    // `auds` / `mids` / `txts` round-trip legibly), otherwise as
    // eight lower-case hex characters with a `0x` prefix (so vendor
    // FOURCCs containing non-printable bytes stay round-trippable
    // without colliding with any ASCII tag) — matching the
    // printable-vs-hex split this crate uses for the
    // `avi:strh.<n>.handler` and `avi:<fourcc>` unknown-tag keys.
    for (i, fcc_opt) in stream_fcc_types.iter().enumerate() {
        if let Some(f) = fcc_opt {
            metadata.push((format!("avi:strh.{i}.fcc_type"), format_fourcc_or_hex(f)));
        }
    }

    // Round-210: AVI 1.0 §"AVISTREAMHEADER" `fccHandler` field at byte
    // offset 4 of the strh. Surfaced as
    // `avi:strh.<index>.handler = "<fourcc-or-hex>"` for every stream
    // whose strh declared a non-zero driver-handler FourCC ("An
    // optional FOURCC that identifies a specific data handler. The
    // data handler is the preferred handler for the stream. For audio
    // and video streams, this specifies the codec for decoding the
    // stream." per AVI 1.0 §"AVISTREAMHEADER" Appendix B `fccHandler`
    // row, line 236). The all-zero `\0\0\0\0` "no preferred handler"
    // writer default is omitted so an unspecified hint reads the same
    // as an absent one, mirroring the `avi:strh.<n>.start` /
    // `priority` / `quality` / `initial_frames` / `language`
    // conventions. The raw 4-byte value is also reachable via the
    // typed [`AviDemuxer::stream_handler`] accessor; the demuxer
    // surfaces the bytes verbatim. The metadata-string form renders
    // as four printable ASCII characters when every byte is in the
    // `0x20..=0x7e` printable range (so e.g. `MJPG` round-trips
    // legibly), otherwise as eight lower-case hex characters with a
    // `0x` prefix (so e.g. a binary `00 11 22 33` driver hint stays
    // round-trippable without colliding with any ASCII tag) — this
    // matches the printable-vs-hex split documented for the
    // `avi:<fourcc>` unknown-video-tag key in the codec mapping
    // table.
    for (i, handler_opt) in stream_handlers.iter().enumerate() {
        if let Some(h) = handler_opt {
            metadata.push((format!("avi:strh.{i}.handler"), format_fourcc_or_hex(h)));
        }
    }

    // Round-217: AVI 1.0 §"AVISTREAMHEADER" `dwSuggestedBufferSize` field
    // at byte offset 36 of the strh. Surfaced as
    // `avi:strh.<index>.suggested_buffer_size = "<u32>"` for every stream
    // whose strh declared a non-zero read-ahead hint ("How large a buffer
    // should be used to read this stream. Typically, this contains a
    // value corresponding to the largest chunk present in the stream.
    // Using the correct buffer size makes playback more efficient. Use
    // zero if you do not know the correct buffer size." per AVI 1.0
    // §"AVISTREAMHEADER" `dwSuggestedBufferSize` row, line 245). The
    // spec-documented `0` "do not know the correct buffer size"
    // sentinel is omitted so an unspecified hint reads the same as an
    // absent one, mirroring the `avi:strh.<n>.handler` / `start` /
    // `priority` / `quality` / `initial_frames` / `language`
    // conventions. The raw 32-bit value is also reachable via the
    // typed [`AviDemuxer::stream_suggested_buffer_size`] accessor; the
    // demuxer surfaces the value verbatim with no validation against
    // the actual largest chunk seen in `movi`.
    for (i, sbs_opt) in stream_suggested_buffer_sizes.iter().enumerate() {
        if let Some(n) = sbs_opt {
            metadata.push((format!("avi:strh.{i}.suggested_buffer_size"), n.to_string()));
        }
    }

    // Round-222: AVI 1.0 §"AVISTREAMHEADER" `dwSampleSize` field at byte
    // offset 44 of the strh. Surfaced as `avi:strh.<index>.sample_size =
    // "<u32>"` for every stream whose strh declared a non-zero
    // sample-size hint ("The size of a single sample of data. This is
    // set to zero if the samples can vary in size. If this number is
    // nonzero, then multiple samples of data can be grouped into a
    // single chunk within the file. … For video streams, this number is
    // typically zero, although it can be nonzero if all video frames are
    // the same size. For audio streams, this number should be the same
    // as the nBlockAlign member of the WAVEFORMATEX structure describing
    // the audio." per AVI 1.0 §"AVISTREAMHEADER" `dwSampleSize` row,
    // line 247). The spec-documented `0` "samples can vary in size"
    // sentinel is omitted so an unspecified hint reads the same as an
    // absent one, mirroring the round-217 `suggested_buffer_size` /
    // round-210 `handler` / round-203 `start` / round-182 `priority` /
    // round-176 `quality` / round-153 `initial_frames` / round-119
    // `language` conventions. The raw 32-bit value is also reachable via
    // the typed [`AviDemuxer::stream_sample_size`] accessor; the
    // demuxer surfaces the value verbatim with no validation against
    // `WAVEFORMATEX.nBlockAlign` (the round-14 C2 audio sample-size
    // invariant is a separate VBR/CBR consistency check).
    for (i, ss_opt) in stream_sample_sizes.iter().enumerate() {
        if let Some(n) = ss_opt {
            metadata.push((format!("avi:strh.{i}.sample_size"), n.to_string()));
        }
    }

    // Round-229: AVI 1.0 §"AVISTREAMHEADER" `dwLength` field at byte
    // offset 32 of the strh. Surfaced as `avi:strh.<index>.length =
    // "<u32>"` for every stream whose strh declared a non-zero
    // length ("Length of this stream. The units are defined by the
    // dwRate and dwScale members of the stream's header." per AVI 1.0
    // §"AVISTREAMHEADER" `dwLength` row, line 244). The `0` "no length
    // declared" value is omitted so an unspecified length reads the
    // same as an absent one, mirroring the round-222 `sample_size` /
    // round-217 `suggested_buffer_size` / round-210 `handler` /
    // round-203 `start` / round-182 `priority` / round-176 `quality` /
    // round-153 `initial_frames` / round-119 `language` conventions.
    // The raw 32-bit value is also reachable via the typed
    // [`AviDemuxer::stream_length`] accessor; the unit is the stream's
    // own `(dwRate / dwScale)` tick and the demuxer surfaces the value
    // verbatim with no rate-conversion. Logically distinct from the
    // `StreamInfo::duration` already exposed by [`Demuxer::streams`]
    // (also derived from this same DWORD but typed as an `Option<i64>`
    // for the framework-level duration model); the `avi:strh.<n>.length`
    // surface keeps the raw u32 visible for callers that need to
    // round-trip a value exceeding `i64::MAX` or compare bytewise
    // against a separately-emitted writer's stamp.
    for (i, len_opt) in stream_lengths.iter().enumerate() {
        if let Some(n) = len_opt {
            metadata.push((format!("avi:strh.{i}.length"), n.to_string()));
        }
    }

    // Round-247: AVI 1.0 §"AVISTREAMHEADER" `dwFlags` field at byte
    // offset 8 of the strh. Surfaced as
    // `avi:strh.<index>.flags = "0xXXXXXXXX"` (upper-case 8-hex) for
    // every stream whose strh declared a non-zero flag DWORD per the
    // `dwFlags` row in
    // `docs/container/riff/avi-riff-file-reference.md` (line 237) +
    // the spec's *dwFlags values* table at lines 252–255
    // (`AVISF_DISABLED` `0x0000_0001` + `AVISF_VIDEO_PALCHANGES`
    // `0x0001_0000`). The `0` legacy default is omitted so an
    // unspecified flag DWORD reads the same as an absent one,
    // mirroring the round-229 `length` / round-222 `sample_size` /
    // round-217 `suggested_buffer_size` / round-210 `handler` /
    // round-203 `start` / round-182 `priority` / round-176 `quality`
    // / round-153 `initial_frames` / round-119 `language` / round-115
    // `frame_rect` conventions. The raw 32-bit value is also reachable
    // via the typed [`AviDemuxer::stream_flags`] /
    // [`AviDemuxer::stream_flags_typed`] accessors. Hex-string
    // rendering mirrors the file-global `avi:flags` key (`avih.dwFlags`,
    // round-10) so callers walking metadata pairs see consistent
    // formatting between the two flag DWORDs.
    for (i, flags_opt) in stream_flags.iter().enumerate() {
        if let Some(bits) = flags_opt {
            metadata.push((format!("avi:strh.{i}.flags"), format!("0x{bits:08X}")));
        }
    }

    // Round-249: AVI 1.0 §"AVISTREAMHEADER" `dwScale` + `dwRate`
    // DWORDs at byte offsets 20 and 24 of the strh. Surfaced as
    // `avi:strh.<index>.scale = <N>` and `avi:strh.<index>.rate = <N>`
    // (decimal u32) for every stream whose strh declared both DWORDs
    // non-zero per the `dwScale` row in
    // `docs/container/riff/avi-riff-file-reference.md` (line 241) +
    // the `dwRate` row (line 242). The "either zero" sentinel is
    // omitted so an unspecified pair reads the same as an absent one,
    // mirroring the round-247 `flags` / round-229 `length` /
    // round-222 `sample_size` / round-217 `suggested_buffer_size` /
    // round-210 `handler` / round-203 `start` / round-182 `priority`
    // / round-176 `quality` / round-153 `initial_frames` / round-119
    // `language` / round-115 `frame_rect` conventions. The two raw
    // u32s are also reachable via the typed
    // [`AviDemuxer::stream_timebase`] accessor. Decimal rendering
    // matches the `avi:streams.<n>.<rate|scale>` convention rather
    // than the hex-string `avi:strh.<n>.flags` since these are
    // numeric magnitudes, not bit fields.
    for (i, rate_opt) in stream_rates.iter().enumerate() {
        if let Some((scale, rate)) = rate_opt {
            metadata.push((format!("avi:strh.{i}.scale"), scale.to_string()));
            metadata.push((format!("avi:strh.{i}.rate"), rate.to_string()));
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
    // Round-285: `rec ` LIST entries recorded in idx1 (AVI 1.0 §"AVI
    // Index Entries": idx1 carries "entries for each data chunk,
    // including 'rec ' chunks"). Collected by `build_idx_table` while
    // it walks the raw entries; empty when no idx1 or no rec entries.
    let mut idx1_rec_entries: Vec<Idx1RecEntry> = Vec::new();
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
        let (table, recs) = build_idx_table(&mut *input, &raw, movi_start, &streams)?;
        idx1_rec_entries = recs;
        table
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
    // Round-285: surface the `rec ` LIST entry count from idx1 so
    // callers walking `Demuxer::metadata()` can detect CD-ROM-style
    // `LIST rec ` interleave grouping without the typed accessor.
    // Omitted entirely when zero so absence stays observable.
    if !idx1_rec_entries.is_empty() {
        metadata.push((
            "avi:idx1.rec_lists".into(),
            idx1_rec_entries.len().to_string(),
        ));
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

    // Round-317: surface an `ix##` standard-index whose `qwBaseOffset`
    // anchors outside every `movi` LIST region. Per AVISTDINDEX
    // (`docs/container/riff/avi-riff-file-reference.md` Appendix G,
    // `qwBaseOffset` row: *"Base offset (typically the file offset of the
    // 'movi' list)."*) the base every entry resolves against is expected
    // to point inside the enclosing `movi`. When it doesn't, emit
    // `avi:ix.<stream>.<segment>.base_outside_movi = "<qwBaseOffset>"` so
    // a downstream tool can flag the malformed anchor even if seek still
    // limps along on the verbatim value. The well-formed in-`movi` case
    // emits no key, so absence stays observable, mirroring the
    // "default == absent" convention used across the other `avi:ix.*` /
    // `avi:indx.*` keys. Mirrors the typed
    // [`AviDemuxer::std_index_base_offset_violations`] surface.
    {
        let mut seg_per_stream: std::collections::BTreeMap<u32, usize> =
            std::collections::BTreeMap::new();
        for ix in &std_indexes {
            let stream = match parse_stream_index(&ix.chunk_id) {
                Some(s) => s,
                None => continue,
            };
            let seg = seg_per_stream.entry(stream).or_default();
            let segment_index = *seg;
            *seg += 1;
            let inside_movi = movi_segments
                .iter()
                .any(|&(start, end)| ix.qw_base_offset >= start && ix.qw_base_offset < end);
            if !inside_movi {
                metadata.push((
                    format!("avi:ix.{stream}.{segment_index}.base_outside_movi"),
                    ix.qw_base_offset.to_string(),
                ));
            }
        }
    }

    // Round-322: surface an `ix##` standard index whose body `dwChunkId`
    // declares a *different* stream than the `ix##` chunk's own RIFF
    // FourCC. Per AVISTDINDEX (`docs/container/riff/avi-riff-file-reference.md`
    // Appendix G, `dwChunkId` row: *"FOURCC of indexed chunks."*) the
    // standard-index body's `dwChunkId` names the `movi` data-chunk FourCC
    // every entry points at, so for a well-formed file its two leading
    // ASCII digits encode the same stream the `ix##` chunk itself was
    // emitted for (e.g. an `ix01` chunk indexes `01dc` / `01wb`). When the
    // two disagree the standard index is cross-wired — its entries resolve
    // into another stream's chunks — and emitting
    // `avi:ix.<stream>.<segment>.chunk_id = "<FOURCC>"` lets a downstream
    // repair tool flag it even though the seek path keeps using the
    // verbatim `dwChunkId`-derived stream. The canonical own-slot value is
    // skipped so absence stays observable, mirroring the round-312
    // super-index `avi:indx.<n>.chunk_id` "default == absent" convention.
    // The per-segment ordinal counts in file order across every `ix##`
    // for the stream, matching the `base_outside_movi` key above and the
    // typed [`AviDemuxer::std_index_chunk_ids`] surface.
    {
        let mut seg_per_stream: std::collections::BTreeMap<u32, usize> =
            std::collections::BTreeMap::new();
        for ix in &std_indexes {
            let stream = match parse_stream_index(&ix.chunk_id) {
                Some(s) => s,
                None => continue,
            };
            let seg = seg_per_stream.entry(stream).or_default();
            let segment_index = *seg;
            *seg += 1;
            // The `ix##` chunk's own FourCC carries the stream digits at
            // bytes [2..4] (`ix00` → stream 0); `dwChunkId` carries them
            // at [0..2] (`00dc` → stream 0). A divergence means the body
            // declares a different stream than the chunk header.
            let own_stream = parse_stream_index(&[ix.own_fourcc[2], ix.own_fourcc[3], 0, 0]);
            if let Some(os) = own_stream {
                if os != stream {
                    metadata.push((
                        format!("avi:ix.{os}.{segment_index}.chunk_id"),
                        format_fourcc_or_hex(&ix.chunk_id),
                    ));
                }
            }
        }
    }

    // Round-325: surface an `ix##` standard index whose declared
    // `nEntriesInUse` exceeds the number of entries the demuxer could
    // physically parse from its (truncated) body. Per AVISTDINDEX
    // (`docs/container/riff/avi-riff-file-reference.md` Appendix G / the
    // base AVIMETAINDEX in Appendix E, `nEntriesInUse` row: *"Number of
    // valid entries in adwIndex."*) a well-formed chunk holds exactly
    // `nEntriesInUse` entries; a truncated capture crash-dump can stamp a
    // larger count. The demuxer keeps the entries it could read and emits
    // `avi:ix.<stream>.<segment>.declared_entries = "<declared>/<parsed>"`
    // so a downstream repair tool can flag the loss even though seek limps
    // along on the entries that survived. The well-formed (declared ==
    // parsed) case emits no key, so absence stays observable, mirroring the
    // "default == absent" convention across the other `avi:ix.*` keys.
    // Mirrors the typed
    // [`AviDemuxer::std_index_entry_count_violations`] surface.
    {
        let mut seg_per_stream: std::collections::BTreeMap<u32, usize> =
            std::collections::BTreeMap::new();
        for ix in &std_indexes {
            let stream = match parse_stream_index(&ix.chunk_id) {
                Some(s) => s,
                None => continue,
            };
            let seg = seg_per_stream.entry(stream).or_default();
            let segment_index = *seg;
            *seg += 1;
            let parsed = ix.entries.len() as u32;
            if ix.declared_n_entries > parsed {
                metadata.push((
                    format!("avi:ix.{stream}.{segment_index}.declared_entries"),
                    format!("{}/{}", ix.declared_n_entries, parsed),
                ));
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

    // Round-197: surface the `indx` super-index's own `bIndexSubType`
    // byte. Per the AVISUPERINDEX layout in
    // `docs/container/riff/avi-riff-file-reference.md` Appendix F
    // (`bIndexSubType` field, line 366: *"0 (default)"*) and the
    // §"AVISUPERINDEX" companion in Appendix E (`bIndexSubType` line
    // 329: *"Index sub-type (e.g., AVI_INDEX_SUB_2FIELD)"*), the
    // super-index inherits the sub-type of the per-segment `ix##`
    // standard indexes it points at — so `AVI_INDEX_SUB_2FIELD` on
    // the super-index is the reader-facing signal that every
    // pointed-to segment's `ix##` will carry 2-field-interlaced
    // entries (12-byte `(dwOffset, dwSize, dwOffsetField2)` records
    // instead of the default 8-byte `(dwOffset, dwSize)` pair). The
    // muxer already stamps this byte under `with_field2_stream(0)`
    // for stream 0's super-index (see round-4 P1
    // `opendml_field2_super_index_inherits_subtype`); this surfaces
    // the parsed value verbatim on the demuxer side so a reader can
    // detect the 2-field declaration from the `strl`-level super
    // index *before* having to scan into the `movi` body for the
    // first `ix##` chunk. Skip the `0` default so absence of the key
    // stays observable, mirroring the round-176/153/119/115/107
    // "default == absent" convention. Skip empty super-indexes so a
    // stream that never declared an `indx` produces no key (otherwise
    // the post-pad to `streams.len()` zero-slots would emit spurious
    // entries).
    for (i, sx) in super_indexes.iter().enumerate() {
        if sx.entries.is_empty() {
            continue;
        }
        if sx.b_index_sub_type == AVI_INDEX_SUB_2FIELD {
            metadata.push((format!("avi:indx.{i}.sub_type_2field"), "true".into()));
        }
    }

    // Round-304: surface the `indx` super-index's own `wLongsPerEntry`
    // WORD. Per the AVISUPERINDEX layout in
    // `docs/container/riff/avi-riff-file-reference.md` Appendix F
    // (`wLongsPerEntry` row: *"4 (each entry is 16 bytes)."*) and the
    // base AVIMETAINDEX in Appendix E (`wLongsPerEntry` row: *"Size of
    // each index entry, in 4-byte units."*), this WORD is the
    // super-index's per-entry stride in 4-byte DWORD units. For a
    // well-formed AVI 2.0 super-index it is always `4` — each
    // `_avisuperindex_entry` is `(qwOffset, dwSize, dwDuration)` = 16
    // bytes = 4 longs. Skip the spec-default `4` so absence of the key
    // stays observable (the typed `super_index_longs_per_entry`
    // accessor returns the raw value verbatim either way), mirroring
    // the round-197 `sub_type_2field` / round-176/153 "default ==
    // absent" convention. Skip empty super-indexes so the post-pad to
    // `streams.len()` zero-slots produce no spurious keys.
    for (i, sx) in super_indexes.iter().enumerate() {
        if sx.entries.is_empty() {
            continue;
        }
        if sx.w_longs_per_entry != 4 {
            metadata.push((
                format!("avi:indx.{i}.longs_per_entry"),
                sx.w_longs_per_entry.to_string(),
            ));
        }
    }

    // Round-312: surface the `indx` super-index's own `dwChunkId`
    // FOURCC. Per the AVISUPERINDEX layout in
    // `docs/container/riff/avi-riff-file-reference.md` Appendix F
    // (`dwChunkId` row: *"FOURCC of chunks indexed (e.g., '00dc')."*)
    // and the base AVIMETAINDEX in Appendix E (`dwChunkId` row: *"FOURCC
    // of chunks indexed (e.g., '00dc'); for super index only."*), this
    // DWORD declares which `movi` data-chunk FOURCC every `ix##` segment
    // referenced by this super-index points at. For a well-formed AVI
    // 2.0 file it spells the stream's own packet FourCC — `00dc` /
    // `00wb` for stream 0, `01dc` / `01wb` for stream 1, and so on — so
    // the canonical value is fully redundant with the stream slot the
    // super-index lives in. We surface it verbatim only when the parsed
    // FOURCC's two leading ASCII stream-digits do NOT decode to the
    // super-index's own stream slot `i`: a divergent `dwChunkId` means
    // the super-index declares it indexes a *different* stream's chunks
    // than the `strl` it sits in (a malformed / cross-wired file), and
    // surfacing the raw FOURCC lets a downstream repair tool detect that
    // before trusting the `ix##` scan. The canonical-matching value is
    // skipped so absence of the key stays observable, mirroring the
    // round-304 `longs_per_entry` / round-197 `sub_type_2field` "default
    // == absent" convention. Skip empty super-indexes so the post-pad to
    // `streams.len()` zero-slots produce no spurious keys; the all-zero
    // chunk_id of a `default()` slot is therefore never emitted.
    for (i, sx) in super_indexes.iter().enumerate() {
        if sx.entries.is_empty() {
            continue;
        }
        let declares_own_slot = parse_stream_index(&sx.chunk_id) == Some(i as u32);
        if !declares_own_slot {
            metadata.push((
                format!("avi:indx.{i}.chunk_id"),
                format_fourcc_or_hex(&sx.chunk_id),
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

    // Round-17 candidate 4: idx1 ↔ ix## cross-validator.
    //
    // Both indexes describe the same packet stream (per OpenDML 2.0
    // §"Index Locations": ix## entries and idx1 entries within the
    // primary segment must agree on (offset, length) per packet),
    // but real-world capture-card files sometimes ship a stale
    // idx1 — recovered from a crash, rebuilt by a non-conformant
    // tool, or copied from a different cut of the file — that
    // disagrees with the truth in ix##. The OpenDML spec is
    // explicit that ix## is more reliable when both are present
    // (idx1 offsets are 32-bit and primary-segment-only, ix## is
    // 64-bit and per-segment), so the canonical fix is to surface
    // the disagreement under metadata and let downstream code
    // prefer ix## as the source of truth.
    //
    // Crucially: idx1 only covers the PRIMARY segment per AVI 1.0
    // §3.4 (its 32-bit offsets can't reach into a continuation
    // RIFF AVIX), so we compare idx1 against ONLY the std_indexes
    // whose `qw_base_offset` falls within the primary segment's
    // `movi_segments[0]` byte range. For multi-segment OpenDML
    // files this naturally drops the AVIX continuation indexes
    // from the comparison; for single-segment OpenDML it's a no-op
    // (every std_index belongs to the primary).
    //
    // We walk per-stream idx1 entries in file order and compare
    // them against the per-stream primary-segment ix## entries
    // (also in file order). For each ordinal we compare
    // (file-absolute header offset, payload size). Only the first
    // mismatch per stream is surfaced — callers want the one
    // diagnostic line per stream, not a spam of every divergent
    // entry. Per `parse_ix_chunk` semantics: `dw_offset` is the
    // offset from `qw_base_offset` to chunk DATA (8 B past the
    // header), so subtract 8 to recover the header offset that
    // matches `IdxEntry::offset`.
    if !idx_table.is_empty() && !std_indexes.is_empty() {
        let primary_range = movi_segments.first().copied();
        for (s_idx, _) in streams.iter().enumerate() {
            let stream_id = s_idx as u32;
            let idx1_for_stream: Vec<(u64, u32)> = idx_table
                .iter()
                .filter(|e| e.stream == stream_id)
                .map(|e| (e.offset, e.size))
                .collect();
            if idx1_for_stream.is_empty() {
                continue;
            }
            let mut ix_for_stream: Vec<(u64, u32)> = Vec::new();
            for ix in &std_indexes {
                let ix_stream = match parse_stream_index(&ix.chunk_id) {
                    Some(s) => s,
                    None => continue,
                };
                if ix_stream != stream_id {
                    continue;
                }
                // Drop any std_index whose qw_base_offset falls
                // outside the primary `movi` segment's byte range.
                // idx1 can't address that data, so the comparison
                // is meaningless.
                if let Some((p_start, p_end)) = primary_range {
                    if ix.qw_base_offset < p_start || ix.qw_base_offset >= p_end {
                        continue;
                    }
                }
                for entry in &ix.entries {
                    let header_off = ix
                        .qw_base_offset
                        .saturating_add(entry.dw_offset as u64)
                        .saturating_sub(8);
                    ix_for_stream.push((header_off, entry.dw_size));
                }
            }
            if ix_for_stream.is_empty() {
                continue;
            }
            let common = idx1_for_stream.len().min(ix_for_stream.len());
            let mut divergent_at: Option<usize> = None;
            for i in 0..common {
                if idx1_for_stream[i] != ix_for_stream[i] {
                    divergent_at = Some(i);
                    break;
                }
            }
            // Length mismatch is itself a divergence even when
            // the shared prefix matched — record it at index `common`.
            if divergent_at.is_none() && idx1_for_stream.len() != ix_for_stream.len() {
                divergent_at = Some(common);
            }
            if let Some(seq) = divergent_at {
                let (a_off, a_size) = idx1_for_stream
                    .get(seq)
                    .copied()
                    .unwrap_or((u64::MAX, u32::MAX));
                let (b_off, b_size) = ix_for_stream
                    .get(seq)
                    .copied()
                    .unwrap_or((u64::MAX, u32::MAX));
                // Round-18 candidate 3: strict mode promotes the
                // lenient `avi:idx1.<n>.divergent_offsets` metadata
                // key into a hard `Error::InvalidData` so callers
                // wanting fail-fast (validate-then-ship pipelines,
                // strict players) abort instead of surfacing the
                // metadata. The lenient `open_avi` path still pushes
                // metadata as before; only `open_avi_strict` flips
                // this flag.
                if strict_cross_validate {
                    return Err(Error::invalid(format!(
                        "AVI: idx1↔ix## offset divergence at seq={seq} \
                         on stream {stream_id}: \
                         idx1=offset_{a_off}_size_{a_size} \
                         ix##=offset_{b_off}_size_{b_size}"
                    )));
                }
                metadata.push((
                    format!("avi:idx1.{stream_id}.divergent_offsets"),
                    format!(
                        "seq={seq} idx1=offset_{a_off}_size_{a_size} \
                         ix##=offset_{b_off}_size_{b_size}"
                    ),
                ));
            }
        }
    }

    // Round-15 candidate 1: surface a "stamped dwMaxBytesPerSec is
    // smaller than the per-stream demand" warning under the
    // `avi:over_budget` metadata key. Per AVI 1.0 §3.1 the field is
    // the approximate maximum data rate; a capture-card player that
    // sized its disk-read pacing budget from this value will under-
    // allocate when the real demand exceeds it. We compute the
    // expected demand as `sum(audio.avg_bytes_per_sec) +
    // computed_video_bytes_per_sec`, where the audio sum comes from
    // each `auds` stream's parsed WAVEFORMATEX `nAvgBytesPerSec`
    // (preserved on `params.bit_rate / 8` by `build_stream`) and the
    // video bytes-per-sec comes from `sum(idx1 entry sizes for vids
    // streams) * 1_000_000 / duration_micros`. Skip silently when:
    //   - avih is missing,
    //   - dwMaxBytesPerSec is 0 (writer didn't bother — there's no
    //     stamped value to compare against),
    //   - duration_micros is 0 (no usable per-frame timing for the
    //     video bitrate term — false positives outweigh signal),
    //   - the file has no idx1 (the video bitrate term is 0 and
    //     audio-only pacing is already exact in the populator).
    if let Some(h) = &avih {
        if h.max_bytes_per_sec > 0 && duration_micros > 0 && !idx_table.is_empty() {
            // Audio: pull `avg_bytes_per_sec` per stream off
            // `params.bit_rate` (which `build_stream` already set
            // from the parsed WAVEFORMATEX's `nAvgBytesPerSec * 8`).
            let audio_sum: u64 = streams
                .iter()
                .filter(|s| matches!(s.params.media_type, MediaType::Audio))
                .filter_map(|s| s.params.bit_rate)
                .map(|br| br / 8)
                .sum();
            // Video: sum of idx1 entry sizes for video streams,
            // converted to bytes/sec via the file duration.
            let mut video_bytes: u64 = 0;
            for e in &idx_table {
                let s = e.stream as usize;
                if let Some(stream) = streams.get(s) {
                    if matches!(stream.params.media_type, MediaType::Video) {
                        video_bytes = video_bytes.saturating_add(e.size as u64);
                    }
                }
            }
            // bytes_per_sec = video_bytes * 1_000_000 / micros.
            let video_bps = if duration_micros > 0 {
                let big = (video_bytes as u128) * 1_000_000u128;
                let bps = big / (duration_micros as u128);
                bps.min(u32::MAX as u128) as u64
            } else {
                0
            };
            let expected = audio_sum.saturating_add(video_bps);
            if expected > h.max_bytes_per_sec as u64 {
                metadata.push((
                    "avi:over_budget".into(),
                    format!(
                        "expected_max={} stamped={}",
                        expected.min(u32::MAX as u64),
                        h.max_bytes_per_sec
                    ),
                ));
            }
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
        idx1_rec_entries,
        super_indexes,
        std_indexes,
        audio_cbr_block_aligns,
        idx1_flags_per_stream,
        palette_change_counts,
        text_chunk_counts,
        avih_flags: avih.as_ref().map(|h| h.flags).unwrap_or(0),
        avih_suggested_buffer_size: avih.as_ref().map(|h| h.suggested_buffer_size).unwrap_or(0),
        avih_padding_granularity: avih.as_ref().map(|h| h.padding_granularity).unwrap_or(0),
        avih_initial_frames: avih.as_ref().map(|h| h.initial_frames).unwrap_or(0),
        avih_micro_sec_per_frame: avih.as_ref().map(|h| h.micro_sec_per_frame).unwrap_or(0),
        avih_max_bytes_per_sec: avih.as_ref().map(|h| h.max_bytes_per_sec).unwrap_or(0),
        avih_total_frames: avih.as_ref().map(|h| h.total_frames).unwrap_or(0),
        avih_streams: avih.as_ref().map(|h| h.streams).unwrap_or(0),
        avih_width: avih.as_ref().map(|h| h.width).unwrap_or(0),
        avih_height: avih.as_ref().map(|h| h.height).unwrap_or(0),
        avih_reserved: avih.as_ref().map(|h| h.reserved).unwrap_or([0; 4]),
        vprps,
        dmlh_total_frames,
        palette_change_data,
        text_chunk_data,
        sideband_data_loaded,
        video_strf: video_strfs,
        audio_strf: audio_strfs,
        stream_names,
        stream_header_data,
        stream_frame_rects,
        stream_languages,
        stream_initial_frames,
        stream_qualities,
        stream_priorities,
        stream_starts,
        stream_handlers,
        stream_suggested_buffer_sizes,
        stream_sample_sizes,
        stream_lengths,
        stream_flags,
        stream_rates,
        stream_fcc_types,
        digitization_date,
        smpte_timecode,
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
    video_strfs: &mut Vec<Option<VideoStrfInfo>>,
    audio_strfs: &mut Vec<Option<AudioStrfInfo>>,
    stream_names: &mut Vec<Option<String>>,
    stream_header_data: &mut Vec<Option<Vec<u8>>>,
    stream_frame_rects: &mut Vec<Option<[i16; 4]>>,
    stream_languages: &mut Vec<Option<u16>>,
    stream_initial_frames: &mut Vec<Option<u32>>,
    stream_qualities: &mut Vec<Option<u32>>,
    stream_priorities: &mut Vec<Option<u16>>,
    stream_starts: &mut Vec<Option<u32>>,
    stream_handlers: &mut Vec<Option<[u8; 4]>>,
    stream_suggested_buffer_sizes: &mut Vec<Option<u32>>,
    stream_sample_sizes: &mut Vec<Option<u32>>,
    stream_lengths: &mut Vec<Option<u32>>,
    stream_flags: &mut Vec<Option<u32>>,
    stream_rates: &mut Vec<Option<(u32, u32)>>,
    stream_fcc_types: &mut Vec<Option<[u8; 4]>>,
    digitization_date: &mut Option<String>,
    smpte_timecode: &mut Option<String>,
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
                    let (
                        main,
                        stream_infos,
                        suffixes,
                        sxs,
                        vps,
                        dmlh,
                        info_md,
                        ais,
                        vss,
                        asfs,
                        names,
                        strds,
                        rcframes,
                        langs,
                        initial_frames_vec,
                        qualities_vec,
                        priorities_vec,
                        starts_vec,
                        handlers_vec,
                        suggested_buffer_sizes_vec,
                        sample_sizes_vec,
                        lengths_vec,
                        flags_vec,
                        rates_vec,
                        fcc_types_vec,
                        idit,
                        ismp,
                    ) = parse_hdrl(input, body_end, codecs)?;
                    *avih = Some(main);
                    *streams = stream_infos;
                    *packet_chunk_suffix = suffixes;
                    *super_indexes = sxs;
                    *vprps = vps;
                    *dmlh_total_frames = dmlh;
                    metadata.extend(info_md);
                    *audio_infos = ais;
                    *video_strfs = vss;
                    *audio_strfs = asfs;
                    *stream_names = names;
                    *stream_header_data = strds;
                    *stream_frame_rects = rcframes;
                    *stream_languages = langs;
                    *stream_initial_frames = initial_frames_vec;
                    *stream_qualities = qualities_vec;
                    *stream_priorities = priorities_vec;
                    *stream_starts = starts_vec;
                    *stream_handlers = handlers_vec;
                    *stream_suggested_buffer_sizes = suggested_buffer_sizes_vec;
                    *stream_sample_sizes = sample_sizes_vec;
                    *stream_lengths = lengths_vec;
                    *stream_flags = flags_vec;
                    *stream_rates = rates_vec;
                    *stream_fcc_types = fcc_types_vec;
                    *digitization_date = idit;
                    *smpte_timecode = ismp;
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
    /// `dwInitialFrames` per AVI 1.0 §"AVIMAINHEADER" (offset 16 of
    /// the body, i.e. byte 20 of the chunk including the 4-byte cb).
    /// Round-157: the file-global counterpart of the per-stream
    /// [`crate::demuxer::AviDemuxer::stream_initial_frames`] DWORD
    /// (round-153). The spec: *"Initial frame for interleaved files.
    /// Noninterleaved files should specify zero. If creating
    /// interleaved files, specify the number of frames in the file
    /// prior to the initial frame of the AVI sequence."* Captured
    /// here so [`AviDemuxer::initial_frames`] can surface it without
    /// re-parsing the file.
    initial_frames: u32,
    streams: u32,
    suggested_buffer_size: u32,
    width: u32,
    height: u32,
    /// `dwPaddingGranularity` per AVI 1.0 §"AVIMAINHEADER" (offset 8).
    /// Round-92: captured so [`AviDemuxer::padding_granularity`] can
    /// surface the alignment value the muxer used. Zero (the legacy
    /// sentinel) means "no alignment guarantee" — files predating the
    /// stream-aligned remux path leave this 0.
    padding_granularity: u32,
    /// `dwReserved[4]` per AVI 1.0 §"AVIMAINHEADER" (offsets 40..56 of
    /// the 56-byte body). Round-330: the spec pins this trailing array
    /// as *"Reserved. Set this array to zero."* A spec-conformant
    /// writer leaves all four DWORDs `0`; this field captures whatever
    /// the file actually stamped so
    /// [`AviDemuxer::avih_reserved`] can surface a non-conformant
    /// writer (a hand-edited / capture-card / vendor-extended header
    /// that smuggled data into the reserved slot) for forensic / repair
    /// callers. Defaults to `[0; 4]` for a short (`< 56`-byte) body so
    /// an absent array reads the same as the conformant all-zero one.
    reserved: [u32; 4],
}

/// Parse the AVIMAINHEADER body (should be 56 bytes).
fn parse_avih(buf: &[u8]) -> Result<AviMainHeader> {
    if buf.len() < 40 {
        return Err(Error::invalid("AVI: avih too short"));
    }
    Ok(AviMainHeader {
        micro_sec_per_frame: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
        max_bytes_per_sec: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        // dwPaddingGranularity (offset 8) — round-92 captures this.
        padding_granularity: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        flags: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        total_frames: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
        initial_frames: u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
        streams: u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]),
        suggested_buffer_size: u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]),
        width: u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
        height: u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]),
        // dwReserved[4] (offsets 40..56) — round-330. Read only when
        // the body is the full 56 bytes; a short body (some capture-card
        // crash dumps stamp a truncated 40-byte avih) leaves the array
        // all-zero, matching the spec-conformant default so an absent
        // array is indistinguishable from a zeroed one.
        reserved: if buf.len() >= 56 {
            [
                u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]),
                u32::from_le_bytes([buf[44], buf[45], buf[46], buf[47]]),
                u32::from_le_bytes([buf[48], buf[49], buf[50], buf[51]]),
                u32::from_le_bytes([buf[52], buf[53], buf[54], buf[55]]),
            ]
        } else {
            [0; 4]
        },
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
/// `None` for video / data streams. `audio_strfs` mirrors that
/// parallel-by-index pattern for the WAVEFORMATEX(TENSIBLE)-derived
/// audio-strf side-info captured at parse time.
type HdrlOutput = (
    AviMainHeader,
    Vec<StreamInfo>,
    Vec<[u8; 2]>,
    Vec<SuperIndex>,
    Vec<VprpHeader>,
    Option<u32>,
    Vec<(String, String)>,
    Vec<Option<AudioStrhInfo>>,
    Vec<Option<VideoStrfInfo>>,
    Vec<Option<AudioStrfInfo>>,
    Vec<Option<String>>,
    Vec<Option<Vec<u8>>>,
    Vec<Option<[i16; 4]>>,
    Vec<Option<u16>>,
    Vec<Option<u32>>,
    Vec<Option<u32>>,
    Vec<Option<u16>>,
    Vec<Option<u32>>,
    Vec<Option<[u8; 4]>>,
    Vec<Option<u32>>,
    Vec<Option<u32>>,
    Vec<Option<u32>>,
    // Round-247: per-stream `strh.dwFlags` raw u32s, parallel to
    // `streams`. `None` for the `0` legacy "no flags set" default.
    Vec<Option<u32>>,
    // Round-249: per-stream `(strh.dwScale, strh.dwRate)` raw timebase
    // pair, parallel to `streams`. `None` for the writer-skips-it
    // sentinel where either DWORD is zero.
    Vec<Option<(u32, u32)>>,
    // Round-253: per-stream `strh.fccType` raw FOURCC, parallel to
    // `streams`. `None` for the all-zero `[0, 0, 0, 0]` sentinel.
    Vec<Option<[u8; 4]>>,
    Option<String>,
    Option<String>,
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
    let mut video_strfs: Vec<Option<VideoStrfInfo>> = Vec::new();
    let mut audio_strfs: Vec<Option<AudioStrfInfo>> = Vec::new();
    let mut stream_names: Vec<Option<String>> = Vec::new();
    let mut stream_header_data: Vec<Option<Vec<u8>>> = Vec::new();
    let mut stream_frame_rects: Vec<Option<[i16; 4]>> = Vec::new();
    let mut stream_languages: Vec<Option<u16>> = Vec::new();
    let mut stream_initial_frames: Vec<Option<u32>> = Vec::new();
    let mut stream_qualities: Vec<Option<u32>> = Vec::new();
    let mut stream_priorities: Vec<Option<u16>> = Vec::new();
    let mut stream_starts: Vec<Option<u32>> = Vec::new();
    let mut stream_handlers: Vec<Option<[u8; 4]>> = Vec::new();
    // Round-217: per-stream `strh.dwSuggestedBufferSize` hint (raw u32
    // at byte offset 36 of each AVISTREAMHEADER). Spec-documented `0`
    // "do not know" sentinel maps to `None`.
    let mut stream_suggested_buffer_sizes: Vec<Option<u32>> = Vec::new();
    // Round-222: per-stream `strh.dwSampleSize` (raw u32 at byte offset
    // 44 of each AVISTREAMHEADER). The spec-documented `0` "samples can
    // vary in size" sentinel maps to `None` so an unspecified hint reads
    // the same as an absent one — mirroring the round-217
    // `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    // `dwStart` etc. "default == absent" convention.
    let mut stream_sample_sizes: Vec<Option<u32>> = Vec::new();
    // Round-229: per-stream `strh.dwLength` (raw u32 at byte offset 32
    // of each AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER", `dwLength`
    // row). The `0` "no length declared" value maps to `None` so an
    // empty / unspecified stream reads the same as an absent one —
    // mirroring the round-222 `dwSampleSize` / round-217
    // `dwSuggestedBufferSize` / round-210 `fccHandler` "default ==
    // absent" convention.
    let mut stream_lengths: Vec<Option<u32>> = Vec::new();
    // Round-247: per-stream `strh.dwFlags` (raw u32 at byte offset 8 of
    // each AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER", `dwFlags`
    // row + the spec's *dwFlags values* table at lines 252–255). The `0`
    // "no flags set" value maps to `None` so an unspecified flag field
    // reads the same as an absent one — mirroring the round-229
    // `dwLength` / round-222 `dwSampleSize` / round-217
    // `dwSuggestedBufferSize` / round-210 `fccHandler` "default ==
    // absent" convention.
    let mut stream_flags: Vec<Option<u32>> = Vec::new();
    // Round-249: per-stream `(strh.dwScale, strh.dwRate)` raw timebase
    // pair captured from byte offsets 20 + 24 of each AVISTREAMHEADER
    // per AVI 1.0 §"AVISTREAMHEADER" (`dwScale` row line 241 + `dwRate`
    // row line 242). The writer-skips-it `(0, 0)` (or either-zero)
    // sentinel maps to `None` so the absent / degenerate case reads
    // the same as a default one — mirroring the precedent set by the
    // round-247 `dwFlags` / round-229 `dwLength` / round-222
    // `dwSampleSize` etc. "default == absent" convention. Both DWORDs
    // are surfaced verbatim when present; the internal `time_base`
    // derivation still applies `.max(1)` separately to keep the
    // decoder functional on degenerate files.
    let mut stream_rates: Vec<Option<(u32, u32)>> = Vec::new();
    // Round-253: per-stream `strh.fccType` raw FOURCC captured verbatim
    // from byte offset 0 of each AVISTREAMHEADER per AVI 1.0
    // §"AVISTREAMHEADER" (`fccType` row at line 235 + the `fcc` row at
    // line 234 documenting the standard `auds` / `mids` / `txts` /
    // `vids` values). The all-zero `[0, 0, 0, 0]` sentinel maps to
    // `None` so an absent / writer-skipped type reads the same as a
    // default one — mirroring the round-249 `(dwScale, dwRate)` /
    // round-247 `dwFlags` / round-229 `dwLength` "default == absent"
    // convention. Non-standard FOURCCs outside the spec's
    // `{auds, mids, txts, vids}` set are surfaced verbatim for the
    // caller to interpret.
    let mut stream_fcc_types: Vec<Option<[u8; 4]>> = Vec::new();
    let mut dmlh_total_frames: Option<u32> = None;
    let mut info_metadata: Vec<(String, String)> = Vec::new();
    let mut digitization_date: Option<String> = None;
    let mut smpte_timecode: Option<String> = None;

    while r.stream_position()? < end_pos {
        let hdr = match read_chunk_header(r)? {
            Some(h) => h,
            None => break,
        };
        match &hdr.id {
            // Round-107: `IDIT` digitization-date chunk, a direct child
            // of `LIST hdrl` (a sibling of `avih` / `strl` / `LIST odml`
            // / `LIST INFO`). Per the RIFF *Hdrl Tags* namespace
            // (`docs/container/riff/metadata/exiftool-riff-tags.html`
            // §"RIFF Hdrl Tags": `'IDIT'` → `DateTimeOriginal`) it
            // carries the capture / digitization timestamp as a text
            // string. The on-disk text format is writer-defined and not
            // pinned by the staged docs (capture hardware commonly emits
            // a C `asctime`-style "Wed Jan 02 02:03:55 2002" while other
            // tools use ISO-8601), so the body is surfaced verbatim as a
            // trimmed UTF-8-lossy string and left for the caller to
            // interpret — exactly the conservative treatment the `strn`
            // stream-name chunk gets.
            b"IDIT" => {
                let body = read_body_bounded(r, hdr.size)?;
                digitization_date = parse_idit_body(&body);
                skip_pad(r, hdr.size)?;
            }
            // Round-112: `ISMP` SMPTE-timecode chunk, the other direct
            // child of `LIST hdrl` documented alongside `IDIT` in the
            // RIFF *Hdrl Tags* namespace
            // (`docs/container/riff/metadata/exiftool-riff-tags.html`
            // §"RIFF Hdrl Tags": `'ISMP'` → `TimeCode`). The on-disk
            // text format is writer-defined and not pinned by the
            // staged docs (capture filters commonly emit the SMPTE
            // "HH:MM:SS:FF" or "HH:MM:SS;FF" drop-frame form while
            // other tools use a fractional "HH:MM:SS.ss"); the body is
            // surfaced verbatim as a trimmed UTF-8-lossy string and
            // left for the caller to interpret — the same conservative
            // treatment `IDIT` and the per-stream `strn` chunks get.
            b"ISMP" => {
                let body = read_body_bounded(r, hdr.size)?;
                smpte_timecode = parse_ismp_body(&body);
                skip_pad(r, hdr.size)?;
            }
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
                    let (
                        si,
                        suf,
                        sx,
                        vp,
                        ai,
                        vs,
                        asi,
                        name,
                        strd_bytes,
                        rc_frame,
                        lang,
                        initial_frames,
                        quality,
                        priority,
                        start,
                        handler,
                        suggested_buffer_size,
                        sample_size,
                        length,
                        flags,
                        rate_scale,
                        fcc_type,
                    ) = parse_strl(r, body_end, streams.len() as u32, codecs)?;
                    if let Some(si) = si {
                        streams.push(si);
                        suffixes.push(suf.unwrap_or(*b"xx"));
                        super_indexes.push(sx);
                        vprps.push(vp);
                        audio_infos.push(ai);
                        video_strfs.push(vs);
                        audio_strfs.push(asi);
                        stream_names.push(name);
                        stream_header_data.push(strd_bytes);
                        stream_frame_rects.push(rc_frame);
                        stream_languages.push(lang);
                        stream_initial_frames.push(initial_frames);
                        stream_qualities.push(quality);
                        stream_priorities.push(priority);
                        stream_starts.push(start);
                        stream_handlers.push(handler);
                        stream_suggested_buffer_sizes.push(suggested_buffer_size);
                        stream_sample_sizes.push(sample_size);
                        stream_lengths.push(length);
                        stream_flags.push(flags);
                        stream_rates.push(rate_scale);
                        stream_fcc_types.push(fcc_type);
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
        video_strfs,
        audio_strfs,
        stream_names,
        stream_header_data,
        stream_frame_rects,
        stream_languages,
        stream_initial_frames,
        stream_qualities,
        stream_priorities,
        stream_starts,
        stream_handlers,
        stream_suggested_buffer_sizes,
        stream_sample_sizes,
        stream_lengths,
        stream_flags,
        stream_rates,
        stream_fcc_types,
        digitization_date,
        smpte_timecode,
    ))
}

/// Parse an `IDIT` chunk body into an owned digitization-date string
/// (round-107).
///
/// `IDIT` lives directly inside `LIST hdrl` and carries the capture /
/// digitization timestamp as text per the RIFF *Hdrl Tags* namespace
/// (`docs/container/riff/metadata/exiftool-riff-tags.html` §"RIFF Hdrl
/// Tags": `'IDIT'` → `DateTimeOriginal`). The staged docs do not pin a
/// canonical byte format for the field, so this parser is deliberately
/// format-agnostic: it strips trailing NUL / whitespace bytes (capture
/// hardware frequently terminates the C `asctime` form with a `'\n'`
/// and/or a NUL, and pads to a WORD boundary with extra NULs) and
/// passes the remainder through `String::from_utf8_lossy` so legacy
/// Latin-1 / CP1252 bytes don't fail the parse. An empty payload
/// (`cb=0`) — or a body that is all NUL / whitespace — yields `None` so
/// "no IDIT chunk" stays distinguishable from "an IDIT chunk with a
/// non-empty timestamp".
fn parse_idit_body(body: &[u8]) -> Option<String> {
    // Strip every trailing NUL and ASCII whitespace byte (space, tab,
    // CR, LF, form-feed, vertical-tab) — asctime-style writers append a
    // newline + NUL; the RIFF WORD-pad may add a further NUL.
    let end = body
        .iter()
        .rposition(|&b| b != 0 && !b.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(0);
    if end == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&body[..end]).into_owned())
}

/// Parse an `ISMP` chunk body into an owned SMPTE-timecode string
/// (round-112).
///
/// `ISMP` is the sibling of [`parse_idit_body`]'s `IDIT` in the RIFF
/// *Hdrl Tags* namespace: both are documented as direct children of
/// `LIST hdrl` in
/// `docs/container/riff/metadata/exiftool-riff-tags.html` §"RIFF Hdrl
/// Tags" (`'ISMP'` → `TimeCode`). The staged docs do not pin a canonical
/// byte format for the SMPTE timecode either — capture pipelines emit
/// either the colon form (`"HH:MM:SS:FF"` non-drop-frame), the
/// semicolon form (`"HH:MM:SS;FF"` drop-frame), or a fractional
/// `"HH:MM:SS.ss"` — so this parser mirrors `parse_idit_body`'s
/// conservative treatment: strip trailing NUL / ASCII-whitespace bytes
/// (covering both writer-appended `'\n'` / NUL terminators and the
/// RIFF WORD-pad NUL) and pass the remainder through
/// `String::from_utf8_lossy` so legacy Latin-1 / CP1252 bytes don't
/// fail the parse. An empty payload (`cb=0`) — or a body that is all
/// NUL / whitespace — yields `None` so "no ISMP chunk" stays
/// distinguishable from "an ISMP chunk with a non-empty timecode".
fn parse_ismp_body(body: &[u8]) -> Option<String> {
    let end = body
        .iter()
        .rposition(|&b| b != 0 && !b.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(0);
    if end == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&body[..end]).into_owned())
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

/// 7-tuple returned by [`parse_strl`]: optional [`StreamInfo`],
/// optional packet-chunk suffix, [`SuperIndex`] (default-empty when
/// no `indx`), [`VprpHeader`] (default when no `vprp`), the
/// audio-stream's `(format_tag, sample_size)` pair when the strh
/// declared `fccType == "auds"` (round-14 C2 — used by the VBR/CBR
/// validator at `open_avi`), the video-stream's BMIH-derived
/// side-info ([`VideoStrfInfo`]) when the strh declared
/// `fccType == "vids"` (round-19 C1+C2), the audio-stream's
/// WAVEFORMATEX(TENSIBLE)-derived side-info ([`AudioStrfInfo`])
/// (round-75), and the optional per-stream name captured from the
/// `strn` chunk per AVI 1.0 §"AVI Stream Headers" (round-80; `None`
/// when the strl had no `strn` chunk).
type StrlOutput = (
    Option<StreamInfo>,
    Option<[u8; 2]>,
    SuperIndex,
    VprpHeader,
    Option<AudioStrhInfo>,
    Option<VideoStrfInfo>,
    Option<AudioStrfInfo>,
    Option<String>,
    Option<Vec<u8>>,
    Option<[i16; 4]>,
    Option<u16>,
    Option<u32>,
    Option<u32>,
    Option<u16>,
    Option<u32>,
    Option<[u8; 4]>,
    Option<u32>,
    Option<u32>,
    Option<u32>,
    // Round-247: `strh.dwFlags` raw u32.
    Option<u32>,
    // Round-249: `(strh.dwScale, strh.dwRate)` raw timebase pair.
    Option<(u32, u32)>,
    // Round-253: `strh.fccType` raw FOURCC (`None` for the all-zero
    // sentinel).
    Option<[u8; 4]>,
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
    let mut strn_name: Option<String> = None;
    let mut strd_bytes: Option<Vec<u8>> = None;
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
            b"strn" => {
                // AVI 1.0 §"AVI Stream Headers": optional null-terminated
                // text string describing the stream. Per Microsoft Learn,
                // the encoding is unspecified; we round-trip as UTF-8 and
                // fall back to lossy decoding for byte sequences that
                // aren't valid UTF-8 (legacy capture tools occasionally
                // write Latin-1 / CP1252 stream names). The trailing NUL
                // is stripped; an empty payload (cb=0) is treated as
                // absent so it doesn't surface a phantom name.
                let body = read_body_bounded(r, hdr.size)?;
                skip_pad(r, hdr.size)?;
                strn_name = parse_strn_body(&body);
            }
            b"strd" => {
                // AVI 1.0 §"AVI Stream Headers" (round-89): optional
                // codec-driver configuration blob. Per Microsoft Learn:
                // "If the stream-header data ('strd') chunk is present,
                // it follows the stream format chunk. The format and
                // content of this chunk are defined by the codec
                // driver. Typically, drivers use this information for
                // configuration. Applications that read and write AVI
                // files do not need to interpret this information; they
                // simple transfer it to and from the driver as a memory
                // block." The demuxer preserves the raw bytes verbatim
                // (no interpretation) so a re-mux can hand them back to
                // the same codec driver. An empty payload (`cb=0`) is
                // captured as `Some(Vec::new())` so absence of the
                // chunk stays distinguishable from an empty driver
                // blob. A duplicate `strd` overwrites — the spec
                // allows at most one per `strl`.
                let body = read_body_bounded(r, hdr.size)?;
                skip_pad(r, hdr.size)?;
                strd_bytes = Some(body);
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
        None => {
            return Ok((
                None,
                None,
                super_index,
                vprp,
                None,
                None,
                None,
                strn_name,
                strd_bytes,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                // Round-249: per-stream (dwScale, dwRate) absent when
                // the strl had no strh chunk.
                None,
                // Round-253: per-stream `strh.fccType` absent when the
                // strl had no strh chunk.
                None,
            ));
        }
    };
    let strf = strf_buf.unwrap_or_default();
    let parsed = build_stream(index, &strh, &strf, codecs)?;
    Ok((
        Some(parsed.0),
        Some(parsed.1),
        super_index,
        vprp,
        parsed.2,
        parsed.3,
        parsed.4,
        strn_name,
        strd_bytes,
        parsed.5,
        parsed.6,
        parsed.7,
        parsed.8,
        parsed.9,
        parsed.10,
        parsed.11,
        parsed.12,
        parsed.13,
        parsed.14,
        parsed.15,
        parsed.16,
        parsed.17,
    ))
}

/// Parse a `strn` chunk body into an owned `String`.
///
/// Per AVI 1.0 §"AVI Stream Headers" the body is a null-terminated text
/// string. The trailing NUL is stripped; bytes that aren't valid UTF-8
/// are passed through `String::from_utf8_lossy` so legacy Latin-1 /
/// CP1252 stream names don't fail the parse. An empty payload (`cb=0`)
/// is treated as "no name" so downstream code can distinguish absent
/// vs. empty-string names cleanly.
fn parse_strn_body(body: &[u8]) -> Option<String> {
    // Strip every trailing NUL (some writers pad with multiple NULs to a
    // WORD boundary).
    let end = body
        .iter()
        .rposition(|&b| b != 0)
        .map(|p| p + 1)
        .unwrap_or(0);
    if end == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&body[..end]).into_owned())
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
        // Retain every slot within `nEntriesInUse`. Per OpenDML 2.0
        // §"AVI Super Index Chunk" the spec marks `qwOffset == 0` as the
        // "unused entry" sentinel, but unused slots live *beyond*
        // `nEntriesInUse` (writers preallocate a fixed table and bump
        // `nEntriesInUse` only for filled slots), so a zero offset
        // *within* the used range is a real segment — most commonly the
        // primary `RIFF AVI ` segment, which starts at file offset 0 and
        // therefore has `qwOffset == 0` when a writer (this crate's muxer
        // included) records the segment's RIFF offset. We keep its
        // `dwDuration` so [`AviDemuxer::super_index_segment_durations`]
        // and the round-101 cross-check see the complete per-segment
        // partition. Seeking never dereferences `qw_offset` (the in-`movi`
        // `ix##` scan in `scan_ix_in_movi` resolves the real chunk
        // locations — see [`SuperIndexEntry`]), so retaining a zero-offset
        // entry is inert for the seek path. Round-101.
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
fn parse_ix_chunk(own_fourcc: [u8; 4], body: &[u8]) -> Option<StdIndex> {
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
    let declared_n_entries = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    let n_entries_in_use = declared_n_entries as usize;
    let mut chunk_id = [0u8; 4];
    chunk_id.copy_from_slice(&body[8..12]);
    let qw_base_offset = u64::from_le_bytes([
        body[12], body[13], body[14], body[15], body[16], body[17], body[18], body[19],
    ]);
    // Round-325: tolerate a truncated entry table rather than discarding
    // the whole `ix##` chunk. The 24-byte AVISTDINDEX header (already
    // validated above) carries the declared `nEntriesInUse` regardless of
    // how many entries actually fit in the body, so we parse as many
    // complete entries as the body holds and retain `declared_n_entries`
    // verbatim. A short-read leaves `entries.len() < declared_n_entries`,
    // which [`AviDemuxer::std_index_entry_count_violations`] surfaces as a
    // truncation cross-check — the previous behaviour silently dropped the
    // chunk and lost the declared count entirely. The seek path only ever
    // dereferences the entries it actually parsed, so a partial table stays
    // safe.
    let available_entries = body.len().saturating_sub(24) / entry_size;
    let parse_count = n_entries_in_use.min(available_entries);
    let mut entries = Vec::with_capacity(parse_count);
    for i in 0..parse_count {
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
        own_fourcc,
        chunk_id,
        qw_base_offset,
        b_index_sub_type,
        declared_n_entries,
        entries,
    })
}

/// Walk the per-segment `movi` LIST scanning for `ix##` AVISTDINDEX
/// chunks. Used for OpenDML 2.0 random-access seek when no `idx1`
/// table is present (typical for files segmented by modern AVI
/// writers that cap RIFF size). Returns the parsed std-index per
/// `ix##` chunk found (each maps back to one stream via the `##`
/// ASCII digits in its FourCC).
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
                    if let Some(idx) = parse_ix_chunk(hdr.id, &b) {
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
    /// `WAVEFORMATEX.nBlockAlign` — the byte size of one sample block
    /// (`nChannels * wBitsPerSample / 8` for PCM). For CBR audio every
    /// indexed data chunk must contain a whole number of these blocks,
    /// which the round-96 `ix##` standard-index validator
    /// ([`AviDemuxer::cbr_audio_block_alignment_violations`]) checks.
    /// Zero when the stream carried no parsable WAVEFORMATEX.
    pub block_align: u16,
}

/// Per-audio-stream `strf` (WAVEFORMATEX / WAVEFORMATEXTENSIBLE)
/// decoded side-data the demuxer captures at `open()` for callers
/// that need surround channel mask, 24-in-32 container vs valid bits
/// disambiguation, or the SubFormat GUID without re-parsing
/// extradata. Round-75 — pairs with [`VideoStrfInfo`] for audio
/// streams.
///
/// One entry per audio stream (parallel to [`AviDemuxer::streams`]
/// for `media_type == Audio`); video / data streams have `None`. For
/// audio streams whose `wFormatTag` was NOT
/// [`crate::stream_format::WAVE_FORMAT_EXTENSIBLE`] (`0xFFFE`), the
/// extensible-only fields are `None` (the legacy `WAVEFORMATEX`
/// shape doesn't carry channel mask / SubFormat GUID); only the
/// `wFormatTag` field is always populated.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioStrfInfo {
    /// On-wire `wFormatTag` value. Always set; equal to
    /// [`crate::stream_format::WAVE_FORMAT_EXTENSIBLE`] (`0xFFFE`)
    /// for extensible streams and to the legacy `WAVE_FORMAT_*`
    /// constant otherwise.
    pub format_tag: u16,
    /// `WAVEFORMATEXTENSIBLE.Samples.wValidBitsPerSample` for
    /// extensible streams — the actual sample precision, which may
    /// be less than the container size carried in
    /// `WAVEFORMATEX.wBitsPerSample` (the canonical example is 24-bit
    /// PCM in a 32-bit container). `None` for legacy
    /// `WAVEFORMATEX` streams (caller should fall back to the
    /// `WAVEFORMATEX.wBitsPerSample` value).
    pub valid_bits_per_sample: Option<u16>,
    /// `WAVEFORMATEXTENSIBLE.dwChannelMask` — `SPEAKER_*` bitmap
    /// per Microsoft `mmreg.h` / Microsoft Learn § "Channel-mask
    /// channel ordering". `None` for legacy streams (the spec
    /// pre-dated explicit speaker assignment so the order is
    /// channel-count-dependent).
    pub channel_mask: Option<u32>,
    /// `WAVEFORMATEXTENSIBLE.SubFormat` GUID — the actual codec
    /// identifier when the legacy `wFormatTag` is the
    /// `WAVE_FORMAT_EXTENSIBLE` escape hatch. `None` for non-
    /// extensible streams.
    pub subformat: Option<Guid>,
}

/// One CBR-audio `ix##` standard-index entry whose `dwSize` is not a
/// whole multiple of the stream's `WAVEFORMATEX.nBlockAlign`
/// (round-96).
///
/// Per OpenDML 2.0 §3.0 ("AVI Standard Index Chunk") each
/// `AVISTDINDEX_ENTRY.dwSize` is the byte length of the indexed data
/// chunk. For a constant-bit-rate audio stream (PCM / A-law / µ-law /
/// IMA-ADPCM) every data chunk holds a whole number of `nBlockAlign`
/// sample blocks, so `dwSize % nBlockAlign == 0` must hold. A nonzero
/// remainder means the index points at a partial sample block — the
/// file's index disagrees with its own WAVEFORMATEX and a consumer
/// that trusts the index will mis-frame the audio.
///
/// Returned (possibly empty) by
/// [`AviDemuxer::cbr_audio_block_alignment_violations`]. The
/// validator is purely informational — it never fails `open()` (the
/// AVI 1.0 sample-size invariant at `open()` already rejects the
/// gross VBR/CBR mismatch; this is a finer, index-level check callers
/// opt into).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockAlignViolation {
    /// Stream number (from the two ASCII digits of the `ix##` chunk's
    /// `dwChunkId`, e.g. `01wb` ⇒ stream 1).
    pub stream_index: u32,
    /// Zero-based position of the offending entry within that stream's
    /// `ix##` standard-index entries, counted in file order across
    /// every `ix##` chunk that indexes this stream.
    pub entry_index: usize,
    /// The entry's `dwSize` (keyframe bit already cleared) — the
    /// indexed data chunk's declared byte length.
    pub dw_size: u32,
    /// The stream's `WAVEFORMATEX.nBlockAlign` the size was checked
    /// against.
    pub block_align: u16,
}

/// One super-index whose per-segment `dwDuration` total disagrees with
/// the file's `dmlh.dwTotalFrames` (round-101).
///
/// Per OpenDML 2.0 §"AVI Super Index Chunk" each `_avisuperindex_entry`
/// carries a `dwDuration` field — *"time span in stream ticks"* — for
/// the chunks indexed by the `ix##` standard index that entry points
/// at. The same spec's §5.0 ("Extended AVI Header") defines
/// `dmlh.dwTotalFrames` as *"the real size of the AVI file"* — the
/// total frame count across **every** `RIFF AVIX` segment (whereas
/// `avih.dwTotalFrames` counts only the primary segment). Because the
/// super-index entries partition the file segment-by-segment, the sum
/// of a one-tick-per-frame video stream's per-segment `dwDuration`
/// values must equal `dmlh.dwTotalFrames`. A mismatch means the
/// super-index disagrees with the extended header about how many frames
/// the file holds — a consumer that trusts one for total-duration math
/// and the other for per-segment seeking will land off by the
/// difference.
///
/// Returned (possibly empty) by
/// [`AviDemuxer::super_index_duration_violations`]. The validator is
/// purely informational — it never fails `open()`, and it only fires
/// for video streams that carry **both** a non-empty `indx` super-index
/// and a `dmlh` extended header (the only configuration where the two
/// counts are independently recorded and therefore comparable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuperIndexDurationViolation {
    /// Stream number whose `indx` super-index was checked.
    pub stream_index: u32,
    /// Sum of the stream's per-segment `_avisuperindex_entry.dwDuration`
    /// values (saturating at `u64::MAX`).
    pub super_index_duration_total: u64,
    /// The file's `dmlh.dwTotalFrames` the total was compared against.
    pub dmlh_total_frames: u64,
}

/// One `ix##` standard-index whose `qwBaseOffset` does not anchor
/// inside any of the file's `movi` LIST regions (round-317).
///
/// Per AVISTDINDEX (clean-room source:
/// `docs/container/riff/avi-riff-file-reference.md` Appendix G,
/// `qwBaseOffset` row: *"Base offset (typically the file offset of the
/// 'movi' list)."*) every `AVISTDINDEX_ENTRY.dwOffset` is added to the
/// chunk-level `qwBaseOffset` to recover the file-absolute position of
/// the indexed data — so `qwBaseOffset` is expected to point at (or just
/// inside) the enclosing `movi` LIST. A `qwBaseOffset` that falls
/// outside every `movi` segment range means the standard index's
/// base anchor disagrees with where the file actually stored its data
/// chunks; a consumer that trusts the `dwOffset`-from-base arithmetic
/// for seeking will compute positions that miss the real chunk headers.
///
/// Returned (possibly empty) by
/// [`AviDemuxer::std_index_base_offset_violations`]. The validator is
/// purely informational — it never affects `open()`, and the demuxer's
/// own OpenDML seek path resolves offsets from the verbatim
/// `qwBaseOffset` regardless (so a malformed base still round-trips
/// observably rather than being silently "corrected"). It complements
/// the per-segment `dwChunkId` / `dwDuration` surfaces with a
/// position-level sanity check between the index and the `movi` body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StdIndexBaseOffsetViolation {
    /// Stream number (from the two ASCII digits of the `ix##` chunk's
    /// `dwChunkId`, e.g. `01wb` ⇒ stream 1).
    pub stream_index: u32,
    /// Zero-based position of the offending `ix##` chunk within that
    /// stream's standard indexes, counted in file order across every
    /// `ix##` chunk that indexes this stream (one `ix##` per `(stream,
    /// movi segment)` pair for OpenDML files).
    pub segment_index: usize,
    /// The chunk's verbatim `qwBaseOffset` that landed outside every
    /// `movi` LIST region.
    pub qw_base_offset: u64,
}

/// One truncated `ix##` standard-index chunk surfaced by
/// [`AviDemuxer::std_index_entry_count_violations`] (round-325).
///
/// Per AVISTDINDEX (clean-room source:
/// `docs/container/riff/avi-riff-file-reference.md` Appendix G / the base
/// AVIMETAINDEX in Appendix E) the `nEntriesInUse` DWORD declares how many
/// `AVISTDINDEX_ENTRY` records the `ix##` chunk holds. A well-formed chunk
/// carries exactly that many 8- (or 12-byte 2-field) entries; a truncated
/// capture crash-dump or hand-edited file can stamp `nEntriesInUse = N`
/// while the chunk body only physically contains `M < N` entries. The
/// demuxer parses the `M` entries it can read and reports the
/// `(declared, parsed)` pair here so a downstream repair tool can detect
/// the loss. Informational only — never fails `open()`, and the OpenDML
/// seek path uses just the entries it actually parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StdIndexEntryCountViolation {
    /// Stream number (from the two ASCII digits of the `ix##` chunk's
    /// `dwChunkId`, e.g. `01wb` ⇒ stream 1).
    pub stream_index: u32,
    /// Zero-based position of the offending `ix##` chunk within that
    /// stream's standard indexes, counted in file order (one `ix##` per
    /// `(stream, movi segment)` pair for OpenDML files). Matches the
    /// `segment_index` of [`StdIndexBaseOffsetViolation`].
    pub segment_index: usize,
    /// The chunk header's verbatim `nEntriesInUse`.
    pub declared_entries: u32,
    /// The number of complete entries the demuxer could physically read
    /// from the (truncated) body. Always `< declared_entries` for a
    /// reported violation.
    pub parsed_entries: u32,
}

/// 8-tuple returned by [`build_stream`]: the [`StreamInfo`], the
/// 2-byte chunk suffix (`dc` / `db` / `wb` / `xx`), the
/// audio-strh `(format_tag, sample_size)` pair (`Some` for audio
/// streams only — round-14 C2), the video-strf BMIH side-info
/// (`Some` for video streams only — round-19 C1+C2), the
/// audio-strf WAVEFORMATEX(TENSIBLE) side-info (`Some` for audio
/// streams only — round-75 WAVEFORMATEXTENSIBLE landing), the
/// `strh.rcFrame` destination rectangle `[left, top, right, bottom]`
/// (`Some` when non-zero — round-115; the all-zero default is mapped
/// to `None` so an unspecified rect reads the same as an absent one),
/// the `strh.wLanguage` LANGID (`Some` when non-zero — round-119;
/// `0` is the documented "unspecified" sentinel and surfaces as `None`
/// so the absence stays observable), and the `strh.dwInitialFrames`
/// skew (`Some` when non-zero — round-153; `0` is the documented
/// "noninterleaved file" sentinel per AVIMAINHEADER §`dwInitialFrames`
/// and surfaces as `None` so an unspecified skew reads the same as an
/// absent one).
type BuildStreamOutput = (
    StreamInfo,
    [u8; 2],
    Option<AudioStrhInfo>,
    Option<VideoStrfInfo>,
    Option<AudioStrfInfo>,
    Option<[i16; 4]>,
    Option<u16>,
    Option<u32>,
    Option<u32>,
    Option<u16>,
    Option<u32>,
    Option<[u8; 4]>,
    Option<u32>,
    Option<u32>,
    Option<u32>,
    // Round-247: `strh.dwFlags` raw u32 (`None` for the `0` "no flags
    // set" legacy writer default; non-zero values surface verbatim).
    Option<u32>,
    // Round-249: `(strh.dwScale, strh.dwRate)` raw timebase pair
    // captured from byte offsets 20 + 24 of the AVISTREAMHEADER.
    // `None` when either DWORD is zero (the writer-skips-it /
    // mathematically-undefined `rate/scale` ratio).
    Option<(u32, u32)>,
    // Round-253: `strh.fccType` raw FOURCC captured from byte offset
    // 0 of the AVISTREAMHEADER. `None` when the 4 bytes were all
    // zero (the writer-skips-it sentinel mirroring the round-247
    // `dwFlags` / round-229 `dwLength` etc. "default == absent"
    // convention).
    Option<[u8; 4]>,
);

fn build_stream(
    index: u32,
    strh: &[u8],
    strf: &[u8],
    codecs: &dyn CodecResolver,
) -> Result<BuildStreamOutput> {
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
    // Round-249: raw `(dwScale, dwRate)` DWORDs from byte offsets 20
    // and 24 per AVI 1.0 §"AVISTREAMHEADER" (`dwScale` row line 241 +
    // `dwRate` row line 242: *"Used with dwRate to specify the time
    // scale that this stream will use. Dividing dwRate by dwScale gives
    // the number of samples per second. For video streams, this is the
    // frame rate. For audio streams, this rate corresponds to the time
    // needed to play nBlockAlign bytes of audio, which for PCM audio is
    // the just the sample rate."*). The raw values surface verbatim
    // for round-trip parity; the internal `time_base` derivation below
    // still applies a `.max(1)` clamp on each DWORD to keep degenerate
    // / zero-padded files decodable.
    let scale_raw = u32::from_le_bytes([strh[20], strh[21], strh[22], strh[23]]);
    let rate_raw = u32::from_le_bytes([strh[24], strh[25], strh[26], strh[27]]);
    let scale = scale_raw.max(1);
    let rate = rate_raw.max(1);
    let length = u32::from_le_bytes([strh[32], strh[33], strh[34], strh[35]]);
    let sample_size = u32::from_le_bytes([strh[44], strh[45], strh[46], strh[47]]);
    // Round-249: surface the raw `(scale, rate)` pair as `Some` only
    // when both DWORDs are non-zero. Either being zero is a
    // writer-skips-it / mathematically-undefined `rate/scale` ratio
    // and is mapped to `None` so an unspecified pair reads the same
    // as an absent one — mirroring the round-247 `dwFlags` / round-229
    // `dwLength` / round-222 `dwSampleSize` etc. "default == absent"
    // convention.
    let rate_scale: Option<(u32, u32)> = if scale_raw == 0 || rate_raw == 0 {
        None
    } else {
        Some((scale_raw, rate_raw))
    };

    // Round-253: surface the raw `fccType` FOURCC from byte offset 0
    // verbatim. The all-zero `[0, 0, 0, 0]` sentinel is the
    // writer-skips-it / "no declared type" default and maps to `None`
    // so an unspecified type reads the same as an absent one —
    // mirroring the round-249 `(dwScale, dwRate)` / round-247
    // `dwFlags` / round-229 `dwLength` "default == absent" convention.
    // Non-standard FOURCCs outside the spec's `{auds, mids, txts,
    // vids}` set are surfaced verbatim for the caller to interpret;
    // the demuxer's own internal codec classification (which switches
    // on `&fcc_type` for media-kind routing below) is independent of
    // this surface and remains free to fall through to the
    // `Unknown / Data` arm for non-standard types.
    let fcc_type_opt: Option<[u8; 4]> = if fcc_type == [0, 0, 0, 0] {
        None
    } else {
        Some(fcc_type)
    };

    // `wLanguage` (round-119): a 16-bit LANGID at byte offset 14 of the
    // AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    // (docs/container/riff/avi-riff-file-reference.md, `wLanguage` row):
    // *"Language tag (BCP 47 / RFC 1766 / similar; AVI does not normatively
    // pin a registry)."* Microsoft conventions populate this with a
    // Win32 LANGID — `LANG_PRIMARY << 0 | SUBLANG << 10` — while non-MS
    // writers may pack different values; the demuxer surfaces the raw
    // 16-bit DWORD verbatim and leaves interpretation to the caller. The
    // `0` ("LANG_NEUTRAL / SUBLANG_NEUTRAL", the default writer-skips-it
    // value) is mapped to `None` so an unspecified language reads the
    // same as an absent one — mirroring the round-115 `rcFrame` and
    // round-80 `strn` "default == absent" convention.
    let language_raw = u16::from_le_bytes([strh[14], strh[15]]);
    let language: Option<u16> = if language_raw == 0 {
        None
    } else {
        Some(language_raw)
    };

    // `wPriority` (round-182): a 16-bit DWORD at byte offset 12 of the
    // AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`wPriority` row
    // in `docs/container/riff/avi-riff-file-reference.md`, Appendix B
    // line 238): *"Priority of a stream type. For example, in a file
    // with multiple audio streams, the one with the highest priority
    // might be the default stream."* The spec describes the field as a
    // selection hint among same-`fccType` streams (the file with
    // several audio streams picking a default-playback one), not a
    // sortable global priority. The demuxer surfaces the raw 16-bit
    // DWORD verbatim and leaves the "what counts as highest" decision
    // to the caller — the spec does not normatively pin a value range
    // or a tie-break rule. `0` is the legacy writer default (the
    // muxer has stamped a zero priority since round-3) and maps to
    // `None` here so an unspecified priority reads the same as an
    // absent one, mirroring the round-119 `wLanguage` / round-153
    // `dwInitialFrames` / round-176 `dwQuality` / round-115 `rcFrame`
    // / round-80 `strn` / round-107 `IDIT` "default == absent"
    // convention.
    let priority_raw = u16::from_le_bytes([strh[12], strh[13]]);
    let priority: Option<u16> = if priority_raw == 0 {
        None
    } else {
        Some(priority_raw)
    };

    // `dwStart` (round-203): a u32 at byte offset 28 of the 56-byte
    // AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwStart` row in
    // `docs/container/riff/avi-riff-file-reference.md`, line 243):
    // *"Starting time for this stream. The units are defined by the
    // dwRate and dwScale members in the main file header. Usually, this
    // is zero, but it can specify a delay time for a stream that does
    // not start concurrently with the file."* The `0` value is the
    // documented "starts concurrently with the file" default (also the
    // muxer's own default since round-3), mapped here to `None` so an
    // unspecified start reads the same as an absent one, mirroring the
    // round-182 `wPriority` / round-176 `dwQuality` / round-153
    // `dwInitialFrames` / round-119 `wLanguage` / round-115 `rcFrame`
    // / round-80 `strn` / round-107 `IDIT` "default == absent"
    // convention. The unit is the stream's own `(dwRate / dwScale)`
    // tick (frames for video, samples-or-blocks for audio) and the
    // demuxer surfaces the raw u32 verbatim with no rate-conversion.
    let start_raw = u32::from_le_bytes([strh[28], strh[29], strh[30], strh[31]]);
    let start: Option<u32> = if start_raw == 0 {
        None
    } else {
        Some(start_raw)
    };

    // `dwInitialFrames` (round-153): a u32 at byte offset 16 of the
    // 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwInitialFrames`
    // row in `docs/container/riff/avi-riff-file-reference.md`): *"How
    // far audio data is skewed ahead of the video frames in
    // interleaved files. Typically, this is about 0.75 seconds. If
    // creating interleaved files, set the value of this member to the
    // number of frames in the file prior to the initial frame of the
    // AVI sequence in this member."* AVIMAINHEADER §`dwInitialFrames`
    // adds: *"Initial frame for interleaved files. Noninterleaved
    // files should specify zero."* — i.e. `0` is the documented
    // "unspecified / noninterleaved" sentinel, mapped here to `None`
    // so an absent skew reads the same as a default one (mirroring
    // the round-119 `wLanguage` / round-80 `strn` /  round-107 `IDIT`
    // "default == absent" convention). The demuxer surfaces the raw
    // u32 verbatim; the spec defines the unit as the stream's own
    // (`dwRate` / `dwScale`) tick rate, but the demuxer does not
    // convert.
    let initial_frames_raw = u32::from_le_bytes([strh[16], strh[17], strh[18], strh[19]]);
    let initial_frames: Option<u32> = if initial_frames_raw == 0 {
        None
    } else {
        Some(initial_frames_raw)
    };

    // `dwQuality` (round-176): a u32 at byte offset 40 of the 56-byte
    // AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwQuality` row in
    // `docs/container/riff/avi-riff-file-reference.md`, line 246):
    // *"Indicator of the quality of the data in the stream. Quality is
    // represented as a number between 0 and 10,000. For compressed data,
    // this typically represents the value of the quality parameter passed
    // to the compression software. If set to -1, drivers use the default
    // quality value."* The `-1` (`0xFFFF_FFFF` as u32) sentinel is the
    // documented "use default driver quality" marker — both the legacy
    // muxer default and what the spec text calls out as a special value —
    // mapped here to `None` so an unspecified quality reads the same as
    // an absent one, mirroring the round-153 `dwInitialFrames` /
    // round-119 `wLanguage` / round-115 `rcFrame` "default == absent"
    // convention. Values in the documented `[0, 10_000]` range surface
    // verbatim; anomalous out-of-range writers (e.g. capture tools that
    // stamp arbitrary u32 values like the high-precision quality scores
    // some legacy VfW drivers use) also surface verbatim — the demuxer
    // does not clamp or normalise.
    let quality_raw = u32::from_le_bytes([strh[40], strh[41], strh[42], strh[43]]);
    let quality: Option<u32> = if quality_raw == 0xFFFF_FFFF {
        None
    } else {
        Some(quality_raw)
    };

    // `rcFrame` (round-115): the AVISTREAMHEADER destination rectangle at
    // byte offset 48, four little-endian signed WORDs in
    // `[left, top, right, bottom]` order. Per AVI 1.0 §"AVISTREAMHEADER"
    // (docs/container/riff/avi-riff-file-reference.md, `rcFrame` row): the
    // "destination rectangle for a text or video stream within the movie
    // rectangle specified by the dwWidth and dwHeight members of the AVI
    // main header structure … used in support of multiple video streams …
    // Units for this member are pixels. The upper-left corner of the
    // destination rectangle is relative to the upper-left corner of the
    // movie rectangle." The full 56-byte header carries it; truncated
    // 48-byte headers (the minimum we accept above) simply leave it absent.
    // The canonical "whole movie rectangle" writer default is all-zero
    // (0,0,0,0) — mapped to `None` here so an unspecified rect reads the
    // same as an absent one, mirroring the round-80 `strn` / round-107
    // `IDIT` "empty == absent" convention.
    let rc_frame: Option<[i16; 4]> = if strh.len() >= 56 {
        let left = i16::from_le_bytes([strh[48], strh[49]]);
        let top = i16::from_le_bytes([strh[50], strh[51]]);
        let right = i16::from_le_bytes([strh[52], strh[53]]);
        let bottom = i16::from_le_bytes([strh[54], strh[55]]);
        if left == 0 && top == 0 && right == 0 && bottom == 0 {
            None
        } else {
            Some([left, top, right, bottom])
        }
    } else {
        None
    };

    // `dwFlags` (round-247): a u32 at byte offset 8 of the 56-byte
    // AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwFlags` row in
    // `docs/container/riff/avi-riff-file-reference.md`, line 237 + the
    // *dwFlags values* table at lines 252–255). Two bits are spec-
    // documented:
    // - `AVISF_DISABLED` (`0x0000_0001`): *"Indicates this stream
    //   should not be enabled by default."*
    // - `AVISF_VIDEO_PALCHANGES` (`0x0001_0000`): *"Indicates this
    //   video stream contains palette changes. This flag warns the
    //   playback software that it will need to animate the palette."*
    // The `0` "no flags set" value is the legacy writer default (the
    // muxer has stamped zero since round-3 and the wild's
    // disabled-by-default / palette-animating files are a minority);
    // it maps here to `None` so an unspecified flag field reads the
    // same as an absent one, mirroring the round-229 `dwLength` /
    // round-222 `dwSampleSize` / round-217 `dwSuggestedBufferSize` /
    // round-210 `fccHandler` / round-203 `dwStart` / round-182
    // `wPriority` / round-176 `dwQuality` / round-153
    // `dwInitialFrames` / round-119 `wLanguage` / round-115 `rcFrame`
    // "default == absent" convention. Non-zero values surface
    // verbatim — the demuxer does not mask bits outside the
    // documented set (some legacy capture filters pack driver-private
    // bits in the upper half-DWORD that the spec does not pin).
    let flags_raw = u32::from_le_bytes([strh[8], strh[9], strh[10], strh[11]]);
    let flags: Option<u32> = if flags_raw == 0 {
        None
    } else {
        Some(flags_raw)
    };

    // `fccHandler` (round-210): a 4-byte FourCC at byte offset 4 of
    // the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`fccHandler`
    // row in `docs/container/riff/avi-riff-file-reference.md`,
    // Appendix B line 236): *"An optional FOURCC that identifies a
    // specific data handler. The data handler is the preferred handler
    // for the stream. For audio and video streams, this specifies the
    // codec for decoding the stream."* The `\0\0\0\0` all-zero value
    // is the spec-aligned "no preferred handler" default (the
    // *optional* qualifier in the prose lines up with the legacy
    // writer practice of leaving audio-stream fccHandler zero); it
    // maps here to `None` so an unspecified driver hint reads the
    // same as an absent one, mirroring the round-203 `dwStart` /
    // round-182 `wPriority` / round-176 `dwQuality` / round-153
    // `dwInitialFrames` / round-119 `wLanguage` / round-115
    // `rcFrame` / round-80 `strn` / round-107 `IDIT` "default ==
    // absent" convention. Non-zero values surface verbatim — the
    // demuxer does NOT inspect or validate the bytes (capture
    // hardware writers in the wild sometimes pack non-printable
    // bytes here as a vendor-specific driver token, and the spec's
    // *optional FOURCC that identifies a specific data handler*
    // phrasing does not normatively pin printability).
    let handler: Option<[u8; 4]> = if fcc_handler == [0, 0, 0, 0] {
        None
    } else {
        Some(fcc_handler)
    };

    // `dwSuggestedBufferSize` (round-217): a u32 at byte offset 36 of
    // the 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    // (`dwSuggestedBufferSize` row in
    // `docs/container/riff/avi-riff-file-reference.md`, line 245):
    // *"How large a buffer should be used to read this stream.
    // Typically, this contains a value corresponding to the largest
    // chunk present in the stream. Using the correct buffer size makes
    // playback more efficient. Use zero if you do not know the correct
    // buffer size."* The `0` value is the spec-documented "do not know"
    // sentinel — mapped here to `None` so an unspecified hint reads the
    // same as an absent one, mirroring the round-210 `fccHandler` /
    // round-203 `dwStart` / round-182 `wPriority` / round-176
    // `dwQuality` / round-153 `dwInitialFrames` / round-119
    // `wLanguage` / round-115 `rcFrame` "default == absent" convention.
    // Non-zero values surface verbatim — the demuxer does not validate
    // against the actual largest chunk in `movi` (writers commonly
    // overestimate to bound read-ahead allocation; some legacy
    // capture tools under-declare a stream's largest chunk).
    let suggested_buffer_size_raw = u32::from_le_bytes([strh[36], strh[37], strh[38], strh[39]]);
    let suggested_buffer_size: Option<u32> = if suggested_buffer_size_raw == 0 {
        None
    } else {
        Some(suggested_buffer_size_raw)
    };

    // `dwSampleSize` (round-222): a u32 at byte offset 44 of the 56-byte
    // AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwSampleSize` row in
    // `docs/container/riff/avi-riff-file-reference.md`, line 247): *"The
    // size of a single sample of data. This is set to zero if the samples
    // can vary in size. If this number is nonzero, then multiple samples
    // of data can be grouped into a single chunk within the file. If it
    // is zero, each sample of data (such as a video frame) must be in a
    // separate chunk. For video streams, this number is typically zero,
    // although it can be nonzero if all video frames are the same size.
    // For audio streams, this number should be the same as the
    // nBlockAlign member of the WAVEFORMATEX structure describing the
    // audio."* The `0` value is the spec-documented "samples can vary in
    // size" sentinel — the dominant value for video streams (one frame
    // per chunk) and the required value for VBR audio (MP3 / AAC / MPEG)
    // — mapped here to `None` so an unspecified / variable-size hint
    // reads the same as an absent one, mirroring the round-217
    // `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    // `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    // round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
    // `rcFrame` "default == absent" convention. Non-zero values surface
    // verbatim — the demuxer does not validate against `WAVEFORMATEX
    // .nBlockAlign` (the round-14 C2 audio sample-size invariant in
    // `open_avi` covers VBR / CBR mismatches separately) nor against any
    // observed chunk-size pattern in `movi`. The 44 raw byte is already
    // captured for the `AudioStrhInfo` round-14 C2 validator as the
    // `sample_size: u32` field; this round adds the public per-stream
    // surface.
    let sample_size_opt: Option<u32> = if sample_size == 0 {
        None
    } else {
        Some(sample_size)
    };

    // `dwLength` (round-229): a u32 at byte offset 32 of the 56-byte
    // AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (`dwLength` row in
    // `docs/container/riff/avi-riff-file-reference.md`, line 244):
    // *"Length of this stream. The units are defined by the dwRate and
    // dwScale members of the stream's header."* The `0` value is the
    // de-facto "no length declared" marker — typical for half-written
    // capture dumps and the case the long-standing internal
    // `length > 0` duration guard already treated as absent — mapped
    // here to `None` so an unspecified length reads the same as an
    // absent one, mirroring the round-222 `dwSampleSize` / round-217
    // `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    // `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    // round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
    // `rcFrame` "default == absent" convention. Non-zero values surface
    // verbatim — the unit is the stream's own `(dwRate / dwScale)` tick
    // (frames for video, samples-or-blocks for audio per the existing
    // muxer derivation in `patch_post_counts`) and the demuxer does not
    // rate-convert. The 32 raw byte is already read above as the
    // `length` local used for `StreamInfo::duration`; this round adds
    // the public per-stream surface that keeps the raw u32 visible
    // separately from the framework's `Option<i64>` duration model.
    let length_opt: Option<u32> = if length == 0 { None } else { Some(length) };

    let mut audio_info: Option<AudioStrhInfo> = None;
    let mut video_strf_info: Option<VideoStrfInfo> = None;
    let mut audio_strf_info: Option<AudioStrfInfo> = None;
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
                // Round-19 C1+C2: surface BMIH-derived side-info
                // (top-down orientation per VfW §"biHeight sign
                // rules"; `BI_BITFIELDS` color masks per VfW
                // §"Color tables (palettes)"). Only `BI_BITFIELDS`
                // streams have meaningful R/G/B masks in extradata;
                // for other compression types the masks accessor
                // returns `None` and only `top_down` is filled.
                let bitfields_masks = if b.compression == crate::stream_format::BI_BITFIELDS {
                    crate::stream_format::parse_bitfields_masks(&b.extradata)
                } else {
                    None
                };
                video_strf_info = Some(VideoStrfInfo {
                    top_down: b.top_down,
                    bitfields_masks,
                });
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

            // Round-75: WAVEFORMATEXTENSIBLE (wFormatTag == 0xFFFE) —
            // when the `strf` payload carries the 22-byte extension,
            // pull `wValidBitsPerSample` / `dwChannelMask` / SubFormat
            // GUID off it. SubFormat is the canonical codec identity
            // when the legacy `wFormatTag` is the EXTENSIBLE escape
            // hatch per docs/container/riff/waveformatextensible/
            // README §"What's covered".
            let wfex = if format_tag == WAVE_FORMAT_EXTENSIBLE && !strf.is_empty() {
                Some(parse_waveformatextensible(strf)?)
            } else {
                None
            };

            let tag = CodecTag::wave_format(format_tag);
            let mut ctx = ProbeContext::new(&tag).header(strf);
            if let Some(w) = &wfx {
                ctx = ctx
                    .bits(w.bits_per_sample)
                    .channels(w.channels)
                    .sample_rate(w.samples_per_sec);
            }
            // Codec id resolution path:
            // 1. Try the codec registry against `CodecTag::wave_format(format_tag)`
            //    (legacy path). Registered codecs for `0xFFFE` could
            //    re-dispatch via `ctx.header` — we keep that surface
            //    so a future codec crate can claim the EXTENSIBLE tag.
            // 2. For extensible streams, prefer the SubFormat GUID's
            //    well-known mapping (PCM / IEEE_FLOAT / ALAW / MULAW
            //    / ADPCM / MPEG / DRM) over the synthetic
            //    `avi:tag_fffe` placeholder the legacy fallback would
            //    produce — the GUID is the actual codec id.
            // 3. Fall back to the legacy depth-aware
            //    `audio_codec_id_fallback`.
            let codec_id = codecs.resolve_tag(&ctx).unwrap_or_else(|| {
                if let Some(wfe) = &wfex {
                    // For depth-aware PCM/IEEE_FLOAT resolution, the
                    // SubFormat's `wValidBitsPerSample` is the actual
                    // codec precision (e.g. 24 for 24-in-32 PCM); fall
                    // back to the WAVEFORMATEX `wBitsPerSample`
                    // container size only when the union member is
                    // zero (writers in the wild sometimes leave it
                    // unset, in which case the container size is the
                    // best available proxy).
                    let depth = if wfe.valid_bits_per_sample > 0 {
                        wfe.valid_bits_per_sample
                    } else {
                        bits
                    };
                    if let Some(hint) = subformat_codec_hint(&wfe.subformat, depth) {
                        return CodecId::new(hint);
                    }
                    // Unknown SubFormat — synthesise an `avi:guid_<...>`
                    // id so downstream `make_decoder` lookup fails
                    // cleanly with a CodecNotFound naming the actual
                    // GUID rather than the opaque `0xFFFE` tag.
                    return CodecId::new(format!("avi:guid_{}", wfe.subformat.display()));
                }
                audio_codec_id_fallback(format_tag, bits)
            });
            let mut p = CodecParameters::audio(codec_id.clone());
            // Stamp the on-wire wFormatTag onto the params for
            // round-trip preservation.
            p.tag = Some(CodecTag::wave_format(format_tag));
            if let Some(w) = &wfx {
                p.channels = Some(w.channels);
                p.sample_rate = Some(w.samples_per_sec);
                p.extradata = w.extradata.clone();
                // For extensible streams, prefer the SubFormat's
                // wValidBitsPerSample for the sample-format hint —
                // matches what the underlying codec actually decodes
                // (24-in-32 PCM uses S24, not S32, even when the
                // container is 32 bits). Fall back to the container
                // size when the extension union is zero.
                let depth_for_format = wfex
                    .as_ref()
                    .map(|wfe| {
                        if wfe.valid_bits_per_sample > 0 {
                            wfe.valid_bits_per_sample
                        } else {
                            w.bits_per_sample
                        }
                    })
                    .unwrap_or(w.bits_per_sample);
                p.sample_format = sample_format_for(codec_id.as_str(), depth_for_format);
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
                // nBlockAlign from the parsed WAVEFORMATEX (0 when the
                // strf had no parsable WAVEFORMATEX — e.g. a 0-byte
                // strf). The round-96 ix## block-alignment validator
                // uses this for CBR streams.
                block_align: wfx.as_ref().map(|w| w.block_align).unwrap_or(0),
            });
            // Round-75: capture WAVEFORMATEX(TENSIBLE) side-info on
            // every audio stream so the demuxer can hand callers the
            // typed shape via [`AviDemuxer::stream_audio_strf`] /
            // [`AviDemuxer::stream_channel_mask`] /
            // [`AviDemuxer::stream_subformat`].
            audio_strf_info = Some(AudioStrfInfo {
                format_tag,
                valid_bits_per_sample: wfex.as_ref().map(|wfe| wfe.valid_bits_per_sample),
                channel_mask: wfex.as_ref().map(|wfe| wfe.channel_mask),
                subformat: wfex.as_ref().map(|wfe| wfe.subformat),
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
    Ok((
        stream,
        suffix,
        audio_info,
        video_strf_info,
        audio_strf_info,
        rc_frame,
        language,
        initial_frames,
        quality,
        priority,
        start,
        handler,
        suggested_buffer_size,
        sample_size_opt,
        length_opt,
        flags,
        rate_scale,
        fcc_type_opt,
    ))
}

/// Synthesise a placeholder `avi:<fourcc>` codec_id when the resolver
/// has no claim on the FourCC. Downstream `make_decoder` will return
/// `CodecNotFound` for these; the prefix lets callers tell "the codec
/// crate isn't wired in" apart from "the codec id is genuinely unknown".
/// Render a 4-byte FourCC for human-readable metadata output (round-210).
///
/// When every byte is in the `0x20..=0x7e` ASCII printable range,
/// returns the literal four-character string (so e.g. `MJPG` /
/// `iv32` / `DIB ` round-trip legibly). Otherwise returns an
/// `0xHHHHHHHH` lower-case hex form (8 hex digits prefixed with
/// `0x`) so a binary or out-of-band-byte handler hint stays
/// round-trippable without colliding with any printable ASCII
/// tag — matching the printable-vs-hex split this crate already
/// uses for unknown-FourCC palette / text / data side-band keys
/// (see the `key` branch in [`scan_idx1_for_sideband_summary`] and
/// the `avi_namespaced` branches around line 4935).
fn format_fourcc_or_hex(fourcc: &[u8; 4]) -> String {
    let printable = fourcc.iter().all(|&b| (0x20..=0x7e).contains(&b));
    if printable {
        // Safe: every byte is printable ASCII (so valid UTF-8).
        std::str::from_utf8(fourcc).unwrap().to_string()
    } else {
        format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            fourcc[0], fourcc[1], fourcc[2], fourcc[3]
        )
    }
}

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
    // Same offset-base probe as `build_idx_table` so the two stay in
    // sync — including the round-285 rule that non-per-stream entries
    // (`rec ` LIST entries, whose offset points at a `LIST` FourCC
    // rather than the recorded ckid) can't anchor the probe.
    let n = raw.len() / 16;
    let movi_fourcc_pos = movi_start.saturating_sub(4);
    let mut probe_raw_offset: Option<u32> = None;
    let mut probe_ckid: Option<[u8; 4]> = None;
    for i in 0..n {
        let base = i * 16;
        let mut ckid = [0u8; 4];
        ckid.copy_from_slice(&raw[base..base + 4]);
        if parse_stream_index(&ckid).is_none() {
            continue;
        }
        let off =
            u32::from_le_bytes([raw[base + 8], raw[base + 9], raw[base + 10], raw[base + 11]]);
        if off != 0 {
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
) -> Result<(Vec<IdxEntry>, Vec<Idx1RecEntry>)> {
    if raw.len() < 16 {
        return Ok((Vec::new(), Vec::new()));
    }
    let n = raw.len() / 16;
    // Pick the first per-stream entry with a non-zero offset as a probe.
    // Entries whose ckid doesn't name a per-stream data chunk (e.g. the
    // `rec ` LIST entries of AVI 1.0 §"AVI Index Entries") are skipped:
    // the bytes at a `rec ` entry's offset are the `LIST` FourCC, not
    // the recorded ckid, so they can never anchor the offset-base probe
    // and would silently force the conservative default below.
    let mut probe_raw_offset: Option<u32> = None;
    let mut probe_ckid: Option<[u8; 4]> = None;
    for i in 0..n {
        let base = i * 16;
        let mut ckid = [0u8; 4];
        ckid.copy_from_slice(&raw[base..base + 4]);
        if parse_stream_index(&ckid).is_none() {
            continue;
        }
        let off =
            u32::from_le_bytes([raw[base + 8], raw[base + 9], raw[base + 10], raw[base + 11]]);
        if off != 0 {
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
    // for unknown stream indexes (tolerate stray junk), but collect
    // `rec ` LIST entries separately — per AVI 1.0 §"AVI Index Entries"
    // idx1 carries "entries for each data chunk, including 'rec '
    // chunks", and those describe the grouping LISTs themselves rather
    // than any per-stream payload (Appendix C: `AVIIF_LIST` — "The
    // chunk is a 'rec ' list."). They carry no stream index, so they
    // never enter the per-stream seek table; the typed
    // [`AviDemuxer::idx1_rec_list_entries`] surface keeps them
    // observable verbatim.
    let mut entries: Vec<IdxEntry> = Vec::with_capacity(n);
    let mut rec_entries: Vec<Idx1RecEntry> = Vec::new();
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
            None => {
                if ckid == *b"rec " {
                    rec_entries.push(Idx1RecEntry {
                        flags,
                        offset: base_off.saturating_add(raw_off as u64),
                        size,
                    });
                }
                continue;
            }
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

    Ok((entries, rec_entries))
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

/// Per-video-stream `strf` (BITMAPINFOHEADER) decoded side-data the
/// demuxer captures at `open()` for callers that need orientation,
/// `BI_BITFIELDS` color-mask layouts, or other DIB-shape facts that
/// don't fit on [`oxideav_core::CodecParameters`]. Round-19 candidate
/// 1 + 2 per VfW `wingdi.h` §"biHeight sign rules" and
/// §"Color tables (palettes) / BI_BITFIELDS".
///
/// One entry per video stream (parallel to [`AviDemuxer::streams`]
/// for `media_type == Video`); audio / data streams have `None`.
#[derive(Clone, Debug, Default)]
pub struct VideoStrfInfo {
    /// `true` when the on-wire `biHeight` was negative ⇒ top-down
    /// DIB (origin upper-left) per VfW §"biHeight sign rules". Only
    /// semantically meaningful for `BI_RGB` and `BI_BITFIELDS`
    /// streams; YUV bitmaps are always top-down regardless of sign,
    /// and compressed FourCCs MUST use positive `biHeight`.
    pub top_down: bool,
    /// `(red_mask, green_mask, blue_mask)` for `BI_BITFIELDS`
    /// compression (16-bpp / 32-bpp uncompressed RGB whose
    /// channel layout is declared via the three trailing color
    /// masks). `None` when the stream isn't `BI_BITFIELDS` or the
    /// extradata was too short to carry the three DWORDs.
    pub bitfields_masks: Option<(u32, u32, u32)>,
}

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
    /// `rec ` LIST entries recorded in idx1, in file order (round-285).
    ///
    /// Per AVI 1.0 §"AVI Index Entries" the idx1 chunk holds "entries
    /// for each data chunk, including 'rec ' chunks"; per Appendix C
    /// the `AVIIF_LIST` flag marks an entry whose chunk "is a 'rec '
    /// list". These describe the grouping LISTs themselves, carry no
    /// stream index, and never enter [`Self::idx_table`]. Surfaced via
    /// [`AviDemuxer::idx1_rec_list_entries`] +
    /// `avi:idx1.rec_lists` metadata. Empty when the file has no idx1
    /// or its idx1 indexes no `rec ` lists.
    idx1_rec_entries: Vec<Idx1RecEntry>,
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
    /// Per-stream `WAVEFORMATEX.nBlockAlign` for CBR audio streams
    /// (round-96). Parallel to `streams`: `Some(block_align)` only for
    /// audio streams the AVI 1.0 sample-size invariant pins as CBR
    /// (PCM / A-law / µ-law / IMA-ADPCM, see
    /// [`classify_audio_sample_size`]) whose WAVEFORMATEX carried a
    /// nonzero `nBlockAlign`; `None` for video / data streams, VBR
    /// audio, and CBR audio whose `nBlockAlign` was zero (nothing to
    /// validate against). Drives
    /// [`AviDemuxer::cbr_audio_block_alignment_violations`].
    audio_cbr_block_aligns: Vec<Option<u16>>,
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
    /// Raw `dwPaddingGranularity` from `AVIMAINHEADER` (round-92).
    /// Reflects the muxer's stream-alignment promise from AVI 1.0
    /// §"AVIMAINHEADER" (line 197): *"Alignment for data, in bytes.
    /// Pad the data to multiples of this value."* `0` (the legacy
    /// sentinel) means "no alignment" — files predating round-92
    /// leave this 0. Surfaced via [`AviDemuxer::padding_granularity`]
    /// and the `avi:padding_granularity` metadata key.
    avih_padding_granularity: u32,
    /// Raw `dwInitialFrames` from `AVIMAINHEADER` (round-157). The
    /// file-global counterpart of the per-stream
    /// [`Self::stream_initial_frames`] DWORD (round-153). Per AVI 1.0
    /// §"AVIMAINHEADER" (line 200): *"Initial frame for interleaved
    /// files. Noninterleaved files should specify zero. If creating
    /// interleaved files, specify the number of frames in the file
    /// prior to the initial frame of the AVI sequence."* `0` is the
    /// documented "noninterleaved / unspecified" sentinel, mapped to
    /// `None` by [`AviDemuxer::initial_frames`] so an unspecified
    /// skew reads the same as an absent one (mirroring the per-stream
    /// round-153 / round-119 / round-115 / round-80 "default ==
    /// absent" convention).
    avih_initial_frames: u32,
    /// Raw `dwMicroSecPerFrame` from `AVIMAINHEADER` (round-256). The
    /// file-global frame-period DWORD at byte offset 0 of the 56-byte
    /// AVIMAINHEADER body. Per AVI 1.0 §"AVIMAINHEADER" (line 195):
    /// *"Number of microseconds between frames. Indicates the overall
    /// timing for the file."* The demuxer already consumes this DWORD
    /// internally to derive `duration_micros = total_frames *
    /// micro_sec_per_frame` (see `parse_hdrl`), but pre-round-256 the
    /// value was not surfaced verbatim — only the derived duration
    /// reached callers. Captured here so [`AviDemuxer::micro_sec_per_frame`]
    /// can hand out the raw u32 for callers that want to inspect the
    /// file-global frame period independently of the per-stream
    /// `(dwScale, dwRate)` pair (round-249) — for example to detect a
    /// file written by a capture pipeline that stamped a non-standard
    /// frame period that doesn't match `1_000_000 * stream0_scale /
    /// stream0_rate`. `0` is the writer-skips-it sentinel mapped to
    /// `None` by the accessor, mirroring the round-249 / round-247 /
    /// round-229 etc. "default == absent" convention.
    avih_micro_sec_per_frame: u32,
    /// Raw `dwMaxBytesPerSec` from `AVIMAINHEADER` (round-260). The
    /// file-global maximum-data-rate DWORD at byte offset 4 of the
    /// 56-byte AVIMAINHEADER body. Per AVI 1.0 §"AVIMAINHEADER"
    /// (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
    /// `dwMaxBytesPerSec` row, line 196): *"Approximate maximum data
    /// rate of the file. Number of bytes per second the system must
    /// handle to present an AVI sequence as specified by the other
    /// parameters in the main header and stream header chunks."*
    ///
    /// Pre-round-260 this DWORD was already parsed and surfaced via
    /// the `avi:max_bytes_per_sec` metadata key (round-14) plus the
    /// internal "stamped-rate is generous" advisory check that
    /// compares the value against the post-mux observed peak; round-260
    /// surfaces the raw u32 verbatim through a typed
    /// [`AviDemuxer::max_bytes_per_sec`] accessor so a downstream
    /// remuxer / capture-info dumper can inspect the writer's stamped
    /// data-rate hint without scanning the metadata Vec. `0` is the
    /// writer-skips-it sentinel mapped to `None` by the accessor,
    /// mirroring the round-256 / round-249 / round-247 / round-229 etc.
    /// "default == absent" convention.
    avih_max_bytes_per_sec: u32,
    /// Raw `dwTotalFrames` from `AVIMAINHEADER` (round-268). The
    /// file-global frame-count DWORD at byte offset 16 of the 56-byte
    /// AVIMAINHEADER body. Per AVI 1.0 §"AVIMAINHEADER"
    /// (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
    /// `dwTotalFrames` row, line 199): *"Total number of frames of
    /// data in the file."*
    ///
    /// Pre-round-268 this DWORD was already parsed and consumed
    /// internally to derive `duration_micros = total_frames *
    /// micro_sec_per_frame` (the source of `Demuxer::duration`), but
    /// the raw value was never surfaced — neither a typed accessor
    /// nor a metadata key existed, only the derived duration reached
    /// callers. Captured here so [`AviDemuxer::avih_total_frames`] can
    /// hand out the raw u32 verbatim. For a multi-segment OpenDML
    /// file this field only carries the primary `RIFF AVI ` segment's
    /// frame count (per OpenDML 2.0 §5.0); the cross-segment truth is
    /// the separate `dmlh.dwTotalFrames` surfaced via
    /// [`AviDemuxer::dmlh_total_frames`]. `0` is the writer-skips-it /
    /// empty-file sentinel mapped to `None` by the accessor, mirroring
    /// the round-260 / round-256 / round-249 etc. "default == absent"
    /// convention.
    avih_total_frames: u32,
    /// Raw `dwStreams` from `AVIMAINHEADER` (round-292). The
    /// file-global declared stream-count DWORD at byte offset 24 of the
    /// 56-byte AVIMAINHEADER body. Per AVI 1.0 §"AVIMAINHEADER"
    /// (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
    /// `dwStreams` row, line 201): *"Number of streams in the file. For
    /// example, a file with audio and video has two streams."*
    ///
    /// Captured so [`AviDemuxer::avih_declared_stream_count`] can hand
    /// back the writer-declared count verbatim, and so
    /// [`AviDemuxer::declared_vs_actual_stream_count_mismatch`] can
    /// cross-check it against the number of `strl` LISTs actually walked
    /// in `hdrl`. `0` is the writer-skips-it / unspecified sentinel
    /// mapped to `None` by the accessor, mirroring the round-275 /
    /// round-268 / round-260 / round-256 "default == absent" convention.
    /// Already surfaced as the `avi:streams` metadata key (emitted
    /// verbatim, omitted only for the `0` sentinel).
    avih_streams: u32,
    /// Raw `dwWidth` from `AVIMAINHEADER` (round-275). The file-global
    /// movie-rectangle width DWORD at byte offset 32 of the 56-byte
    /// AVIMAINHEADER body. Per AVI 1.0 §"AVIMAINHEADER"
    /// (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
    /// `dwWidth` row, line 203): *"Width of the AVI file in pixels."*
    /// Captured so [`AviDemuxer::avih_movie_rect`] can hand back the
    /// `(width, height)` pair the per-stream `strh.rcFrame` destination
    /// rectangle (round-119) is expressed relative to. `0` is the
    /// writer-skips-it / unspecified sentinel mapped to `None`,
    /// mirroring the round-268 / round-260 / round-256 "default ==
    /// absent" convention. Already surfaced as `avi:width` metadata.
    avih_width: u32,
    /// Raw `dwHeight` from `AVIMAINHEADER` (round-275). The file-global
    /// movie-rectangle height DWORD at byte offset 36 of the 56-byte
    /// AVIMAINHEADER body. Per AVI 1.0 §"AVIMAINHEADER"
    /// (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
    /// `dwHeight` row, line 204): *"Height of the AVI file in pixels."*
    /// Captured so [`AviDemuxer::avih_movie_rect`] can hand back the
    /// `(width, height)` pair. `0` is the writer-skips-it / unspecified
    /// sentinel mapped to `None`. Already surfaced as `avi:height`
    /// metadata.
    avih_height: u32,
    /// Raw `dwReserved[4]` from `AVIMAINHEADER` (round-330). The four
    /// trailing DWORDs at byte offsets 40..56 of the 56-byte
    /// AVIMAINHEADER body. Per AVI 1.0 §"AVIMAINHEADER"
    /// (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
    /// `dwReserved` row, line 205): *"Reserved. Set this array to
    /// zero."* Captured so [`AviDemuxer::avih_reserved`] can hand back
    /// the verbatim array, mapping the spec-conformant all-zero default
    /// to `None`. Also surfaced as the `avi:reserved` metadata key when
    /// any DWORD is non-zero.
    avih_reserved: [u32; 4],
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
    /// Per-video-stream BMIH-derived side-info (top-down orientation,
    /// `BI_BITFIELDS` color masks) — round-19 candidates 1+2 per VfW
    /// `wingdi.h` §"biHeight sign rules" and §"BI_BITFIELDS". Indexed
    /// by stream number; `None` for non-video streams or video streams
    /// whose `strf` payload was empty / shorter than 40 bytes.
    video_strf: Vec<Option<VideoStrfInfo>>,
    /// Per-audio-stream WAVEFORMATEX(TENSIBLE)-derived side-info
    /// (channel mask, valid bits per sample, SubFormat GUID) — round-75
    /// per Microsoft `mmreg.h` §"WAVEFORMATEXTENSIBLE" and
    /// docs/container/riff/waveformatextensible/. Indexed by stream
    /// number; `None` for non-audio streams. For audio streams whose
    /// `wFormatTag` was not `WAVE_FORMAT_EXTENSIBLE` (`0xFFFE`) the
    /// extensible-only fields are `None`.
    audio_strf: Vec<Option<AudioStrfInfo>>,
    /// Per-stream human-readable name parsed from the optional `strn`
    /// chunk inside each `strl` LIST per AVI 1.0 §"AVI Stream Headers"
    /// (round-80). Indexed by stream number; `None` for streams that
    /// did not carry a `strn` chunk. Surfaced via the typed
    /// [`AviDemuxer::stream_name`] accessor in addition to the
    /// `avi:strn.<index>` metadata key (only emitted for non-empty
    /// names). Empty-payload `strn` chunks parse as `None` so absence
    /// of the chunk and an empty-string body are not conflated.
    stream_names: Vec<Option<String>>,
    /// Per-stream opaque codec-driver configuration blob parsed from
    /// the optional `strd` chunk inside each `strl` LIST per AVI 1.0
    /// §"AVI Stream Headers" (round-89). Indexed by stream number;
    /// `None` for streams that did not carry a `strd` chunk. The
    /// spec defines this body as opaque codec-driver data: "The
    /// format and content of this chunk are defined by the codec
    /// driver. Typically, drivers use this information for
    /// configuration. Applications that read and write AVI files do
    /// not need to interpret this information; they simple transfer
    /// it to and from the driver as a memory block." An empty-payload
    /// `strd` (`cb=0`) parses as `Some(Vec::new())` so an empty driver
    /// blob stays distinguishable from "no strd chunk at all".
    /// Surfaced via the typed [`AviDemuxer::stream_header_data`]
    /// accessor in addition to the `avi:strd.<index>.len` metadata
    /// key (length only, not the raw driver bytes).
    stream_header_data: Vec<Option<Vec<u8>>>,
    /// Per-stream `strh.rcFrame` destination rectangle from the 56-byte
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-115). Indexed
    /// by stream number; `Some([left, top, right, bottom])` when the strh
    /// declared a non-zero rect, `None` when the header was the short
    /// 48-byte form (no `rcFrame`) or carried the all-zero "whole movie
    /// rectangle" writer default. The rect positions a text or video
    /// stream within the movie rectangle (`avih.dwWidth` × `dwHeight`);
    /// units are pixels and the origin is the movie rectangle's upper-left
    /// corner. Surfaced via the typed [`AviDemuxer::stream_frame_rect`]
    /// accessor in addition to the `avi:strh.<index>.frame_rect` metadata
    /// key (`"left,top,right,bottom"`).
    stream_frame_rects: Vec<Option<[i16; 4]>>,
    /// Per-stream `strh.wLanguage` LANGID from byte offset 14 of the
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-119).
    /// Indexed by stream number; `Some(langid)` when the strh declared a
    /// non-zero language tag, `None` when it carried the `0`
    /// ("LANG_NEUTRAL / SUBLANG_NEUTRAL", the writer-skips-it default)
    /// so an unspecified language reads the same as an absent one. The
    /// staged docs (`docs/container/riff/avi-riff-file-reference.md`,
    /// `wLanguage` row in AVISTREAMHEADER) note that AVI does **not**
    /// normatively pin a registry; Microsoft writers populate the field
    /// with a Win32 LANGID while other writers may pack different values
    /// — the demuxer surfaces the raw 16-bit DWORD verbatim and leaves
    /// interpretation to the caller. Surfaced via the typed
    /// [`AviDemuxer::stream_language`] accessor in addition to the
    /// `avi:strh.<index>.language` metadata key.
    stream_languages: Vec<Option<u16>>,
    /// Per-stream `strh.dwInitialFrames` from byte offset 16 of the
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-153).
    /// Indexed by stream number; `Some(frames)` when the strh declared
    /// a non-zero skew, `None` when it carried the `0` writer default
    /// ("noninterleaved file" per AVIMAINHEADER §`dwInitialFrames`:
    /// *"Noninterleaved files should specify zero"*) so an unspecified
    /// skew reads the same as an absent one. Per AVI 1.0 §"AVISTREAMHEADER"
    /// (`dwInitialFrames` row in
    /// `docs/container/riff/avi-riff-file-reference.md`): *"How far
    /// audio data is skewed ahead of the video frames in interleaved
    /// files. Typically, this is about 0.75 seconds. If creating
    /// interleaved files, set the value of this member to the number
    /// of frames in the file prior to the initial frame of the AVI
    /// sequence."* The demuxer surfaces the raw 32-bit DWORD verbatim
    /// and leaves the rate-conversion (unit is the stream's own
    /// `dwRate`/`dwScale` tick) to the caller. Surfaced via the typed
    /// [`AviDemuxer::stream_initial_frames`] accessor in addition to
    /// the `avi:strh.<index>.initial_frames` metadata key.
    stream_initial_frames: Vec<Option<u32>>,
    /// Per-stream `strh.dwQuality` from byte offset 40 of the
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-176).
    /// Indexed by stream number; `Some(quality)` when the strh declared
    /// a value other than the `-1` (`0xFFFF_FFFF` u32) "use default
    /// driver quality" sentinel, `None` when it carried the legacy
    /// muxer default per the `dwQuality` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (*"If set to
    /// -1, drivers use the default quality value."*) so an unspecified
    /// quality reads the same as an absent one. Per the same row the
    /// documented range is `[0, 10_000]` (*"Indicator of the quality
    /// of the data in the stream. Quality is represented as a number
    /// between 0 and 10,000. For compressed data, this typically
    /// represents the value of the quality parameter passed to the
    /// compression software."*); the demuxer surfaces the raw u32
    /// verbatim and does not clamp or normalise, so anomalous
    /// out-of-range writers round-trip exactly. Surfaced via the typed
    /// [`AviDemuxer::stream_quality`] accessor in addition to the
    /// `avi:strh.<index>.quality` metadata key.
    stream_qualities: Vec<Option<u32>>,
    /// Per-stream `strh.wPriority` from byte offset 12 of the
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-182).
    /// Indexed by stream number; `Some(priority)` when the strh declared
    /// a non-zero selection hint, `None` when it carried the legacy `0`
    /// writer default so an unspecified priority reads the same as an
    /// absent one. Per the `wPriority` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (Appendix B
    /// line 238): *"Priority of a stream type. For example, in a file
    /// with multiple audio streams, the one with the highest priority
    /// might be the default stream."* The field is a selection hint
    /// among same-`fccType` streams (the spec illustration picks a
    /// default-playback audio stream among several); the spec does not
    /// normatively pin a value range or a tie-break rule so the
    /// demuxer surfaces the raw 16-bit DWORD verbatim and leaves the
    /// "what counts as highest" decision to the caller. Surfaced via
    /// the typed [`AviDemuxer::stream_priority`] accessor in addition
    /// to the `avi:strh.<index>.priority` metadata key.
    stream_priorities: Vec<Option<u16>>,
    /// Per-stream `strh.dwStart` starting time from byte offset 28 of
    /// the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-203).
    /// Indexed by stream number; `Some(start)` when the strh declared
    /// a non-zero start, `None` when it carried the legacy `0` writer
    /// default so an unspecified start reads the same as an absent
    /// one. Per the `dwStart` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 243):
    /// *"Starting time for this stream. The units are defined by the
    /// dwRate and dwScale members in the main file header. Usually,
    /// this is zero, but it can specify a delay time for a stream
    /// that does not start concurrently with the file."* The unit is
    /// the stream's own `(dwRate / dwScale)` tick (frames for video,
    /// samples-or-blocks for audio); the demuxer surfaces the raw u32
    /// verbatim with no rate-conversion. Surfaced via the typed
    /// [`AviDemuxer::stream_start`] accessor in addition to the
    /// `avi:strh.<index>.start` metadata key.
    stream_starts: Vec<Option<u32>>,
    /// Per-stream `strh.fccHandler` driver hint from byte offset 4 of
    /// the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-210).
    /// Indexed by stream number; `Some([f0,f1,f2,f3])` when the strh
    /// declared a non-zero FourCC, `None` when it carried the all-zero
    /// `\0\0\0\0` "no preferred handler" default so an unspecified hint
    /// reads the same as an absent one. Per the `fccHandler` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (Appendix B
    /// line 236): *"An optional FOURCC that identifies a specific data
    /// handler. The data handler is the preferred handler for the
    /// stream. For audio and video streams, this specifies the codec
    /// for decoding the stream."* The field is the optional VfW data-
    /// handler identifier — distinct from the video stream's
    /// `BITMAPINFOHEADER.biCompression` FourCC (which the strh's
    /// fccHandler typically mirrors but is not required to match;
    /// some legacy capture writers leave fccHandler zero on video
    /// streams that have a perfectly valid biCompression) — and the
    /// spec's *optional FOURCC* phrasing does not normatively pin
    /// printability, so the demuxer surfaces the raw 4 bytes
    /// verbatim and does not validate. Surfaced via the typed
    /// [`AviDemuxer::stream_handler`] accessor in addition to the
    /// `avi:strh.<index>.handler` metadata key.
    stream_handlers: Vec<Option<[u8; 4]>>,
    /// Per-stream `strh.dwSuggestedBufferSize` read-ahead hint from byte
    /// offset 36 of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    /// (round-217). Indexed by stream number; `Some(n)` when the strh
    /// declared a non-zero hint, `None` when it carried the spec-
    /// documented `0` "do not know the correct buffer size" sentinel so
    /// an unspecified hint reads the same as an absent one. Per the
    /// `dwSuggestedBufferSize` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 245):
    /// *"How large a buffer should be used to read this stream.
    /// Typically, this contains a value corresponding to the largest
    /// chunk present in the stream. Using the correct buffer size makes
    /// playback more efficient. Use zero if you do not know the correct
    /// buffer size."* The field is the per-stream counterpart of the
    /// file-global `avih.dwSuggestedBufferSize` already surfaced via
    /// [`AviDemuxer::avih_suggested_buffer_size`] — the avih flavour is the
    /// largest chunk across every stream, the strh flavour is a
    /// per-stream upper bound (which the spec recommends keeping equal
    /// to the largest chunk in that one stream). Surfaced via the typed
    /// [`AviDemuxer::stream_suggested_buffer_size`] accessor in addition
    /// to the `avi:strh.<index>.suggested_buffer_size` metadata key.
    stream_suggested_buffer_sizes: Vec<Option<u32>>,
    /// Per-stream `strh.dwSampleSize` hint from byte offset 44 of the
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-222).
    /// Indexed by stream number; `Some(n)` when the strh declared a
    /// non-zero size, `None` when it carried the spec-documented `0`
    /// "samples can vary in size" sentinel so an unspecified hint reads
    /// the same as an absent one. Per the `dwSampleSize` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 247):
    /// *"The size of a single sample of data. This is set to zero if
    /// the samples can vary in size. If this number is nonzero, then
    /// multiple samples of data can be grouped into a single chunk
    /// within the file. If it is zero, each sample of data (such as a
    /// video frame) must be in a separate chunk. For video streams,
    /// this number is typically zero, although it can be nonzero if all
    /// video frames are the same size. For audio streams, this number
    /// should be the same as the nBlockAlign member of the WAVEFORMATEX
    /// structure describing the audio."* Surfaced via the typed
    /// [`AviDemuxer::stream_sample_size`] accessor in addition to the
    /// `avi:strh.<index>.sample_size` metadata key.
    stream_sample_sizes: Vec<Option<u32>>,
    /// Per-stream `strh.dwLength` raw value from byte offset 32 of the
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-229).
    /// Indexed by stream number; `Some(n)` when the strh declared a
    /// non-zero length, `None` when it carried the `0` "no length
    /// declared" value so an empty / unspecified stream reads the same
    /// as an absent one. Per the `dwLength` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 244):
    /// *"Length of this stream. The units are defined by the dwRate
    /// and dwScale members of the stream's header."* Surfaced via the
    /// typed [`AviDemuxer::stream_length`] accessor in addition to the
    /// `avi:strh.<index>.length` metadata key. Logically distinct from
    /// the `StreamInfo::duration` exposed by
    /// [`oxideav_core::Demuxer::streams`] — both are derived from this
    /// same DWORD but the framework's duration is typed as
    /// `Option<i64>` while the raw-u32 surface keeps the value
    /// observable verbatim for callers that need byte-exact round-trip
    /// semantics or comparison against a separately-emitted writer's
    /// stamp.
    stream_lengths: Vec<Option<u32>>,
    /// Per-stream `strh.dwFlags` raw value from byte offset 8 of the
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-247).
    /// Indexed by stream number; `Some(bits)` when the strh declared a
    /// non-zero flag DWORD, `None` when it carried the `0` "no flags
    /// set" legacy writer default so an unspecified flag field reads
    /// the same as an absent one. Per the `dwFlags` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 237) +
    /// the *dwFlags values* table at lines 252–255: two `AVISF_*`
    /// bits are spec-documented — `AVISF_DISABLED` (`0x0000_0001`,
    /// stream should not be enabled by default) and
    /// `AVISF_VIDEO_PALCHANGES` (`0x0001_0000`, video stream contains
    /// palette changes). Surfaced via the typed
    /// [`AviDemuxer::stream_flags`] (raw u32) and
    /// [`AviDemuxer::stream_flags_typed`] ([`StrhFlags`] decode)
    /// accessors in addition to the `avi:strh.<index>.flags`
    /// hex-string metadata key (`0xXXXXXXXX` upper-case, omitted on
    /// the `0` default). The demuxer does NOT mask bits outside the
    /// documented set so undocumented vendor / driver bits round-trip
    /// observable.
    stream_flags: Vec<Option<u32>>,
    /// Per-stream `(strh.dwScale, strh.dwRate)` raw timebase pair
    /// captured from byte offsets 20 + 24 of each AVISTREAMHEADER
    /// (round-249). Parallel to `streams`: `Some((scale, rate))` when
    /// both raw DWORDs were non-zero, `None` when either was zero (a
    /// writer-skips-it / mathematically-undefined `rate/scale` ratio,
    /// matching the documented behaviour where `dwRate / dwScale`
    /// gives the number of samples per second per AVI 1.0
    /// §"AVISTREAMHEADER"). The internal `StreamInfo::time_base`
    /// derivation still applies `.max(1)` to each member so a
    /// degenerate file stays decodable; this raw-DWORD surface keeps
    /// the on-disk byte pattern observable for round-trip parity.
    /// Surfaced via the typed [`AviDemuxer::stream_timebase`]
    /// accessor and the `avi:strh.<index>.scale` /
    /// `avi:strh.<index>.rate` decimal metadata keys.
    stream_rates: Vec<Option<(u32, u32)>>,
    /// Per-stream `strh.fccType` raw FOURCC from byte offset 0 of the
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-253).
    /// Indexed by stream number; `Some(fcc)` when the strh declared a
    /// non-zero FOURCC, `None` when it carried the all-zero
    /// `[0, 0, 0, 0]` sentinel so an unspecified type reads the same
    /// as an absent one (mirroring the round-249 `(dwScale, dwRate)`
    /// / round-247 `dwFlags` / round-229 `dwLength` "default ==
    /// absent" convention).
    ///
    /// Per the `fccType` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (Appendix B
    /// line 235) the field is *"Same as `fcc` (in the avifmt.h
    /// definition; see Remarks)."*, and the `fcc` row (line 234)
    /// documents the standard `{auds, mids, txts, vids}` set: *"A
    /// FOURCC code that specifies the type of data contained in the
    /// stream."* The demuxer surfaces the raw 4 bytes verbatim and
    /// does NOT validate membership in the spec-documented set —
    /// the spec does not pin a closed registry, and vendor-specific
    /// FOURCCs are surfaced for the caller to interpret. The
    /// demuxer's own internal codec classification (which switches on
    /// the strh `fccType` for media-kind routing) is independent of
    /// this surface; this raw-FOURCC surface keeps the on-disk byte
    /// pattern observable for round-trip parity.
    ///
    /// Surfaced via the typed [`AviDemuxer::stream_fcc_type`] accessor
    /// and the `avi:strh.<index>.fcc_type` metadata key.
    stream_fcc_types: Vec<Option<[u8; 4]>>,
    /// Digitization-date text from the optional `IDIT` chunk inside
    /// `LIST hdrl` (round-107). `IDIT` is a member of the RIFF *Hdrl
    /// Tags* namespace (`DateTimeOriginal`) per
    /// `docs/container/riff/metadata/exiftool-riff-tags.html`. `None`
    /// when the file carried no `IDIT` chunk (or only an empty / all-
    /// whitespace one). The string is the chunk body with trailing
    /// NUL / whitespace stripped and decoded UTF-8-lossy; the on-disk
    /// text format is writer-defined and not normalised. Surfaced via
    /// the typed [`AviDemuxer::digitization_date`] accessor in addition
    /// to the `avi:idit` metadata key.
    digitization_date: Option<String>,
    /// SMPTE-timecode text from the optional `ISMP` chunk inside
    /// `LIST hdrl` (round-112). `ISMP` is a member of the RIFF *Hdrl
    /// Tags* namespace (`TimeCode`) per
    /// `docs/container/riff/metadata/exiftool-riff-tags.html`, sitting
    /// directly beside `IDIT`. `None` when the file carried no `ISMP`
    /// chunk (or only an empty / all-whitespace one). The string is the
    /// chunk body with trailing NUL / whitespace stripped and decoded
    /// UTF-8-lossy; the on-disk text format is writer-defined and not
    /// normalised. Surfaced via the typed [`AviDemuxer::smpte_timecode`]
    /// accessor in addition to the `avi:ismp` metadata key.
    smpte_timecode: Option<String>,
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

/// `AVISF_DISABLED` per AVI 1.0 §"AVISTREAMHEADER" dwFlags table
/// (`docs/container/riff/avi-riff-file-reference.md`, line 254):
/// *"Indicates this stream should not be enabled by default."* Players
/// honouring the bit start playback with the stream muted / hidden
/// unless the user opts in.
pub const AVISF_DISABLED: u32 = 0x0000_0001;
/// `AVISF_VIDEO_PALCHANGES` per AVI 1.0 §"AVISTREAMHEADER" dwFlags
/// table (`docs/container/riff/avi-riff-file-reference.md`, line 255):
/// *"Indicates this video stream contains palette changes. This flag
/// warns the playback software that it will need to animate the
/// palette."* Pairs with the per-stream `xxpc` palette-change chunks
/// already surfaced via [`AviDemuxer::palette_change_count`] /
/// [`AviDemuxer::palette_change_data`].
pub const AVISF_VIDEO_PALCHANGES: u32 = 0x0001_0000;

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
/// `WAVE_FORMAT_AAC_ADTS` per `mmreg.h` — AAC carried in ADTS frames
/// (the alternative tag some captures use instead of `0x00FF`). VBR:
/// requires `strh.dwSampleSize == 0`. Round-16 candidate 4.
pub const WAVE_FORMAT_AAC_ADTS: u16 = 0x1601;
/// `WAVE_FORMAT_DOLBY_AC3_SPDIF` / `WAVE_FORMAT_AC3` per `mmreg.h` —
/// Dolby Digital AC-3 (SPDIF passthrough form-tag also used in AVI
/// for AC-3 carriage). VBR: requires `strh.dwSampleSize == 0`.
/// Round-16 candidate 4.
pub const WAVE_FORMAT_AC3: u16 = 0x2000;
/// `WAVE_FORMAT_DTS` per `mmreg.h` — DTS Coherent Acoustics audio.
/// VBR: requires `strh.dwSampleSize == 0`. Round-16 candidate 4.
pub const WAVE_FORMAT_DTS: u16 = 0x2001;
/// `WAVE_FORMAT_WMAUDIO1` per `mmreg.h` — Windows Media Audio v1.
/// VBR: requires `strh.dwSampleSize == 0`. Round-16 candidate 4.
pub const WAVE_FORMAT_WMA1: u16 = 0x0160;
/// `WAVE_FORMAT_WMAUDIO2` per `mmreg.h` — Windows Media Audio v2/v9.
/// VBR: requires `strh.dwSampleSize == 0`. Round-16 candidate 4.
pub const WAVE_FORMAT_WMA2: u16 = 0x0161;
/// `WAVE_FORMAT_WMAUDIO3` / `WMAUDIO_PRO` per `mmreg.h` — Windows
/// Media Audio Pro. VBR: requires `strh.dwSampleSize == 0`.
/// Round-16 candidate 4.
pub const WAVE_FORMAT_WMA_PRO: u16 = 0x0162;
/// `WAVE_FORMAT_WMAUDIO_LOSSLESS` per `mmreg.h` — Windows Media
/// Audio Lossless. VBR: requires `strh.dwSampleSize == 0`.
/// Round-16 candidate 4.
pub const WAVE_FORMAT_WMA_LOSSLESS: u16 = 0x0163;
/// Xiph-assigned Opus form-tag for AVI carriage (`0x704F`, ASCII
/// `pO` little-endian). VBR: requires `strh.dwSampleSize == 0`.
/// Round-16 candidate 4.
pub const WAVE_FORMAT_OPUS: u16 = 0x704F;

/// Round-14 candidate 2: classify a WAVEFORMATEX `wFormatTag` per
/// the AVI 1.0 sample-size invariant.
///
/// - `Some(true)` ⇒ VBR codec (one packet = one variable-length
///   frame); `strh.dwSampleSize` MUST be 0.
/// - `Some(false)` ⇒ CBR codec (fixed bytes per sample);
///   `strh.dwSampleSize` MUST be > 0.
/// - `None` ⇒ no constraint (codec the spec doesn't pin one way or
///   the other — e.g. obscure / custom registrations).
///
/// Round-16 candidate 4 widens the VBR side to cover AC-3 / DTS /
/// WMA1 / WMA2 / WMA Pro / WMA Lossless / Opus / AAC-ADTS — every
/// modern compressed-audio tag the AVI carriage rules pin to
/// per-frame packets.
fn classify_audio_sample_size(format_tag: u16) -> Option<bool> {
    match format_tag {
        WAVE_FORMAT_MPEG
        | WAVE_FORMAT_MPEGLAYER3
        | WAVE_FORMAT_AAC
        | WAVE_FORMAT_AAC_ADTS
        | WAVE_FORMAT_AC3
        | WAVE_FORMAT_DTS
        | WAVE_FORMAT_WMA1
        | WAVE_FORMAT_WMA2
        | WAVE_FORMAT_WMA_PRO
        | WAVE_FORMAT_WMA_LOSSLESS
        | WAVE_FORMAT_OPUS => Some(true),
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

/// Typed decode of `AVISTREAMHEADER.dwFlags` per AVI 1.0
/// §"AVISTREAMHEADER" `dwFlags` row + the spec's *dwFlags values*
/// table (`docs/container/riff/avi-riff-file-reference.md`, line 237
/// + lines 252–255, round-247).
///
/// Each documented `AVISF_*` bit decodes to its own `bool`; the raw
/// `bits` field carries the original DWORD so callers wanting to
/// inspect undocumented or vendor-extension bits don't lose
/// information.
///
/// Returned by [`AviDemuxer::stream_flags_typed`]; the same DWORD
/// surfaces as the `avi:strh.<index>.flags` hex-string metadata key
/// when non-zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StrhFlags {
    /// `AVISF_DISABLED` (`0x0000_0001`) — stream should not be enabled
    /// by default. Players honouring the bit start playback with the
    /// stream muted / hidden unless the user opts in.
    pub disabled: bool,
    /// `AVISF_VIDEO_PALCHANGES` (`0x0001_0000`) — video stream
    /// contains palette changes. Pairs with the per-stream `xxpc`
    /// palette-change chunks already surfaced via
    /// [`AviDemuxer::palette_change_count`] /
    /// [`AviDemuxer::palette_change_data`].
    pub video_palchanges: bool,
    /// Raw `dwFlags` DWORD as parsed from `strh`. Non-zero bits
    /// outside the documented set are vendor-extension / future-spec
    /// bits and are exposed verbatim — the spec carries only two
    /// `AVISF_*` constants, but writers in the wild occasionally pack
    /// driver-private bits in the upper half-DWORD.
    pub bits: u32,
}

impl StrhFlags {
    /// Decode a raw `strh.dwFlags` u32 into a structured [`StrhFlags`].
    pub fn from_bits(bits: u32) -> Self {
        Self {
            disabled: bits & AVISF_DISABLED != 0,
            video_palchanges: bits & AVISF_VIDEO_PALCHANGES != 0,
            bits,
        }
    }
}

/// Typed decode of one `idx1` entry's `dwFlags` DWORD (round-17
/// candidate 3).
///
/// Per AVI 1.0 §3.4 + Microsoft's `vfw.h` `AVIIF_*` table the 32-bit
/// flag field carries:
/// - `AVIIF_LIST` (0x0001) — entry refers to a `LIST` chunk
/// - `AVIIF_KEYFRAME` (0x0010) — entry is a random-access keyframe
/// - `AVIIF_FIRSTPART` (0x0020) — entry is the first of a multi-part packet
/// - `AVIIF_LASTPART` (0x0040) — entry is the last of a multi-part packet
/// - `AVIIF_NO_TIME` (0x0100) — entry does NOT increment the
///   per-stream presentation clock (typical for `xxpc` palette and
///   `xxtx` text chunks)
/// - `AVIIF_COMPRESSOR` (0x0FFF_0000) — compressor-specific bits
///
/// Returned by [`AviDemuxer::idx1_typed_flags_for_packet`]; the raw
/// `bits` field is preserved verbatim so vendor-extension /
/// future-spec bits don't get lost when a codec needs them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Idx1Flags {
    /// `AVIIF_LIST` — entry refers to a `LIST` chunk (typically a
    /// `LIST rec ` grouping inside `movi`) rather than a single
    /// payload chunk. Rare in modern files.
    pub is_list: bool,
    /// `AVIIF_KEYFRAME` — entry is a keyframe (random-access /
    /// I-frame). Same bit drives [`AviDemuxer::seek_to_keyframe_strict`].
    pub is_keyframe: bool,
    /// `AVIIF_FIRSTPART` — entry is the FIRST chunk of a multi-part
    /// packet. The matching closing entry should carry
    /// `AVIIF_LASTPART`. The muxer also sets both bits together on
    /// every idx1 entry of a 2-field interlaced stream (see
    /// [`AviDemuxer::idx1_flags_for_packet`] for the legacy
    /// `0x60`-stamping convention).
    pub is_first_part: bool,
    /// `AVIIF_LASTPART` — entry is the LAST chunk of a multi-part
    /// packet. See [`Self::is_first_part`].
    pub is_last_part: bool,
    /// `AVIIF_NO_TIME` (also spelled `AVIIF_NOTIME` in some SDK
    /// headers) — entry doesn't advance the per-stream presentation
    /// clock. Set on `xxpc` palette-change and `xxtx` text/subtitle
    /// entries whose timing is gated by the surrounding video
    /// chunk's PTS rather than carrying their own.
    pub is_no_time: bool,
    /// Raw `dwFlags` DWORD as recorded in idx1. Bits outside the
    /// documented union (`AVIIF_LIST | AVIIF_KEYFRAME | AVIIF_FIRSTPART
    /// | AVIIF_LASTPART | AVIIF_NO_TIME | AVIIF_COMPRESSOR`) are
    /// vendor-extension or reserved-future bits and are exposed
    /// verbatim through this field.
    pub bits: u32,
}

impl Idx1Flags {
    /// Decode a raw `dwFlags` u32 into a structured [`Idx1Flags`].
    pub fn from_bits(bits: u32) -> Self {
        Self {
            is_list: bits & AVIIF_LIST != 0,
            is_keyframe: bits & AVIIF_KEYFRAME != 0,
            is_first_part: bits & AVIIF_FIRSTPART != 0,
            is_last_part: bits & AVIIF_LASTPART != 0,
            is_no_time: bits & AVIIF_NO_TIME != 0,
            bits,
        }
    }

    /// Returns the masked compressor-specific bits — `bits &
    /// AVIIF_COMPRESSOR`. Per `vfw.h` the upper 12 bits of the high
    /// 16-bit half are reserved for codec-private use and are
    /// opaque to the container layer; per-codec readers can pull
    /// them out unchanged via this accessor.
    pub fn compressor_bits(self) -> u32 {
        self.bits & AVIIF_COMPRESSOR
    }
}

/// One `rec ` LIST entry recorded in the legacy `idx1` index
/// (round-285).
///
/// Per AVI 1.0 §"AVI Index Entries" the idx1 chunk "consists of an
/// AVIOLDINDEX structure with entries for each data chunk, including
/// 'rec ' chunks", and per Appendix C the `AVIIF_LIST` flag marks an
/// entry whose chunk "is a 'rec ' list". Such an entry describes one
/// `LIST rec ` CD-ROM-interleave grouping cluster inside `movi` rather
/// than any per-stream payload chunk — the recorded ckid is the `rec `
/// form-type, not a `NNxx` stream chunk id — so it carries no stream
/// index and never appears in the per-stream seek table. This struct
/// surfaces the entries verbatim for round-trip parity and
/// interleave-structure inspection via
/// [`AviDemuxer::idx1_rec_list_entries`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Idx1RecEntry {
    /// Raw `dwFlags` DWORD as recorded in idx1 — decode via
    /// [`Idx1Flags::from_bits`]. Writers conforming to Appendix C set
    /// `AVIIF_LIST` (0x0001); the value is surfaced unmasked so
    /// non-conforming / vendor bits stay observable.
    pub flags: u32,
    /// File-absolute offset of the cluster's `LIST` chunk header,
    /// resolved with the same movi-relative vs file-absolute base
    /// detection as the per-stream seek table (idx1's `dwOffset` is
    /// ambiguous between the two conventions; see
    /// [`build_idx_table`]'s probe).
    pub offset: u64,
    /// `dwSize` as recorded in idx1 — the `LIST` chunk's size-field
    /// value (the 4-byte `rec ` form-type FourCC plus the grouped
    /// chunk bytes), surfaced verbatim with no cross-validation
    /// against the on-disk LIST header.
    pub size: u32,
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

/// Typed decode of an `xxtx` text/subtitle chunk body (round-15
/// candidate 3).
///
/// Per Microsoft `vfw.h` the `xxtx` chunk body for an AVI text stream
/// (`txts` `fccType`) carries a 6-byte fixed header followed by the
/// raw text payload:
/// ```text
/// WORD  wCodePage  // ANSI code page (e.g. 0 = system default,
///                  //   1252 = Windows-1252, 65001 = UTF-8). Per
///                  //   `mmsystem.h`'s `MM_PALETTE_FOREGROUND` /
///                  //   text-stream conventions, 0 means "no
///                  //   conversion — interpret bytes as the system
///                  //   default", which is what most legacy capture
///                  //   tools emit.
/// WORD  wLanguage  // Primary language tag (LANGID; ITU/ISO not
///                  //   normatively pinned by AVI, but `vfw.h`
///                  //   re-uses Win32's `LANGID` packed `(sublang
///                  //   << 10) | primary`).
/// WORD  wDialect   // Sub-language / dialect (0 = neutral).
/// BYTE  body[]     // Raw payload — code-page bytes for an ANSI
///                  //   page, UTF-8 octets for codepage 65001.
/// ```
/// Composed by [`AviDemuxer::text_chunk_typed`] /
/// [`AviDemuxer::text_chunk_typed_iter`] from the round-12 raw
/// [`AviDemuxer::text_chunk_data`] accessor; consumed by
/// [`crate::muxer::AviMuxer::with_text_chunk_typed`] to write the
/// equivalent chunk back. Closes the typed round-trip pair so callers
/// don't have to hand-pack the 6-byte VfW header.
///
/// `body` is a `String`: when `codepage == 65001` (or `0`, treated as
/// "best-effort UTF-8") the parser decodes the raw bytes as UTF-8
/// (lossy — invalid sequences become `U+FFFD`); for any other code
/// page the bytes are interpreted as Latin-1 (each byte → one
/// `char`), preserving every octet without depending on a code-page
/// converter crate. Callers that need byte-exact round-trip for a
/// non-UTF-8 page should re-encode their `String` themselves before
/// muxing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextChunk {
    /// `wCodePage`. `0` means "system default" per `vfw.h`; `65001`
    /// is UTF-8 (the modern recommendation); anything else is a
    /// Windows ANSI code page.
    pub codepage: u16,
    /// `wLanguage` — primary LANGID per Microsoft conventions. AVI
    /// itself doesn't pin a registry; modern tools use BCP 47 tags
    /// out-of-band (e.g. via the parent `txts` strh's `wLanguage`).
    pub language: u16,
    /// `wDialect` — sub-language / dialect ID. `0` is neutral.
    pub dialect: u16,
    /// Decoded text body. UTF-8 for `codepage == 0` or `65001`,
    /// Latin-1 (each byte → one `char`) otherwise. See struct docs
    /// for the round-trip caveat on non-UTF-8 pages.
    pub body: String,
}

impl TextChunk {
    /// Parse a raw `xxtx` chunk body into the typed shape. Returns
    /// `None` for bodies shorter than the 6-byte fixed header.
    /// Body decoding picks UTF-8 (lossy) for codepage `0` / `65001`
    /// and Latin-1 byte-pass-through for any other code page; see
    /// the struct-level docs for the round-trip caveat.
    pub fn parse(body: &[u8]) -> Option<Self> {
        if body.len() < 6 {
            return None;
        }
        let codepage = u16::from_le_bytes([body[0], body[1]]);
        let language = u16::from_le_bytes([body[2], body[3]]);
        let dialect = u16::from_le_bytes([body[4], body[5]]);
        let tail = &body[6..];
        let text = if codepage == 0 || codepage == 65001 {
            // Interpret as UTF-8 (lossy on invalid sequences).
            String::from_utf8_lossy(tail).into_owned()
        } else {
            // Latin-1 byte pass-through: every byte is one char.
            // Preserves the raw octets so a downstream caller can
            // re-encode if it knows the page; avoids pulling in a
            // code-page converter crate for the common path.
            tail.iter().map(|&b| b as char).collect()
        };
        Some(Self {
            codepage,
            language,
            dialect,
            body: text,
        })
    }

    /// Encode the typed shape back into a raw `xxtx` chunk body
    /// suitable for [`crate::muxer::AviMuxer::write_text_chunk`].
    /// Output layout matches [`Self::parse`]'s expectations exactly:
    /// 2-byte LE `codepage`, 2-byte LE `language`, 2-byte LE
    /// `dialect`, then the body bytes (UTF-8 for codepage `0` /
    /// `65001`, Latin-1 truncated to the low byte of each `char`
    /// otherwise — symmetric to `parse`'s decode rule).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + self.body.len());
        out.extend_from_slice(&self.codepage.to_le_bytes());
        out.extend_from_slice(&self.language.to_le_bytes());
        out.extend_from_slice(&self.dialect.to_le_bytes());
        if self.codepage == 0 || self.codepage == 65001 {
            out.extend_from_slice(self.body.as_bytes());
        } else {
            // Latin-1 pass-through inverse: take the low byte of
            // each char. For an unmodified parse→to_bytes cycle on
            // the same body this is byte-exact (parse mapped each
            // byte to `b as char` whose low byte is `b`).
            for c in self.body.chars() {
                out.push((c as u32) as u8);
            }
        }
        out
    }
}

/// Lazy iterator returned by [`AviDemuxer::text_chunk_typed_iter`]
/// (round-15 candidate 3). Mirrors [`PaletteChangeTypedIter`] for the
/// `xxtx` text-chunk family — yields one `Result<TextChunk>` per
/// chunk for the requested stream, decoding the typed shape on
/// demand instead of materialising the full Vec. Useful for
/// long-running subtitle / cuepoint streams where the full set may
/// be tens of thousands of entries.
pub struct TextChunkTypedIter<'a> {
    bodies: &'a [Vec<u8>],
    next: usize,
}

impl<'a> Iterator for TextChunkTypedIter<'a> {
    type Item = Result<TextChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        let body = self.bodies.get(self.next)?;
        self.next += 1;
        match TextChunk::parse(body) {
            Some(tc) => Some(Ok(tc)),
            None => Some(Err(Error::invalid(format!(
                "AVI: xxtx body #{} ({} bytes) failed to decode as TextChunk",
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

impl<'a> ExactSizeIterator for TextChunkTypedIter<'a> {}

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
    /// The `ix##` chunk's own RIFF FourCC (e.g. `ix00` for stream 0).
    /// Distinct from the body's `dwChunkId`: this is the chunk-header
    /// FourCC the demuxer found in `movi`, retained so the per-segment
    /// `dwChunkId` cross-check ([`AviDemuxer::std_index_chunk_ids`]) can
    /// compare the declared indexed-chunk FourCC against the stream the
    /// `ix##` chunk itself was emitted for.
    own_fourcc: [u8; 4],
    /// FourCC of indexed chunks (`dwChunkId`, e.g. `00dc`). The two ASCII
    /// digits at `chunk_id[0..2]` give the stream number.
    chunk_id: [u8; 4],
    /// Base offset for `dw_offset` lookups — typically the file offset
    /// of the enclosing `movi` LIST's first chunk header.
    qw_base_offset: u64,
    /// Index sub-type: 0 (default, progressive) or
    /// `AVI_INDEX_SUB_2FIELD` (2-field interlaced).
    #[allow(dead_code)]
    b_index_sub_type: u8,
    /// `nEntriesInUse`: the entry count the `ix##` chunk *declares* in its
    /// header, retained verbatim even when the body is truncated and fewer
    /// entries were actually parseable. `entries.len()` is the number the
    /// demuxer could physically read; the two disagree only for a
    /// truncated standard index (round-325 — surfaced via
    /// [`AviDemuxer::std_index_declared_entry_counts`] and
    /// [`AviDemuxer::std_index_entry_count_violations`]).
    declared_n_entries: u32,
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

/// `AVIIF_LIST` per Microsoft's `vfw.h` — entry refers to a `LIST`
/// chunk (e.g. an internal `LIST rec ` grouping inside `movi`)
/// rather than a single payload chunk. Rare in modern files but
/// still emitted by Microsoft's reference muxer when grouping
/// chunks for streaming I/O alignment.
pub const AVIIF_LIST: u32 = 0x0000_0001;
/// `AVIIF_KEYFRAME` bit in an idx1 entry's flags. Set on entries
/// whose payload chunk is a keyframe (random-access / I-frame). The
/// in-tree seek path uses this bit to drive
/// [`AviDemuxer::seek_to_keyframe_strict`].
pub const AVIIF_KEYFRAME: u32 = 0x0000_0010;
/// `AVIIF_FIRSTPART` per `vfw.h` — entry is the FIRST chunk of a
/// multi-part packet (the rest follow as additional idx1 entries
/// flagged at minimum with `AVIIF_LASTPART` on the closing one).
/// Also set as part of the `AVIIF_FIRSTPART | AVIIF_LASTPART`
/// pair the muxer stamps on every entry for a 2-field interlaced
/// stream — see `idx1_flags_for_packet`'s field2 detection.
pub const AVIIF_FIRSTPART: u32 = 0x0000_0020;
/// `AVIIF_LASTPART` per `vfw.h` — entry is the LAST chunk of a
/// multi-part packet. See [`AVIIF_FIRSTPART`].
pub const AVIIF_LASTPART: u32 = 0x0000_0040;
/// `AVIIF_NO_TIME` per `vfw.h` (also spelled `AVIIF_NOTIME` in some
/// SDK headers) — entry doesn't increment the per-stream
/// presentation clock. Typically set on text/subtitle (`xxtx`) and
/// palette-change (`xxpc`) chunks whose presentation is gated by
/// the next "real" video chunk's PTS rather than carrying their
/// own time.
pub const AVIIF_NO_TIME: u32 = 0x0000_0100;
/// `AVIIF_COMPRESSOR` per `vfw.h` — bitmask covering the
/// compressor-specific upper 16 bits of `dwFlags`. Any non-zero
/// value here is opaque to the container layer; per-codec readers
/// may inspect it via [`Idx1Flags::compressor_bits`].
pub const AVIIF_COMPRESSOR: u32 = 0x0FFF_0000;

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
    ///
    /// This is the legacy bare-`u32` accessor; the
    /// [`Self::avih_suggested_buffer_size_typed`] companion (round-298)
    /// folds the `0` "do not know / unpopulated" sentinel to `None` so
    /// an unspecified hint reads the same as an absent one, matching the
    /// "default == absent" convention every later file-global accessor
    /// adopted.
    pub fn avih_suggested_buffer_size(&self) -> u32 {
        self.avih_suggested_buffer_size
    }

    /// `AVIMAINHEADER.dwSuggestedBufferSize` per AVI 1.0 §"AVIMAINHEADER"
    /// (round-298), as `Option<u32>`.
    ///
    /// Returns the file-global read-ahead allocation hint from byte
    /// offset 28 of the 56-byte AVIMAINHEADER body, or `None` when the
    /// field carried the writer-skips-it / unpopulated `0` sentinel. Per
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A: *"Suggested
    /// buffer size for reading the file. Generally, large enough to
    /// contain the largest chunk in the file. If set to zero or too
    /// small, playback software will have to reallocate memory during
    /// playback, which will reduce performance. For interleaved files,
    /// the buffer size should be large enough to read an entire record
    /// (not just a chunk)."*
    ///
    /// This is the typed companion of the legacy bare-`u32`
    /// [`Self::avih_suggested_buffer_size`] accessor (round-13), which is
    /// retained for backward compatibility. The legacy accessor cannot
    /// distinguish "writer stamped 0 because it did not know the size"
    /// from "field genuinely held 0"; this typed surface folds both to
    /// `None`, matching the "default == absent" convention of
    /// [`Self::avih_total_frames`] (round-268) /
    /// [`Self::max_bytes_per_sec`] (round-260) /
    /// [`Self::micro_sec_per_frame`] (round-256) /
    /// [`Self::avih_declared_stream_count`] (round-292).
    ///
    /// The avih flavour is the file-global read-ahead bound — generally
    /// the largest chunk (or, for interleaved files, the largest `rec `
    /// record) across every stream — and is logically distinct from the
    /// per-stream `strh.dwSuggestedBufferSize` surfaced via
    /// [`Self::stream_suggested_buffer_size`] (round-217), which is a
    /// per-stream upper bound. The two are spec-independent: a writer
    /// may stamp consistent values, set only one, or leave both at `0`,
    /// and the demuxer surfaces each verbatim with no validation against
    /// the actual largest chunk seen in `movi` since over-declaration is
    /// the documented intent of the field. The same value also surfaces
    /// under the `avi:suggested_buffer_size` metadata key, which is
    /// omitted entirely when the value is `0` so absence of the key is
    /// observable.
    ///
    /// The muxer's counterpart is auto-derived (the largest chunk-body
    /// size observed across all streams) unless overridden via
    /// [`crate::muxer::AviMuxOptions::with_suggested_buffer_size`]; a
    /// round-trip through this crate's own writer reproduces the stamped
    /// value verbatim, and an explicit `0` override maps back to `None`
    /// here.
    pub fn avih_suggested_buffer_size_typed(&self) -> Option<u32> {
        if self.avih_suggested_buffer_size == 0 {
            None
        } else {
            Some(self.avih_suggested_buffer_size)
        }
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

    /// Typed [`TextChunk`] decode of every `xxtx` chunk attached to
    /// `stream_index` (round-15 candidate 3).
    ///
    /// Composes the round-12 raw [`Self::text_chunk_data`] accessor
    /// with [`TextChunk::parse`] so callers don't have to hand-decode
    /// the VfW 6-byte text-chunk header. Each entry corresponds 1:1
    /// with the same-indexed raw payload; bodies that fail to parse
    /// (shorter than the 6-byte fixed header) are dropped from the
    /// returned `Vec` rather than aborting the call. Returns an empty
    /// `Vec` for unknown `stream_index`.
    ///
    /// Pairs with [`crate::muxer::AviMuxer::with_text_chunk_typed`]
    /// for the typed muxer side; a writer → reader cycle preserves
    /// `codepage` / `language` / `dialect` / the body bytes (UTF-8
    /// for codepage `0` / `65001`, Latin-1 byte-pass-through
    /// otherwise — see [`TextChunk`] struct docs for the round-trip
    /// caveat on non-UTF-8 pages).
    pub fn text_chunk_typed(&self, stream_index: u32) -> Vec<TextChunk> {
        self.text_chunk_data
            .get(stream_index as usize)
            .map(|v| v.iter().filter_map(|b| TextChunk::parse(b)).collect())
            .unwrap_or_default()
    }

    /// Lazy [`TextChunk`] iterator over every `xxtx` chunk attached
    /// to `stream_index` (round-15 candidate 3).
    ///
    /// Mirrors [`Self::text_chunk_typed`] but yields one
    /// `Result<TextChunk>` per `next()` call instead of materialising
    /// the full `Vec` up front. Useful for long-running subtitle /
    /// cuepoint streams where the eager `Vec` form would clone every
    /// `String` body even when the consumer only needs to walk once.
    ///
    /// Each `next()` returns:
    /// - `Some(Ok(tc))` for a successfully decoded text chunk,
    /// - `Some(Err(_))` for a body shorter than the 6-byte VfW header
    ///   — the iterator advances past the bad body so subsequent
    ///   `next()` calls keep yielding,
    /// - `None` once every chunk for the requested stream is consumed
    ///   (or immediately for an unknown `stream_index`).
    ///
    /// Implements [`ExactSizeIterator`] so callers can pre-allocate a
    /// sink Vec without first counting; mirrors the round-14
    /// [`Self::palette_change_typed_iter`] pattern symmetrically.
    pub fn text_chunk_typed_iter(&self, stream_index: u32) -> TextChunkTypedIter<'_> {
        let bodies = self
            .text_chunk_data
            .get(stream_index as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        TextChunkTypedIter { bodies, next: 0 }
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

    /// `AVIMAINHEADER.dwPaddingGranularity` per AVI 1.0 §"AVIMAINHEADER"
    /// (round-92).
    ///
    /// Returns the alignment-in-bytes value the muxer promised for
    /// `movi` packet chunks, or `0` (the legacy sentinel) when the
    /// file declared no alignment guarantee. A typical stream-aligned
    /// remux carries 512 / 2048 / 4096 here; the spec says *"Pad the
    /// data to multiples of this value"* and pairs this field with
    /// `JUNK` chunk insertion per §"Other Data Chunks".
    ///
    /// Round-trips byte-equal with
    /// [`crate::muxer::AviMuxOptions::with_padding_granularity`]. Same
    /// data also surfaces under the `avi:padding_granularity`
    /// metadata key (omitted entirely when the value is 0 so absence
    /// of the key is observable).
    pub fn padding_granularity(&self) -> u32 {
        self.avih_padding_granularity
    }

    /// `AVIMAINHEADER.dwInitialFrames` per AVI 1.0 §"AVIMAINHEADER"
    /// (round-157).
    ///
    /// Returns the file-global interleave-skew DWORD from byte
    /// offset 16 of the 56-byte AVIMAINHEADER body, or `None` when
    /// the file declared the documented "noninterleaved file"
    /// sentinel (`dwInitialFrames == 0`). Per
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A:
    /// *"Initial frame for interleaved files. Noninterleaved files
    /// should specify zero. If creating interleaved files, specify
    /// the number of frames in the file prior to the initial frame
    /// of the AVI sequence."*
    ///
    /// This is the file-global counterpart of the per-stream
    /// [`Self::stream_initial_frames`] DWORD (round-153, at byte
    /// offset 16 of each AVISTREAMHEADER). The two fields are
    /// independent — Microsoft writers typically stamp the
    /// per-stream value with the leading-frame count and leave the
    /// file-global one at `0`, but the spec allows either to carry
    /// the skew and the demuxer surfaces both verbatim.
    ///
    /// Round-trips byte-equal with
    /// [`crate::muxer::AviMuxOptions::with_initial_frames`]. Same
    /// data also surfaces under the `avi:initial_frames` metadata
    /// key (omitted entirely when the value is 0 so absence of the
    /// key is observable).
    pub fn initial_frames(&self) -> Option<u32> {
        if self.avih_initial_frames == 0 {
            None
        } else {
            Some(self.avih_initial_frames)
        }
    }

    /// `AVIMAINHEADER.dwMicroSecPerFrame` per AVI 1.0 §"AVIMAINHEADER"
    /// (round-256).
    ///
    /// Returns the file-global frame-period DWORD from byte offset 0
    /// of the 56-byte AVIMAINHEADER body, or `None` when the file
    /// declared the writer-skips-it sentinel
    /// (`dwMicroSecPerFrame == 0`). Per
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A:
    /// *"Number of microseconds between frames. Indicates the overall
    /// timing for the file."*
    ///
    /// This is the file-global frame-period hint. Most legitimate AVIs
    /// derive it from the first video stream's `(dwScale, dwRate)`
    /// pair as `1_000_000 * scale / rate`; the per-stream pair stays
    /// authoritative for individual stream timing and is surfaced
    /// independently via [`Self::stream_timebase`] (round-249). The
    /// two surfaces can disagree — a capture pipeline may stamp a
    /// non-standard frame period here, or leave it `0` even when the
    /// per-stream pair is populated — and the demuxer reports both
    /// verbatim so a downstream tool can detect (or repair) any
    /// mismatch.
    ///
    /// Internally the demuxer also folds this DWORD into the
    /// `duration_micros = total_frames * micro_sec_per_frame`
    /// computation surfaced via `Demuxer::duration`; this raw accessor
    /// keeps the on-disk byte pattern observable for round-trip parity
    /// independent of the derived duration.
    ///
    /// Round-trips byte-equal with
    /// [`crate::muxer::AviMuxOptions::with_micro_sec_per_frame`]. Same
    /// data also surfaces under the `avi:micro_sec_per_frame` metadata
    /// key (omitted entirely when the value is 0 so absence of the key
    /// is observable).
    pub fn micro_sec_per_frame(&self) -> Option<u32> {
        if self.avih_micro_sec_per_frame == 0 {
            None
        } else {
            Some(self.avih_micro_sec_per_frame)
        }
    }

    /// `AVIMAINHEADER.dwMaxBytesPerSec` per AVI 1.0 §"AVIMAINHEADER"
    /// (round-260).
    ///
    /// Returns the file-global maximum-data-rate DWORD from byte
    /// offset 4 of the 56-byte AVIMAINHEADER body, or `None` when the
    /// file declared the writer-skips-it sentinel
    /// (`dwMaxBytesPerSec == 0`). Per
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A:
    /// *"Approximate maximum data rate of the file. Number of bytes
    /// per second the system must handle to present an AVI sequence as
    /// specified by the other parameters in the main header and stream
    /// header chunks."*
    ///
    /// This is the file-global data-rate hint a capture-card player
    /// uses to size its disk-read pacing. Most legitimate AVIs derive
    /// it from `sum(per_track_total_bytes) / file_duration_seconds`;
    /// the muxer's [`crate::muxer::AviMuxOptions::with_max_bytes_per_sec`]
    /// builder lets the caller override the computed value verbatim
    /// (round-14 candidate 1). Pre-round-260 the raw DWORD was already
    /// surfaced via the `avi:max_bytes_per_sec` metadata key, but no
    /// typed accessor was offered — round-260 closes that gap.
    ///
    /// The accessor is independent of any per-stream rate the demuxer
    /// also tracks (per-stream `dwScale` / `dwRate` via
    /// [`Self::stream_timebase`] from round-249); the file-global
    /// value can disagree with the sum of per-stream rates (e.g. a
    /// capture pipeline that stamped a conservative ceiling) and the
    /// demuxer surfaces both verbatim so a downstream tool can detect
    /// or repair any mismatch.
    ///
    /// Round-trips byte-equal with
    /// [`crate::muxer::AviMuxOptions::with_max_bytes_per_sec`]. Same
    /// data also surfaces under the `avi:max_bytes_per_sec` metadata
    /// key (omitted entirely when the value is 0 so absence of the key
    /// is observable).
    pub fn max_bytes_per_sec(&self) -> Option<u32> {
        if self.avih_max_bytes_per_sec == 0 {
            None
        } else {
            Some(self.avih_max_bytes_per_sec)
        }
    }

    /// `AVIMAINHEADER.dwTotalFrames` per AVI 1.0 §"AVIMAINHEADER"
    /// (round-268).
    ///
    /// Returns the file-global frame-count DWORD from byte offset 16
    /// of the 56-byte AVIMAINHEADER body, or `None` when the file
    /// declared the writer-skips-it / empty-file sentinel
    /// (`dwTotalFrames == 0`). Per
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A:
    /// *"Total number of frames of data in the file."*
    ///
    /// Internally the demuxer already consumes this DWORD to derive
    /// `duration_micros = total_frames * micro_sec_per_frame` (the
    /// source of `Demuxer::duration`); this raw accessor keeps the
    /// on-disk byte pattern observable independent of the derived
    /// duration, matching the shape of [`Self::micro_sec_per_frame`]
    /// (round-256) / [`Self::max_bytes_per_sec`] (round-260).
    ///
    /// For a multi-segment OpenDML file this field only carries the
    /// **primary** `RIFF AVI ` segment's frame count (per OpenDML 2.0
    /// §5.0 — `AVIX` continuation packets are invisible to an AVI 1.0
    /// reader, so writers stamp only what such a reader can see); the
    /// cross-segment truth is the separate `dmlh.dwTotalFrames`
    /// surfaced via [`Self::dmlh_total_frames`]. The two values are
    /// spec-independent and the demuxer reports both verbatim so a
    /// downstream tool can detect (or repair) any mismatch — the
    /// existing [`Self::super_index_duration_violations`] cross-check
    /// validates the dmlh side against the super-index, not this one.
    ///
    /// The muxer's counterpart is auto-derived: `write_trailer`
    /// patches the first video stream's emitted packet count into
    /// this DWORD (first-track packet count for video-less files; no
    /// override builder exists — the file-global stamp tracks actual
    /// emitted packets). Same data also surfaces under
    /// the `avi:total_frames` metadata key (omitted entirely when the
    /// value is 0 so absence of the key is observable).
    pub fn avih_total_frames(&self) -> Option<u32> {
        if self.avih_total_frames == 0 {
            None
        } else {
            Some(self.avih_total_frames)
        }
    }

    /// `AVIMAINHEADER.dwStreams` per AVI 1.0 §"AVIMAINHEADER"
    /// (round-292).
    ///
    /// Returns the file-global writer-declared stream count from byte
    /// offset 24 of the 56-byte AVIMAINHEADER body, or `None` when the
    /// file declared the writer-skips-it / unspecified sentinel
    /// (`dwStreams == 0`). Per
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A:
    /// *"Number of streams in the file. For example, a file with audio
    /// and video has two streams."*
    ///
    /// This is the count the **writer claimed**, not the number of
    /// `strl` LISTs the demuxer actually walked in `hdrl` — those agree
    /// for a well-formed file but can diverge for a truncated capture
    /// crash dump or a hand-edited header (the on-disk DWORD says "2
    /// streams" while only one `strl` is physically present). The
    /// number of streams actually parsed is the length of
    /// [`Demuxer::streams`]; [`Self::declared_vs_actual_stream_count_mismatch`]
    /// surfaces the `(declared, actual)` pair whenever the two disagree.
    /// This accessor keeps the raw declared DWORD observable on its own,
    /// matching the shape of [`Self::avih_total_frames`] (round-268) /
    /// [`Self::max_bytes_per_sec`] (round-260) / [`Self::micro_sec_per_frame`]
    /// (round-256). The same data also surfaces under the `avi:streams`
    /// metadata key (omitted entirely when the value is 0 so absence of
    /// the key is observable).
    ///
    /// The muxer's counterpart is auto-derived: `write_header` stamps
    /// the actual number of streams passed to `open_muxer`, so a
    /// round-trip through this crate's own writer always agrees with
    /// [`Demuxer::streams`]`.len()`.
    pub fn avih_declared_stream_count(&self) -> Option<u32> {
        if self.avih_streams == 0 {
            None
        } else {
            Some(self.avih_streams)
        }
    }

    /// Cross-check of `AVIMAINHEADER.dwStreams` against the number of
    /// `strl` LISTs actually walked in `hdrl` (round-292).
    ///
    /// Returns `Some((declared, actual))` when the writer-declared
    /// stream count (byte offset 24 of the AVIMAINHEADER body) is
    /// non-zero **and** disagrees with the number of streams the
    /// demuxer physically parsed (the length of [`Demuxer::streams`]),
    /// otherwise `None`. A non-zero declared count that matches the
    /// parsed count, or the writer-skips-it `0` sentinel (which carries
    /// no claim to validate against), both return `None`.
    ///
    /// This is an informational diagnostic in the same family as
    /// [`Self::super_index_duration_violations`] /
    /// [`Self::cbr_audio_block_alignment_violations`]: it never fails
    /// `open()`. A mismatch is a hallmark of a truncated capture crash
    /// dump (the header was stamped up-front for N streams but the file
    /// was cut off before all N `strl` LISTs were written) or a
    /// hand-edited / repacked header; a downstream repair tool can use
    /// the pair to decide whether to trust the declared count or the
    /// physically-present streams. The demuxer always trusts the
    /// streams it actually parsed — `dwStreams` is advisory.
    pub fn declared_vs_actual_stream_count_mismatch(&self) -> Option<(u32, u32)> {
        if self.avih_streams == 0 {
            return None;
        }
        let actual = self.streams.len() as u32;
        if self.avih_streams != actual {
            Some((self.avih_streams, actual))
        } else {
            None
        }
    }

    /// `AVIMAINHEADER.dwWidth` / `dwHeight` movie rectangle per AVI 1.0
    /// §"AVIMAINHEADER" (round-275).
    ///
    /// Returns the file-global movie-rectangle dimensions as a
    /// `(width, height)` pair from byte offsets 32 + 36 of the 56-byte
    /// AVIMAINHEADER body, or `None` when either DWORD is the
    /// writer-skips-it / unspecified `0` sentinel. Per
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A:
    /// *"`dwWidth` — Width of the AVI file in pixels."* and *"`dwHeight`
    /// — Height of the AVI file in pixels."*
    ///
    /// This is the file-global rectangle the per-stream
    /// `strh.rcFrame` destination rectangle surfaced via
    /// [`Self::stream_frame_rect`] (round-119) is expressed relative
    /// to: per the spec's `rcFrame` row, the destination rectangle's
    /// upper-left corner is *"relative to the upper-left corner of the
    /// movie rectangle specified by the `dwWidth` and `dwHeight`
    /// members of the AVI main header structure."* A reader laying out
    /// multiple video / text streams onto a composite surface needs
    /// this pair to position each stream's `rcFrame` correctly.
    ///
    /// The dimensions are logically distinct from any single video
    /// stream's coded `BITMAPINFOHEADER.biWidth` / `biHeight`
    /// (surfaced through `Demuxer::streams` per-stream
    /// `CodecParameters`): the movie rectangle is the overall
    /// composition canvas, which for a single-video file equals that
    /// stream's frame size but for a multi-video file is the union
    /// rectangle. The demuxer surfaces the raw `avih` DWORDs verbatim
    /// with no cross-validation against any stream's coded size.
    ///
    /// Either dimension being `0` collapses the whole pair to `None`
    /// (a movie rectangle is only meaningful when both dimensions are
    /// non-zero), matching the round-249 `stream_timebase` "zero in
    /// either DWORD ⇒ `None`" shape. The same data also surfaces under
    /// the `avi:width` / `avi:height` metadata keys, which are emitted
    /// verbatim including `0` — this typed accessor adds the
    /// "default == absent" mapping the metadata keys don't, mirroring
    /// the round-268 / round-260 / round-256 convention.
    ///
    /// The muxer's counterpart is auto-derived from the first video
    /// stream's coded dimensions at `write_header`; no override builder
    /// exists, so the file-global rectangle tracks the leading video
    /// stream's frame size.
    pub fn avih_movie_rect(&self) -> Option<(u32, u32)> {
        if self.avih_width == 0 || self.avih_height == 0 {
            None
        } else {
            Some((self.avih_width, self.avih_height))
        }
    }

    /// `AVIMAINHEADER.dwReserved[4]` trailing reserved array per AVI 1.0
    /// §"AVIMAINHEADER" (round-330).
    ///
    /// Returns the four trailing DWORDs from byte offsets 40..56 of the
    /// 56-byte AVIMAINHEADER body verbatim, or `None` when the array is
    /// the spec-conformant all-zero default. Per
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix A
    /// (`dwReserved` row): *"Reserved. Set this array to zero."*
    ///
    /// A spec-conformant writer leaves all four DWORDs `0`, so this
    /// accessor returns `None` for every well-formed file. It returns
    /// `Some([w0, w1, w2, w3])` only for a non-conformant header — a
    /// hand-edited / capture-card / vendor-extended AVI that smuggled
    /// data into the reserved slot — which lets a forensic / repair tool
    /// detect (and, if it chooses, scrub) the stray bytes before
    /// trusting the header. The whole array is returned even when only
    /// one DWORD is non-zero, so the caller sees the exact on-disk
    /// pattern.
    ///
    /// A short (`< 56`-byte) `avih` body — some truncated capture
    /// crash dumps stamp only the first 40 bytes — yields the all-zero
    /// default and therefore `None`, so an absent reserved array reads
    /// the same as a zeroed one. This mirrors the "default == absent"
    /// convention used across the sibling `avih` accessors
    /// ([`Self::micro_sec_per_frame`], [`Self::avih_total_frames`],
    /// [`Self::initial_frames`]). The same data also surfaces under the
    /// `avi:reserved` metadata key (comma-joined `0x`-hex), emitted only
    /// when any DWORD is non-zero.
    pub fn avih_reserved(&self) -> Option<[u32; 4]> {
        if self.avih_reserved.iter().all(|&w| w == 0) {
            None
        } else {
            Some(self.avih_reserved)
        }
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

    /// Typed [`Idx1Flags`] decode of one idx1 entry's `dwFlags`
    /// DWORD (round-17 candidate 3).
    ///
    /// Returns `Some(Idx1Flags)` for the `packet_seq`-th idx1 entry
    /// (zero-based, in idx1 file order) belonging to `stream_index`,
    /// or `None` for an out-of-range index, an unknown stream, or
    /// when the file had no `idx1`. Decodes the same raw u32 the
    /// untyped [`Self::idx1_flags_for_packet`] hands back, but
    /// surfaces the documented `AVIIF_*` bits as boolean fields and
    /// exposes the compressor-private upper bits through
    /// [`Idx1Flags::compressor_bits`].
    pub fn idx1_typed_flags_for_packet(
        &self,
        stream_index: u32,
        packet_seq: usize,
    ) -> Option<Idx1Flags> {
        self.idx1_flags_for_packet(stream_index, packet_seq)
            .map(Idx1Flags::from_bits)
    }

    /// `rec ` LIST entries recorded in idx1, in file order (round-285).
    ///
    /// Per AVI 1.0 §"AVI Index Entries" idx1 holds "entries for each
    /// data chunk, including 'rec ' chunks" — one per `LIST rec `
    /// CD-ROM-interleave grouping cluster inside `movi`, flagged
    /// `AVIIF_LIST` per Appendix C. They carry no stream index, so the
    /// per-stream surfaces ([`Self::idx1_flags_for_packet`], the seek
    /// table, `next_packet`) never see them; this accessor keeps the
    /// recorded `(flags, offset, size)` triples observable verbatim,
    /// with each `offset` resolved file-absolute via the same
    /// movi-relative / file-absolute base detection the seek table
    /// uses. Empty slice when the file has no idx1, or its idx1 indexes
    /// no `rec ` lists (the common case — writers that don't cluster,
    /// and pre-round-285 cluster-writing muxers that indexed only the
    /// grouped chunks, both leave it empty).
    pub fn idx1_rec_list_entries(&self) -> &[Idx1RecEntry] {
        &self.idx1_rec_entries
    }

    /// Convenience count of [`Self::idx1_rec_list_entries`] — the
    /// number of `LIST rec ` clusters idx1 declares. Mirrors the
    /// `avi:idx1.rec_lists` metadata key (which is only emitted when
    /// this count is non-zero, so key absence == `0`).
    pub fn idx1_rec_list_count(&self) -> u32 {
        self.idx1_rec_entries.len().min(u32::MAX as usize) as u32
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

    /// Per-stream `ix##` standard-index `qwBaseOffset` values
    /// (round-317).
    ///
    /// Per AVISTDINDEX (clean-room source:
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix G,
    /// `qwBaseOffset` row: *"Base offset (typically the file offset of
    /// the 'movi' list)."*), each `ix##` standard-index chunk carries a
    /// 64-bit base offset; every `AVISTDINDEX_ENTRY.dwOffset` is added to
    /// it to recover the file-absolute position of the indexed data
    /// chunk. For an OpenDML multi-segment file each stream has one `ix##`
    /// per `movi` segment, so this returns one entry per segment in file
    /// order; an AVI-1.0 / no-`ix##` file yields an empty Vec.
    ///
    /// The bytes are surfaced verbatim (no normalisation): the demuxer's
    /// own OpenDML seek path resolves chunk positions from this same raw
    /// value, so a caller can compare what the index declared against
    /// where the data physically landed (see
    /// [`Self::std_index_base_offset_violations`] for the `movi`-region
    /// cross-check). Distinct from the super-index `_avisuperindex_entry.
    /// qwOffset` (which is the file-absolute position of each `ix##` chunk
    /// itself, not the base its entries resolve against).
    pub fn std_index_base_offsets(&self, stream_index: u32) -> Vec<u64> {
        let mut out = Vec::new();
        for ix in &self.std_indexes {
            match parse_stream_index(&ix.chunk_id) {
                Some(s) if s == stream_index => out.push(ix.qw_base_offset),
                _ => {}
            }
        }
        out
    }

    /// Raw `dwChunkId` FOURCC of every `ix##` standard-index segment for
    /// a stream, in file order (round-322).
    ///
    /// Per AVISTDINDEX (clean-room source:
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix G, the
    /// `dwChunkId` row: *"FOURCC of indexed chunks."*) and the base
    /// AVIMETAINDEX in Appendix E (`dwChunkId` row: *"FOURCC of chunks
    /// indexed (e.g., '00dc')."*), each `ix##` standard-index chunk
    /// declares which `movi` data-chunk FOURCC its
    /// `AVISTDINDEX_ENTRY.dwOffset` entries point at. For a well-formed
    /// AVI 2.0 file it spells the indexed stream's own packet FourCC —
    /// `00dc` / `00wb` for stream 0, `01dc` / `01wb` for stream 1, and so
    /// forth — so its two leading ASCII digits encode the same stream
    /// number the demuxer keyed the `ix##` under (the stream this `Vec`
    /// is keyed by, matching [`Self::std_index_base_offsets`]). For an
    /// OpenDML multi-segment file each stream has one `ix##` per `movi`
    /// segment, so this returns one FOURCC per segment in file order —
    /// index-aligned with `std_index_base_offsets` for the same stream.
    ///
    /// The bytes are surfaced verbatim (no normalisation), so a reader
    /// can detect a cross-wired / malformed standard index whose declared
    /// `dwChunkId` points at a *different* stream's chunks than the `ix##`
    /// FourCC the demuxer keyed it under, *before* trusting the
    /// `dwOffset`-from-`qwBaseOffset` arithmetic that backs the OpenDML
    /// seek path. The companion `avi:ix.<n>.<seg>.chunk_id` metadata key
    /// surfaces the same value as a printable-or-hex string but only when
    /// it diverges from the canonical own-slot FOURCC; this accessor
    /// always returns the bytes so a caller can cross-check the canonical
    /// case too.
    ///
    /// Distinct from the super-index `dwChunkId` surfaced via
    /// [`Self::super_index_chunk_id`] (one per stream, declared once in
    /// the `strl`) — this is the per-segment standard-index flavour, one
    /// FOURCC per `ix##` chunk in `movi`. Returns an empty `Vec` for an
    /// AVI-1.0 / no-`ix##` file or an out-of-range `stream_index`.
    pub fn std_index_chunk_ids(&self, stream_index: u32) -> Vec<[u8; 4]> {
        let mut out = Vec::new();
        for ix in &self.std_indexes {
            match parse_stream_index(&ix.chunk_id) {
                Some(s) if s == stream_index => out.push(ix.chunk_id),
                _ => {}
            }
        }
        out
    }

    /// Cross-check each `ix##` standard-index `qwBaseOffset` against the
    /// file's `movi` LIST regions (round-317).
    ///
    /// Per AVISTDINDEX (Appendix G, `qwBaseOffset` row: *"Base offset
    /// (typically the file offset of the 'movi' list)."*) the base offset
    /// every `ix##` entry resolves against is expected to point inside
    /// the enclosing `movi` LIST. This method returns one
    /// [`StdIndexBaseOffsetViolation`] per `ix##` chunk whose
    /// `qwBaseOffset` falls outside **every** `movi` segment range the
    /// demuxer walked (`[start, end)` half-open, where `start` is the
    /// `movi` LIST body's first chunk-header offset and `end` its body
    /// end). A base sitting exactly at a segment's `end` is treated as
    /// outside, matching the half-open `[start, end)` convention used by
    /// the idx1↔ix## cross-validator.
    ///
    /// The check fires for both AVI-1.0-embedded `ix##` (rare) and
    /// OpenDML multi-segment files; a stream's `ix##` whose base anchors
    /// inside its segment yields no violation. The validator is purely
    /// informational — it never affects `open()` and the seek path keeps
    /// using the verbatim `qwBaseOffset` regardless, so a malformed base
    /// stays observable rather than being silently rewritten. An empty
    /// return therefore means "every `ix##` base anchors inside a `movi`
    /// region", which includes the common case of a file with no
    /// standard indexes at all.
    pub fn std_index_base_offset_violations(&self) -> Vec<StdIndexBaseOffsetViolation> {
        let mut out = Vec::new();
        // Per-stream running ix## ordinal so `segment_index` counts
        // across every ix## chunk for that stream in file order
        // (mirrors `field2_offset_for_packet`'s per-stream walk).
        let mut seg_per_stream: std::collections::BTreeMap<u32, usize> =
            std::collections::BTreeMap::new();
        for ix in &self.std_indexes {
            let stream = match parse_stream_index(&ix.chunk_id) {
                Some(s) => s,
                None => continue,
            };
            let seg = seg_per_stream.entry(stream).or_default();
            let segment_index = *seg;
            *seg += 1;
            let inside_movi = self
                .movi_segments
                .iter()
                .any(|&(start, end)| ix.qw_base_offset >= start && ix.qw_base_offset < end);
            if !inside_movi {
                out.push(StdIndexBaseOffsetViolation {
                    stream_index: stream,
                    segment_index,
                    qw_base_offset: ix.qw_base_offset,
                });
            }
        }
        out
    }

    /// Raw `nEntriesInUse` declared by every `ix##` standard-index segment
    /// for a stream, in file order (round-325).
    ///
    /// Per AVISTDINDEX (clean-room source:
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix G) and the
    /// base AVIMETAINDEX in Appendix E (`nEntriesInUse` row: *"Number of
    /// valid entries in adwIndex."*), this DWORD declares how many
    /// `AVISTDINDEX_ENTRY` records the standard-index chunk holds. The
    /// value is surfaced verbatim from the chunk header, so for a truncated
    /// chunk it can exceed the number of entries the demuxer actually
    /// parsed (compare against [`Self::std_index_entry_count_violations`]).
    ///
    /// For an OpenDML multi-segment file each stream has one `ix##` per
    /// `movi` segment, so this returns one count per segment in file order —
    /// index-aligned with [`Self::std_index_base_offsets`] and
    /// [`Self::std_index_chunk_ids`] for the same stream. Returns an empty
    /// `Vec` for an AVI-1.0 / no-`ix##` file or an out-of-range
    /// `stream_index`.
    pub fn std_index_declared_entry_counts(&self, stream_index: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for ix in &self.std_indexes {
            match parse_stream_index(&ix.chunk_id) {
                Some(s) if s == stream_index => out.push(ix.declared_n_entries),
                _ => {}
            }
        }
        out
    }

    /// Cross-check each `ix##` standard-index's declared `nEntriesInUse`
    /// against the number of entries the demuxer could physically parse
    /// (round-325).
    ///
    /// Per AVISTDINDEX (Appendix G / the base AVIMETAINDEX in Appendix E,
    /// `nEntriesInUse` row: *"Number of valid entries in adwIndex."*) a
    /// well-formed `ix##` chunk body holds exactly `nEntriesInUse` entries
    /// (8 bytes each, or 12 for an `AVI_INDEX_SUB_2FIELD` field index). A
    /// truncated capture crash-dump or hand-edited file can stamp a larger
    /// `nEntriesInUse` than the body physically contains; the demuxer
    /// parses the entries it can read (rather than discarding the whole
    /// chunk) and returns one [`StdIndexEntryCountViolation`] per `ix##`
    /// whose declared count exceeds the parsed count.
    ///
    /// The check is purely informational — it never affects `open()` and
    /// the seek path resolves chunk positions only from the entries that
    /// were actually parsed, so a truncated standard index stays usable for
    /// the data it does cover while the loss stays observable. An empty
    /// return means every `ix##` carried as many entries as it declared,
    /// which includes the common case of a file with no standard indexes at
    /// all. The companion `avi:ix.<stream>.<segment>.declared_entries`
    /// metadata key surfaces the same divergence as a string, omitting the
    /// well-formed case so absence stays observable.
    pub fn std_index_entry_count_violations(&self) -> Vec<StdIndexEntryCountViolation> {
        let mut out = Vec::new();
        let mut seg_per_stream: std::collections::BTreeMap<u32, usize> =
            std::collections::BTreeMap::new();
        for ix in &self.std_indexes {
            let stream = match parse_stream_index(&ix.chunk_id) {
                Some(s) => s,
                None => continue,
            };
            let seg = seg_per_stream.entry(stream).or_default();
            let segment_index = *seg;
            *seg += 1;
            let parsed = ix.entries.len() as u32;
            if ix.declared_n_entries > parsed {
                out.push(StdIndexEntryCountViolation {
                    stream_index: stream,
                    segment_index,
                    declared_entries: ix.declared_n_entries,
                    parsed_entries: parsed,
                });
            }
        }
        out
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

    /// Cross-check every CBR-audio `ix##` standard-index entry's
    /// `dwSize` against the stream's `WAVEFORMATEX.nBlockAlign`
    /// (round-96).
    ///
    /// Per OpenDML 2.0 §3.0 ("AVI Standard Index Chunk") each
    /// `AVISTDINDEX_ENTRY.dwSize` is the byte length of the indexed
    /// data chunk. A constant-bit-rate audio stream (PCM / A-law /
    /// µ-law / IMA-ADPCM, per the AVI 1.0 sample-size invariant in
    /// [`classify_audio_sample_size`]) stores a whole number of
    /// `nBlockAlign` sample blocks per chunk, so a conformant index
    /// satisfies `dwSize % nBlockAlign == 0` for every entry. This
    /// method returns one [`BlockAlignViolation`] per entry that
    /// breaks the rule.
    ///
    /// The check is scoped to OpenDML standard indexes (`ix##`), which
    /// are the only place the entry size is recorded independently of
    /// the chunk header — a legacy `idx1`-only AVI 1.0 file has no
    /// `ix##` chunks and yields an empty Vec. VBR streams, video /
    /// data streams, and CBR streams whose WAVEFORMATEX carried a
    /// `nBlockAlign` of 0 or 1 are skipped (nothing to validate
    /// against). An empty return means "no `ix##`-indexed CBR-audio
    /// misalignment found", which includes the common case of a file
    /// with no standard indexes at all.
    ///
    /// This validator is informational and never affects `open()`:
    /// the coarse VBR/CBR sample-size invariant is already enforced at
    /// open time (or skipped via `open_lenient`); this is the finer,
    /// index-level companion a caller invokes when it wants to trust
    /// `ix##` offsets for sample-accurate audio seeking.
    pub fn cbr_audio_block_alignment_violations(&self) -> Vec<BlockAlignViolation> {
        let mut out = Vec::new();
        // Per-stream running entry ordinal so `entry_index` counts
        // across every ix## chunk for that stream in file order
        // (mirrors `field2_offset_for_packet`'s per-stream walk).
        let mut seen_per_stream: Vec<usize> = vec![0; self.audio_cbr_block_aligns.len()];
        for ix in &self.std_indexes {
            let stream = match parse_stream_index(&ix.chunk_id) {
                Some(s) => s,
                None => continue,
            };
            let block_align = match self
                .audio_cbr_block_aligns
                .get(stream as usize)
                .copied()
                .flatten()
            {
                Some(ba) => ba,
                // Not a CBR audio stream with a meaningful nBlockAlign.
                None => continue,
            };
            let base = seen_per_stream
                .get(stream as usize)
                .copied()
                .unwrap_or_default();
            for (local, entry) in ix.entries.iter().enumerate() {
                if entry.dw_size % block_align as u32 != 0 {
                    out.push(BlockAlignViolation {
                        stream_index: stream,
                        entry_index: base + local,
                        dw_size: entry.dw_size,
                        block_align,
                    });
                }
            }
            if let Some(slot) = seen_per_stream.get_mut(stream as usize) {
                *slot = base.saturating_add(ix.entries.len());
            }
        }
        out
    }

    /// Per-segment `_avisuperindex_entry.dwDuration` values from a
    /// stream's `indx` super-index (round-101).
    ///
    /// Per OpenDML 2.0 §"AVI Super Index Chunk", each super-index entry
    /// points at one `ix##` standard index covering one `RIFF AVIX`
    /// segment and records that segment's `dwDuration` — *"time span in
    /// stream ticks"*. This returns those values in segment order (one
    /// per used super-index entry; entries with `qwOffset == 0`, the
    /// spec's "unused entry" sentinel, are dropped at parse time and so
    /// never appear here).
    ///
    /// Returns an empty `Vec` when the stream carries no `indx`
    /// super-index (the common AVI-1.0 / single-`RIFF` case), when
    /// `stream_index` is out of range, or when the super-index declared
    /// zero used entries. Callers wanting to validate the totals against
    /// the extended header should prefer
    /// [`AviDemuxer::super_index_duration_violations`].
    pub fn super_index_segment_durations(&self, stream_index: u32) -> Vec<u32> {
        match self.super_indexes.get(stream_index as usize) {
            Some(sx) => sx.entries.iter().map(|e| e.dw_duration).collect(),
            None => Vec::new(),
        }
    }

    /// Cross-check each stream's `indx` super-index `dwDuration` total
    /// against the file's `dmlh.dwTotalFrames` (round-101).
    ///
    /// Per OpenDML 2.0 §"AVI Super Index Chunk" every
    /// `_avisuperindex_entry.dwDuration` is the per-segment frame span
    /// in stream ticks, and §5.0 ("Extended AVI Header") defines
    /// `dmlh.dwTotalFrames` as the file's real total frame count across
    /// all `RIFF AVIX` segments. For a one-tick-per-frame video stream —
    /// the canonical OpenDML video case, and exactly what this crate's
    /// muxer emits — the super-index entries partition that total, so
    /// `sum(dwDuration) == dmlh.dwTotalFrames` must hold. This method
    /// returns one [`SuperIndexDurationViolation`] per video stream
    /// whose sum disagrees.
    ///
    /// The check only fires when **both** independently-recorded counts
    /// are present: a non-empty `indx` super-index for the stream **and**
    /// a `dmlh` extended header for the file. Streams without a
    /// super-index, files without a `dmlh`, and non-video streams (whose
    /// `dwDuration` ticks need not be one-per-frame) are skipped and
    /// yield no violation. An empty return therefore means "every video
    /// super-index that can be compared agrees with `dmlh`", which
    /// includes the common case of a file that records only one of the
    /// two.
    ///
    /// Like [`AviDemuxer::cbr_audio_block_alignment_violations`], this
    /// validator is purely informational: it never affects `open()`. It
    /// complements the existing `avi:indx.<n>.overflow_entries` signal
    /// (which flags a super-index with more segments than reserved
    /// slots) with a value-level consistency check between the index and
    /// the extended header.
    pub fn super_index_duration_violations(&self) -> Vec<SuperIndexDurationViolation> {
        let mut out = Vec::new();
        let Some(dmlh) = self.dmlh_total_frames else {
            // No extended header to compare against.
            return out;
        };
        let dmlh_total = dmlh as u64;
        for (i, sx) in self.super_indexes.iter().enumerate() {
            if sx.entries.is_empty() {
                continue;
            }
            // Scope to video streams: their per-segment dwDuration is the
            // frame count, the same unit dmlh.dwTotalFrames carries.
            // Audio super-index ticks are sample/block spans, not frames.
            let is_video = self
                .streams
                .get(i)
                .map(|s| matches!(s.params.media_type, MediaType::Video))
                .unwrap_or(false);
            if !is_video {
                continue;
            }
            let total: u64 = sx
                .entries
                .iter()
                .fold(0u64, |acc, e| acc.saturating_add(e.dw_duration as u64));
            if total != dmlh_total {
                out.push(SuperIndexDurationViolation {
                    stream_index: i as u32,
                    super_index_duration_total: total,
                    dmlh_total_frames: dmlh_total,
                });
            }
        }
        out
    }

    /// Raw `bIndexSubType` byte of a stream's `indx` super-index
    /// (round-197).
    ///
    /// Per AVISUPERINDEX (clean-room source:
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix F,
    /// `bIndexSubType` row): *"The index subtype. The value must be
    /// zero or AVI_INDEX_SUB_2FIELD."* `AVI_INDEX_SUB_2FIELD` is
    /// `0x01` per Appendix E §"Sub-types" — the super-index inherits
    /// it from the pointed-to per-segment `ix##` standard indexes so
    /// a reader can detect 2-field interlaced carriage from the
    /// `strl`-level super index *before* opening the `movi` body.
    ///
    /// Returns the raw u8 verbatim (no normalisation: the spec pins
    /// only two values but the demuxer surfaces whatever the file
    /// carried). Returns `None` for `stream_index` out of range or
    /// streams that didn't declare an `indx` super-index (the AVI-1.0
    /// case and OpenDML streams whose `strl` reserved no super-index
    /// slot). The companion [`Self::super_index_is_2field`] folds the
    /// raw byte into a boolean for the common "is this stream
    /// interlaced?" question; this accessor exposes the raw value for
    /// callers that need to round-trip writer-private subtype bytes
    /// or distinguish an explicit `0` declaration from "no super
    /// index at all".
    pub fn super_index_sub_type(&self, stream_index: u32) -> Option<u8> {
        let sx = self.super_indexes.get(stream_index as usize)?;
        if sx.entries.is_empty() {
            // Pad slot (no indx declared) — distinguishable from a
            // genuine super-index whose sub-type happens to be 0.
            return None;
        }
        Some(sx.b_index_sub_type)
    }

    /// True iff the stream's `indx` super-index declares the
    /// `AVI_INDEX_SUB_2FIELD` (= `0x01`) sub-type (round-197).
    ///
    /// Convenience wrapper around [`Self::super_index_sub_type`].
    /// Returns `false` for streams without an `indx` super-index or
    /// whose super-index sub-type byte is the `0` default. This is
    /// the highest-fanout reader-facing signal for interlaced
    /// 2-field carriage on OpenDML files: it requires only the
    /// `strl`-level super-index parse, not the in-`movi` `ix##` scan
    /// that backs [`AviDemuxer::field2_offset_for_packet`] and the
    /// existing `avi:ix.<n>.is_2field` metadata key.
    pub fn super_index_is_2field(&self, stream_index: u32) -> bool {
        self.super_index_sub_type(stream_index) == Some(AVI_INDEX_SUB_2FIELD)
    }

    /// Raw `wLongsPerEntry` WORD of a stream's `indx` super-index
    /// (round-304).
    ///
    /// Per AVISUPERINDEX (clean-room source:
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix F,
    /// `wLongsPerEntry` row: *"4 (each entry is 16 bytes)."*) and the
    /// base AVIMETAINDEX in Appendix E (`wLongsPerEntry` row: *"Size of
    /// each index entry, in 4-byte units."*), this WORD declares the
    /// per-entry stride of the super-index's `aIndex[]` table in units
    /// of 4-byte DWORDs. For a well-formed AVI 2.0 super-index it is
    /// always `4` — each `_avisuperindex_entry` is `(qwOffset:8,
    /// dwSize:4, dwDuration:4)` = 16 bytes = 4 longs — but the demuxer
    /// surfaces whatever value the file declared so a reader can detect
    /// a malformed / future-extended super-index table whose entry
    /// stride differs from the spec's `4` *before* trusting the
    /// 16-byte-stride entry walk in `parse_indx`.
    ///
    /// Returns the raw u16 verbatim (no normalisation). Returns `None`
    /// for `stream_index` out of range or streams that didn't declare
    /// an `indx` super-index (the AVI-1.0 case and OpenDML streams
    /// whose `strl` reserved no super-index slot). This is the
    /// super-index counterpart of the per-segment `ix##`
    /// stride — distinct from the per-stream `(scale, rate)` timebase
    /// and from the `bIndexSubType` exposed via
    /// [`Self::super_index_sub_type`]; the sub-type selects 8-byte vs
    /// 12-byte `ix##` *standard-index* entries, whereas this WORD is
    /// the *super-index's* own entry stride and is independent of the
    /// pointed-to segments' field carriage.
    pub fn super_index_longs_per_entry(&self, stream_index: u32) -> Option<u16> {
        let sx = self.super_indexes.get(stream_index as usize)?;
        if sx.entries.is_empty() {
            // Pad slot (no indx declared) — distinguishable from a
            // genuine super-index, mirroring `super_index_sub_type`.
            return None;
        }
        Some(sx.w_longs_per_entry)
    }

    /// Raw `dwChunkId` FOURCC of a stream's `indx` super-index
    /// (round-312).
    ///
    /// Per AVISUPERINDEX (clean-room source:
    /// `docs/container/riff/avi-riff-file-reference.md` Appendix F,
    /// `dwChunkId` row: *"FOURCC of chunks indexed (e.g., '00dc')."*)
    /// and the base AVIMETAINDEX in Appendix E (`dwChunkId` row: *"FOURCC
    /// of chunks indexed (e.g., '00dc'); for super index only."*), this
    /// DWORD declares which `movi` data-chunk FOURCC every `ix##`
    /// standard-index segment referenced by this super-index points at.
    /// For a well-formed AVI 2.0 file it spells the indexed stream's own
    /// packet FourCC — `00dc` / `00wb` for stream 0, `01dc` / `01wb` for
    /// stream 1, and so forth — i.e. the two leading ASCII digits encode
    /// the same stream number as the `strl` the super-index lives in.
    ///
    /// Returns the raw 4 bytes verbatim (no normalisation), so a reader
    /// can detect a cross-wired / malformed super-index whose declared
    /// `dwChunkId` points at a *different* stream's chunks than the
    /// `strl` it sits in *before* trusting the in-`movi` `ix##` scan that
    /// backs the OpenDML seek path. The companion
    /// `avi:indx.<n>.chunk_id` metadata key surfaces the same value as a
    /// printable-or-hex string but only when it diverges from the
    /// canonical own-slot FOURCC; this accessor always returns the bytes
    /// so a caller can cross-check the canonical case too.
    ///
    /// Returns `None` for `stream_index` out of range or streams that
    /// didn't declare an `indx` super-index (the AVI-1.0 case and
    /// OpenDML streams whose `strl` reserved no super-index slot, plus a
    /// super-index with a non-`AVI_INDEX_OF_INDEXES` `bIndexType` which
    /// `parse_indx` folds to an empty `default()` slot) — mirroring the
    /// `None`-for-no-`indx` shape of [`Self::super_index_sub_type`] and
    /// [`Self::super_index_longs_per_entry`].
    pub fn super_index_chunk_id(&self, stream_index: u32) -> Option<[u8; 4]> {
        let sx = self.super_indexes.get(stream_index as usize)?;
        if sx.entries.is_empty() {
            // Pad slot (no indx declared) — distinguishable from a
            // genuine super-index, mirroring `super_index_sub_type`.
            return None;
        }
        Some(sx.chunk_id)
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

    /// Round-104: per-stream `vprp` active frame aspect ratio, unpacked.
    ///
    /// Returns the OpenDML 2.0 §5.0 *"Active Frame Aspect Ratio"*
    /// (`dwFrameAspectRatio`) as a numeric `(x, y)` pair — the high WORD
    /// is the x term, the low WORD the y term, so `0x0004_0003` decodes
    /// to `(4, 3)` and `0x0010_0009` to `(16, 9)`. This is the typed
    /// companion to the `avi:vprp.<index>.frame_aspect_ratio` metadata
    /// key (which formats the same value as the human-readable string
    /// `"x:y"`); callers wanting to compute a pixel aspect ratio from
    /// the frame width/height — *"This value can be used with the frame
    /// width and height to calculate the pixel aspect ratio"* per
    /// §5.0 — get the two WORDs without parsing the metadata string.
    ///
    /// Returns `None` when the stream carries no `vprp` chunk (presence
    /// gated on `nbFieldPerFrame > 0`, matching the metadata surface) or
    /// when its `dwFrameAspectRatio` is `0` (left unspecified by the
    /// writer — the metadata surface omits the key in that case too, so
    /// absence stays observable). The pair round-trips a muxer-emitted
    /// ratio set via [`crate::muxer::VprpConfig::with_aspect`] /
    /// [`crate::muxer::VprpConfig::with_frame_aspect_ratio`].
    pub fn vprp_frame_aspect_ratio(&self, stream_index: u32) -> Option<(u16, u16)> {
        let vp = self.vprps.get(stream_index as usize)?;
        if vp.nb_field_per_frame == 0 || vp.frame_aspect_ratio == 0 {
            return None;
        }
        let x = (vp.frame_aspect_ratio >> 16) as u16;
        let y = (vp.frame_aspect_ratio & 0xFFFF) as u16;
        Some((x, y))
    }

    /// Round-19 candidate 1: per-video-stream top-down DIB flag.
    ///
    /// Returns `Some(true)` when the stream's BMIH carried a negative
    /// `biHeight` (origin upper-left, top-down DIB per VfW
    /// `wingdi.h` §"biHeight sign rules"); `Some(false)` for
    /// positive `biHeight` (origin lower-left, bottom-up DIB);
    /// `None` for non-video streams or video streams whose `strf`
    /// payload was missing / too short to parse a BMIH.
    ///
    /// Only semantically meaningful for uncompressed RGB streams
    /// (`BI_RGB` and `BI_BITFIELDS`) — YUV bitmaps are always
    /// top-down regardless of sign per the same VfW section, and
    /// compressed FourCCs MUST use positive `biHeight`. Callers
    /// that re-mux a top-down RGB stream and want the orientation
    /// preserved can pair this with
    /// [`crate::muxer::AviMuxOptions::with_top_down_video`].
    pub fn stream_top_down(&self, stream_index: u32) -> Option<bool> {
        self.video_strf
            .get(stream_index as usize)
            .and_then(|v| v.as_ref())
            .map(|vs| vs.top_down)
    }

    /// Round-19 candidate 2: per-video-stream `BI_BITFIELDS` color
    /// masks.
    ///
    /// Returns `Some((red, green, blue))` when the stream's BMIH
    /// declared `biCompression == BI_BITFIELDS` (3) and the trailing
    /// 12 bytes parsed as three little-endian DWORDs per VfW
    /// `wingdi.h` §"Color tables (palettes)". `None` for any other
    /// compression (FourCC bitstreams, `BI_RGB`, etc.), for
    /// non-video streams, or when the extradata was shorter than
    /// 12 bytes.
    ///
    /// Common masks per VfW §"biCompression":
    /// - `(0xF800, 0x07E0, 0x001F)` ⇒ 16-bpp RGB565
    /// - `(0x7C00, 0x03E0, 0x001F)` ⇒ 16-bpp RGB555
    /// - `(0x00FF_0000, 0x0000_FF00, 0x0000_00FF)` ⇒ 32-bpp BGRA
    pub fn stream_bitfields_masks(&self, stream_index: u32) -> Option<(u32, u32, u32)> {
        self.video_strf
            .get(stream_index as usize)
            .and_then(|v| v.as_ref())
            .and_then(|vs| vs.bitfields_masks)
    }

    /// Round-75: per-audio-stream WAVEFORMATEX(TENSIBLE) typed side-info.
    ///
    /// Returns the captured [`AudioStrfInfo`] for an audio stream
    /// (`format_tag` always populated; `valid_bits_per_sample` /
    /// `channel_mask` / `subformat` populated only when the on-wire
    /// `wFormatTag` was [`crate::stream_format::WAVE_FORMAT_EXTENSIBLE`]
    /// — `0xFFFE` — and the strf payload carried the 22-byte
    /// extension). `None` for non-audio streams or out-of-range
    /// stream indexes.
    ///
    /// Pairs with the legacy [`oxideav_core::StreamInfo`] /
    /// [`oxideav_core::CodecParameters`] accessors; surfaces the
    /// per-channel-mask + actual-precision data the legacy
    /// `WAVEFORMATEX` shape couldn't express.
    pub fn stream_audio_strf(&self, stream_index: u32) -> Option<AudioStrfInfo> {
        self.audio_strf.get(stream_index as usize).and_then(|v| *v)
    }

    /// Round-75 convenience: `dwChannelMask` for an extensible audio
    /// stream. Returns `Some(mask)` only when the stream's
    /// `wFormatTag == WAVE_FORMAT_EXTENSIBLE (0xFFFE)` and the strf
    /// payload carried the 22-byte extension. `None` for legacy
    /// `WAVEFORMATEX` audio streams (the spec pre-dated explicit
    /// speaker assignment), non-audio streams, or out-of-range
    /// stream indexes.
    ///
    /// Per Microsoft Learn § "Channel-mask channel ordering", the
    /// channel-byte order in PCM frames follows the bit order of this
    /// mask (lowest set bit first). Use the constants in
    /// [`crate::stream_format`]'s `SPEAKER_*` namespace or the docs
    /// table for layout interpretation.
    pub fn stream_channel_mask(&self, stream_index: u32) -> Option<u32> {
        self.stream_audio_strf(stream_index)
            .and_then(|asi| asi.channel_mask)
    }

    /// Round-75 convenience: `wValidBitsPerSample` for an extensible
    /// audio stream — the actual sample precision, which may be
    /// smaller than `wfx.bits_per_sample` (container size, e.g. 24
    /// valid bits in a 32-bit container). `None` for legacy
    /// `WAVEFORMATEX` audio streams, non-audio streams, or
    /// out-of-range stream indexes.
    pub fn stream_valid_bits_per_sample(&self, stream_index: u32) -> Option<u16> {
        self.stream_audio_strf(stream_index)
            .and_then(|asi| asi.valid_bits_per_sample)
    }

    /// Round-75 convenience: `SubFormat` GUID for an extensible audio
    /// stream — the canonical codec identifier when `wFormatTag` is
    /// the `WAVE_FORMAT_EXTENSIBLE` (`0xFFFE`) escape hatch per
    /// Microsoft `KSMedia.h` `KSDATAFORMAT_SUBTYPE_*`. `None` for
    /// legacy `WAVEFORMATEX` audio streams, non-audio streams, or
    /// out-of-range stream indexes.
    ///
    /// Use [`crate::stream_format::Guid::ksdataformat_tag`] to
    /// recover the legacy `wFormatTag` when the GUID follows the
    /// canonical KSDATAFORMAT base pattern, or
    /// [`crate::stream_format::subformat_codec_hint`] to map the GUID
    /// to a codec-id string for the seven documented well-known
    /// SubFormats.
    pub fn stream_subformat(&self, stream_index: u32) -> Option<Guid> {
        self.stream_audio_strf(stream_index)
            .and_then(|asi| asi.subformat)
    }

    /// Round 163: typed [`ChannelMask`] view of the
    /// `WAVEFORMATEXTENSIBLE.dwChannelMask` for an extensible audio
    /// stream.
    ///
    /// Wraps the raw `u32` returned by [`Self::stream_channel_mask`]
    /// so callers can enumerate the `SPEAKER_*` positions in PCM
    /// byte-stream channel order without re-implementing the bit
    /// arithmetic. Same eligibility as `stream_channel_mask`: returns
    /// `Some` only when the stream's `wFormatTag ==
    /// WAVE_FORMAT_EXTENSIBLE (0xFFFE)` and the strf payload carried
    /// the 22-byte extension.
    ///
    /// The bit-order, speaker abbreviations, and named-layout
    /// recognition are all sourced from
    /// `docs/container/riff/waveformatextensible/README.md` (Microsoft
    /// Learn mirror, 2026-05-18) — see the "Channel-mask channel
    /// ordering" and "Standard layouts" tables.
    pub fn stream_channel_mask_typed(&self, stream_index: u32) -> Option<ChannelMask> {
        self.stream_channel_mask(stream_index)
            .map(ChannelMask::from_raw)
    }

    /// Round 163: named [`ChannelLayout`] recognition for an extensible
    /// audio stream's `dwChannelMask`.
    ///
    /// Returns `Some(layout)` only when the raw mask matches one of
    /// the seven entries in the docs README "Standard layouts" table:
    /// Mono / Stereo / 2.1 / Quad / 5.1 (Microsoft back) / 5.1
    /// (DVD-style side) / 7.1. Any other valid `SPEAKER_*` combination
    /// — and any stream where [`Self::stream_channel_mask`] returns
    /// `None` — yields `None`; the caller can fall back to
    /// [`Self::stream_channel_mask_typed`] for the raw decode.
    ///
    /// Reserved bits in the `SPEAKER_RESERVED` range (between
    /// `SPEAKER_TOP_BACK_RIGHT (0x20000)` and `SPEAKER_ALL
    /// (0x80000000)`) are ignored for matching purposes per
    /// [`ChannelLayout::from_mask`] — call
    /// [`ChannelMask::reserved_bits`] on the typed view if the caller
    /// wants to inspect them.
    pub fn stream_channel_layout(&self, stream_index: u32) -> Option<ChannelLayout> {
        self.stream_channel_mask_typed(stream_index)
            .and_then(|cm| cm.layout())
    }

    /// Round-80: optional per-stream name parsed from the `strn` chunk
    /// inside the stream's `strl` LIST per AVI 1.0 §"AVI Stream
    /// Headers".
    ///
    /// Returns `Some(name)` when the file declared a non-empty `strn`
    /// chunk for `stream_index`; `None` when the chunk was absent or
    /// carried an empty (zero-length / NUL-only) payload. Encoding is
    /// not normatively pinned by Microsoft's reference — the demuxer
    /// passes bytes through `String::from_utf8_lossy` so legacy
    /// Latin-1 / CP1252 names don't fail the parse.
    ///
    /// Pairs with [`crate::muxer::AviMuxOptions::with_stream_name`] on
    /// the mux side for a name → strn → name round-trip.
    pub fn stream_name(&self, stream_index: u32) -> Option<&str> {
        self.stream_names
            .get(stream_index as usize)
            .and_then(|n| n.as_deref())
    }

    /// Round-89: optional per-stream codec-driver configuration blob
    /// parsed from the `strd` chunk inside the stream's `strl` LIST
    /// per AVI 1.0 §"AVI Stream Headers" (docs/container/riff/
    /// avi-riff-file-reference.md §"AVI Stream Headers").
    ///
    /// Returns `Some(bytes)` when the file declared a `strd` chunk
    /// for `stream_index` (including a `cb=0` empty payload, which
    /// surfaces as `Some(&[])`); `None` when the chunk was absent.
    /// The byte slice is the raw chunk body — the spec defines its
    /// format as codec-driver-specific opaque data, so the demuxer
    /// performs no interpretation. Callers typically forward the
    /// bytes verbatim to the matching codec driver.
    ///
    /// Pairs with [`crate::muxer::AviMuxOptions::with_stream_header_data`]
    /// on the mux side for a `strd` round-trip.
    pub fn stream_header_data(&self, stream_index: u32) -> Option<&[u8]> {
        self.stream_header_data
            .get(stream_index as usize)
            .and_then(|h| h.as_deref())
    }

    /// `strh.rcFrame` destination rectangle for a stream, as the tuple
    /// `(left, top, right, bottom)` (round-115).
    ///
    /// Per AVI 1.0 §"AVISTREAMHEADER" (`rcFrame` field in
    /// `docs/container/riff/avi-riff-file-reference.md`): "Destination
    /// rectangle for a text or video stream within the movie rectangle
    /// specified by the dwWidth and dwHeight members of the AVI main
    /// header structure. The `rcFrame` member is typically used in support
    /// of multiple video streams. … Units for this member are pixels. The
    /// upper-left corner of the destination rectangle is relative to the
    /// upper-left corner of the movie rectangle." The four values are
    /// signed WORDs read little-endian in `[left, top, right, bottom]`
    /// order off byte offset 48 of the 56-byte AVISTREAMHEADER.
    ///
    /// Returns `None` when the stream's strh was the short 48-byte form
    /// (no `rcFrame`), when the rect was the all-zero "whole movie
    /// rectangle" writer default (so a default rect reads the same as an
    /// absent one — mirroring the `strn` / `IDIT` "empty == absent"
    /// convention), or for an out-of-range stream index. The same value
    /// surfaces as the `avi:strh.<index>.frame_rect` metadata key
    /// (`"left,top,right,bottom"`; also omitted when absent), and
    /// round-trips a muxer-emitted rect set via
    /// [`crate::muxer::AviMuxOptions::with_stream_frame_rect`].
    pub fn stream_frame_rect(&self, stream_index: u32) -> Option<(i16, i16, i16, i16)> {
        self.stream_frame_rects
            .get(stream_index as usize)
            .and_then(|r| r.as_ref())
            .map(|&[l, t, r, b]| (l, t, r, b))
    }

    /// `strh.wLanguage` LANGID for a stream, as the raw 16-bit value
    /// read little-endian off byte offset 14 of the AVISTREAMHEADER
    /// (round-119).
    ///
    /// Per AVI 1.0 §"AVISTREAMHEADER" (`wLanguage` field in
    /// `docs/container/riff/avi-riff-file-reference.md`): "Language
    /// tag (BCP 47 / RFC 1766 / similar; AVI does not normatively pin
    /// a registry)." Microsoft writers populate the field with a
    /// Win32 LANGID — the low 10 bits a `LANG_*` primary language id
    /// and the upper 6 bits a `SUBLANG_*` dialect id — while other
    /// writers may pack different values. The demuxer surfaces the
    /// raw 16-bit DWORD verbatim and leaves interpretation to the
    /// caller; no LANGID decoding or BCP-47 normalisation is
    /// performed.
    ///
    /// Returns `None` when the strh carried the `0`
    /// ("LANG_NEUTRAL / SUBLANG_NEUTRAL", the writer-skips-it
    /// default) so an unspecified language reads the same as an
    /// absent one (mirroring the `strn` / `IDIT` / `rcFrame`
    /// "default == absent" convention), or for an out-of-range stream
    /// index. The same value surfaces as the
    /// `avi:strh.<index>.language` metadata key (also omitted when
    /// absent), and round-trips a muxer-emitted LANGID set via
    /// [`crate::muxer::AviMuxOptions::with_stream_language`].
    pub fn stream_language(&self, stream_index: u32) -> Option<u16> {
        self.stream_languages
            .get(stream_index as usize)
            .and_then(|l| *l)
    }

    /// `strh.dwInitialFrames` skew for a stream, as the raw 32-bit
    /// value read little-endian off byte offset 16 of the
    /// AVISTREAMHEADER (round-153).
    ///
    /// Per AVI 1.0 §"AVISTREAMHEADER" (`dwInitialFrames` row in
    /// `docs/container/riff/avi-riff-file-reference.md`): *"How far
    /// audio data is skewed ahead of the video frames in interleaved
    /// files. Typically, this is about 0.75 seconds. If creating
    /// interleaved files, set the value of this member to the number
    /// of frames in the file prior to the initial frame of the AVI
    /// sequence in this member."* AVIMAINHEADER §`dwInitialFrames`
    /// adds: *"Initial frame for interleaved files. Noninterleaved
    /// files should specify zero."* The demuxer surfaces the raw
    /// 32-bit DWORD verbatim; the unit is the stream's own
    /// `dwRate` / `dwScale` tick (typically frames for video, blocks
    /// for audio) and the demuxer does not convert.
    ///
    /// Returns `None` when the strh carried the `0` ("noninterleaved
    /// file") writer default so an unspecified skew reads the same as
    /// an absent one (mirroring the `wLanguage` / `rcFrame` / `strn`
    /// / `IDIT` "default == absent" convention), or for an
    /// out-of-range stream index. The same value surfaces as the
    /// `avi:strh.<index>.initial_frames` metadata key (also omitted
    /// when absent), and round-trips a muxer-emitted skew set via
    /// [`crate::muxer::AviMuxOptions::with_stream_initial_frames`].
    pub fn stream_initial_frames(&self, stream_index: u32) -> Option<u32> {
        self.stream_initial_frames
            .get(stream_index as usize)
            .and_then(|f| *f)
    }

    /// `strh.dwQuality` quality indicator for a stream, as the raw
    /// 32-bit value read little-endian off byte offset 40 of the
    /// 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    /// (round-176). Per the `dwQuality` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 246):
    /// *"Indicator of the quality of the data in the stream. Quality
    /// is represented as a number between 0 and 10,000. For
    /// compressed data, this typically represents the value of the
    /// quality parameter passed to the compression software. If set
    /// to -1, drivers use the default quality value."*
    ///
    /// The demuxer surfaces the raw 32-bit DWORD verbatim. Per the
    /// spec the documented range is `[0, 10_000]`; legacy capture
    /// drivers occasionally stamp arbitrary values outside that range
    /// (full-precision quality scores, framework-internal markers,
    /// etc.) and the demuxer does not clamp or normalise — so
    /// out-of-spec writers round-trip exactly.
    ///
    /// Returns `None` when the strh carried the documented `-1`
    /// (`0xFFFF_FFFF` as u32) "use default driver quality" writer
    /// default — the legacy muxer's own default since round-3 — so an
    /// unspecified quality reads the same as an absent one (mirroring
    /// the `dwInitialFrames` / `wLanguage` / `rcFrame` / `strn` /
    /// `IDIT` "default == absent" convention), or for an out-of-range
    /// stream index. The same value surfaces as the
    /// `avi:strh.<index>.quality` metadata key (also omitted when
    /// absent), and round-trips a muxer-emitted quality set via
    /// [`crate::muxer::AviMuxOptions::with_stream_quality`].
    pub fn stream_quality(&self, stream_index: u32) -> Option<u32> {
        self.stream_qualities
            .get(stream_index as usize)
            .and_then(|q| *q)
    }

    /// `strh.wPriority` selection hint for a stream, as the raw 16-bit
    /// DWORD read little-endian off byte offset 12 of the 56-byte
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-182). Per
    /// the `wPriority` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (Appendix B
    /// line 238): *"Priority of a stream type. For example, in a file
    /// with multiple audio streams, the one with the highest priority
    /// might be the default stream."*
    ///
    /// The field is a selection hint among same-`fccType` streams
    /// (the spec illustration picks a default-playback audio stream
    /// among several `auds` streams); the spec does not normatively
    /// pin a value range or a tie-break rule, so the demuxer surfaces
    /// the raw 16-bit DWORD verbatim and leaves the "what counts as
    /// highest" decision to the caller. Out-of-spec writers — those
    /// stamping arbitrary u16 values for application-specific tagging
    /// — round-trip exactly: the demuxer does not clamp or normalise.
    ///
    /// Returns `None` when the strh carried the documented `0` legacy
    /// writer default — the muxer's own default since round-3 — so an
    /// unspecified priority reads the same as an absent one (mirroring
    /// the round-176 `dwQuality` / round-153 `dwInitialFrames` /
    /// round-119 `wLanguage` / round-115 `rcFrame` / round-80 `strn`
    /// / round-107 `IDIT` "default == absent" convention), or for an
    /// out-of-range stream index. The same value surfaces as the
    /// `avi:strh.<index>.priority` metadata key (also omitted when
    /// absent), and round-trips a muxer-emitted priority set via
    /// [`crate::muxer::AviMuxOptions::with_stream_priority`].
    pub fn stream_priority(&self, stream_index: u32) -> Option<u16> {
        self.stream_priorities
            .get(stream_index as usize)
            .and_then(|p| *p)
    }

    /// `strh.dwStart` starting time for a stream, as the raw 32-bit
    /// DWORD read little-endian off byte offset 28 of the 56-byte
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-203). Per
    /// the `dwStart` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 243):
    /// *"Starting time for this stream. The units are defined by the
    /// dwRate and dwScale members in the main file header. Usually,
    /// this is zero, but it can specify a delay time for a stream
    /// that does not start concurrently with the file."*
    ///
    /// The unit is the stream's own `(dwRate / dwScale)` tick — frames
    /// for video, samples-or-blocks for audio — and the demuxer surfaces
    /// the value verbatim. The spec phrases the field as a stream-local
    /// delay relative to the file's logical start (typical use is a
    /// non-zero `dwStart` on an audio stream whose first sample is
    /// supposed to play several frames after the video begins, or on
    /// a late-joining secondary video stream), so the caller decides
    /// how to combine it with the file's own [`Self::initial_frames`]
    /// skew.
    ///
    /// Returns `None` when the strh carried the documented `0` legacy
    /// writer default (the spec-documented "starts concurrently with
    /// the file" value, also the muxer's own default since round-3) so
    /// an unspecified start reads the same as an absent one (mirroring
    /// the round-182 `wPriority` / round-176 `dwQuality` / round-153
    /// `dwInitialFrames` / round-119 `wLanguage` / round-115 `rcFrame`
    /// / round-80 `strn` / round-107 `IDIT` "default == absent"
    /// convention), or for an out-of-range stream index. The same
    /// value surfaces as the `avi:strh.<index>.start` metadata key
    /// (also omitted when absent), and round-trips a muxer-emitted
    /// start set via
    /// [`crate::muxer::AviMuxOptions::with_stream_start`].
    pub fn stream_start(&self, stream_index: u32) -> Option<u32> {
        self.stream_starts
            .get(stream_index as usize)
            .and_then(|s| *s)
    }

    /// `strh.fccHandler` driver-handler FourCC for a stream, as the raw
    /// 4 bytes read off byte offset 4 of the AVISTREAMHEADER
    /// (round-210).
    ///
    /// Per AVI 1.0 §"AVISTREAMHEADER" (`fccHandler` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, Appendix B
    /// line 236): *"An optional FOURCC that identifies a specific data
    /// handler. The data handler is the preferred handler for the
    /// stream. For audio and video streams, this specifies the codec
    /// for decoding the stream."* This is the VfW preferred-driver
    /// hint; it sits beside (and is logically distinct from) the
    /// video stream's `BITMAPINFOHEADER.biCompression` FourCC and
    /// the audio stream's `WAVEFORMATEX.wFormatTag`. The two
    /// typically mirror each other for video (`MJPG` in both
    /// fccHandler and biCompression) but the spec does not require
    /// them to match, and legacy capture writers in the wild
    /// occasionally leave fccHandler zero on a video stream whose
    /// biCompression is set. For audio streams the field is almost
    /// always zero (the spec's *optional* qualifier — the
    /// `WAVEFORMATEX.wFormatTag` already routes to the codec).
    ///
    /// Returns `None` when the strh carried the all-zero
    /// `\0\0\0\0` "no preferred handler" default so an unspecified
    /// driver hint reads the same as an absent one (mirroring the
    /// round-203 `dwStart` / round-182 `wPriority` / round-176
    /// `dwQuality` / round-153 `dwInitialFrames` / round-119
    /// `wLanguage` / round-115 `rcFrame` / round-80 `strn` /
    /// round-107 `IDIT` "default == absent" convention), or for an
    /// out-of-range stream index. The same value surfaces as the
    /// `avi:strh.<index>.handler` metadata key (printable-ASCII form
    /// when every byte is in `0x20..=0x7e`, otherwise an
    /// `0xHHHHHHHH` lower-case hex form; also omitted when absent),
    /// and round-trips a muxer-emitted handler set via
    /// [`crate::muxer::AviMuxOptions::with_stream_handler`].
    pub fn stream_handler(&self, stream_index: u32) -> Option<[u8; 4]> {
        self.stream_handlers
            .get(stream_index as usize)
            .and_then(|h| *h)
    }

    /// `strh.dwSuggestedBufferSize` read-ahead hint for a stream, as the
    /// raw 32-bit DWORD read little-endian off byte offset 36 of the
    /// 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-217).
    /// Per the `dwSuggestedBufferSize` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 245): *"How
    /// large a buffer should be used to read this stream. Typically, this
    /// contains a value corresponding to the largest chunk present in the
    /// stream. Using the correct buffer size makes playback more
    /// efficient. Use zero if you do not know the correct buffer size."*
    ///
    /// The field is the per-stream counterpart of the file-global
    /// `avih.dwSuggestedBufferSize` already surfaced via
    /// [`Self::avih_suggested_buffer_size`]: the avih flavour is meant to
    /// cover the largest chunk across every stream, while this strh
    /// flavour is a per-stream upper bound (the spec recommends keeping
    /// it equal to the largest chunk in that one stream). The two are
    /// spec-independent — writers may stamp consistent values, or set
    /// only one, or leave both at the `0` "do not know" sentinel — and
    /// the demuxer surfaces each verbatim with no validation against
    /// the actual largest chunk seen in `movi`.
    ///
    /// Returns `None` when the strh carried the spec-documented `0` "do
    /// not know the correct buffer size" sentinel so an unspecified hint
    /// reads the same as an absent one (mirroring the round-210
    /// `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
    /// round-176 `dwQuality` / round-153 `dwInitialFrames` / round-119
    /// `wLanguage` / round-115 `rcFrame` / round-80 `strn` / round-107
    /// `IDIT` "default == absent" convention), or for an out-of-range
    /// stream index. The same value surfaces as the
    /// `avi:strh.<index>.suggested_buffer_size` metadata key (also
    /// omitted when absent), and round-trips a muxer-emitted hint set
    /// via
    /// [`crate::muxer::AviMuxOptions::with_stream_suggested_buffer_size`].
    pub fn stream_suggested_buffer_size(&self, stream_index: u32) -> Option<u32> {
        self.stream_suggested_buffer_sizes
            .get(stream_index as usize)
            .and_then(|s| *s)
    }

    /// `strh.dwSampleSize` indicator for a stream, as the raw 32-bit
    /// DWORD read little-endian off byte offset 44 of the 56-byte
    /// AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER" (round-222). Per
    /// the `dwSampleSize` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (line 247):
    /// *"The size of a single sample of data. This is set to zero if
    /// the samples can vary in size. If this number is nonzero, then
    /// multiple samples of data can be grouped into a single chunk
    /// within the file. If it is zero, each sample of data (such as a
    /// video frame) must be in a separate chunk. For video streams,
    /// this number is typically zero, although it can be nonzero if all
    /// video frames are the same size. For audio streams, this number
    /// should be the same as the nBlockAlign member of the WAVEFORMATEX
    /// structure describing the audio."*
    ///
    /// For audio streams the field doubles as the spec's VBR / CBR
    /// switch (a complementary view to the separate round-14 C2 audio
    /// sample-size invariant that this crate enforces at `open` time):
    /// CBR codecs (PCM / G.711 / IMA-ADPCM) must stamp the
    /// `nBlockAlign` byte size here (so `dwLength` ends up as the total
    /// number of audio frames), while VBR codecs (MP3 / AAC / MPEG)
    /// must stamp `0` (so each chunk is one frame and
    /// `Packet.duration` drives the count). For video streams the field
    /// is dominantly `0` (one frame per chunk) and only legacy
    /// fixed-frame-size capture writers (early DV-in-AVI tools, some
    /// raw-yuv recorders) stamp a non-zero value.
    ///
    /// Returns `None` when the strh carried the spec-documented `0`
    /// "samples can vary in size" sentinel so an unspecified hint reads
    /// the same as an absent one (mirroring the round-217
    /// `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    /// `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    /// round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
    /// `rcFrame` / round-80 `strn` / round-107 `IDIT` "default ==
    /// absent" convention), or for an out-of-range stream index. The
    /// same value surfaces as the `avi:strh.<index>.sample_size`
    /// metadata key (also omitted when absent), and round-trips a
    /// muxer-emitted hint set via
    /// [`crate::muxer::AviMuxOptions::with_stream_sample_size`].
    ///
    /// The demuxer surfaces the raw u32 verbatim and does not validate
    /// against `WAVEFORMATEX.nBlockAlign` for audio streams — the
    /// round-14 C2 audio sample-size invariant covers the VBR/CBR
    /// consistency check separately (and is bypassable via
    /// [`open_avi_lenient`] for forensic inspection of files whose
    /// `dwSampleSize` lies about their carriage).
    pub fn stream_sample_size(&self, stream_index: u32) -> Option<u32> {
        self.stream_sample_sizes
            .get(stream_index as usize)
            .and_then(|s| *s)
    }

    /// Round-229: per-stream `strh.dwLength` raw u32 from byte offset
    /// 32 of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    /// (`dwLength` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 244):
    /// *"Length of this stream. The units are defined by the dwRate
    /// and dwScale members of the stream's header."* The unit is the
    /// stream's own `(dwRate / dwScale)` tick — frames for video,
    /// samples-or-blocks for audio per the muxer's own derivation in
    /// `patch_post_counts`. For the audio side the value is
    /// `total_sample_count` for PCM / CBR carriage and `packet_count`
    /// (one DWORD per packet) for VBR carriage; video streams always
    /// carry `packet_count`. The demuxer surfaces the raw 32-bit value
    /// verbatim with no rate-conversion.
    ///
    /// Returns `None` when the strh carried the `0` "no length
    /// declared" value so an unspecified / empty-stream length reads
    /// the same as an absent one (mirroring the round-222
    /// `dwSampleSize` / round-217 `dwSuggestedBufferSize` / round-210
    /// `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
    /// round-176 `dwQuality` / round-153 `dwInitialFrames` /
    /// round-119 `wLanguage` / round-115 `rcFrame` / round-80 `strn`
    /// / round-107 `IDIT` "default == absent" convention), or for an
    /// out-of-range stream index. The same value surfaces as the
    /// `avi:strh.<index>.length` metadata key (also omitted when
    /// absent), and round-trips a muxer-emitted override set via
    /// [`crate::muxer::AviMuxOptions::with_stream_length`].
    ///
    /// Logically distinct from the `StreamInfo::duration` already
    /// exposed by [`oxideav_core::Demuxer::streams`] (also derived
    /// from this same DWORD but typed as `Option<i64>` for the
    /// framework-level duration model). The raw-u32 surface keeps the
    /// value observable verbatim for callers that need byte-exact
    /// round-trip semantics or comparison against a separately-emitted
    /// writer's stamp; the framework duration is the right shape for
    /// arithmetic in stream ticks. The two values agree whenever the
    /// strh stamp fits in `i64`.
    pub fn stream_length(&self, stream_index: u32) -> Option<u32> {
        self.stream_lengths
            .get(stream_index as usize)
            .and_then(|s| *s)
    }

    /// Round-247: per-stream `strh.dwFlags` raw u32 from byte offset 8
    /// of the AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
    /// (`dwFlags` row in
    /// `docs/container/riff/avi-riff-file-reference.md`, line 237).
    /// Two `AVISF_*` bits are spec-documented in the *dwFlags values*
    /// table at lines 252–255:
    ///
    /// - [`AVISF_DISABLED`] (`0x0000_0001`): *"Indicates this stream
    ///   should not be enabled by default."*
    /// - [`AVISF_VIDEO_PALCHANGES`] (`0x0001_0000`): *"Indicates this
    ///   video stream contains palette changes. This flag warns the
    ///   playback software that it will need to animate the palette."*
    ///
    /// Returns `None` when the strh carried the `0` "no flags set"
    /// legacy writer default — so an unspecified flag field reads the
    /// same as an absent one (mirroring the round-229 `dwLength` /
    /// round-222 `dwSampleSize` / round-217 `dwSuggestedBufferSize` /
    /// round-210 `fccHandler` / round-203 `dwStart` / round-182
    /// `wPriority` / round-176 `dwQuality` / round-153
    /// `dwInitialFrames` / round-119 `wLanguage` / round-115
    /// `rcFrame` / round-80 `strn` / round-107 `IDIT` "default ==
    /// absent" convention), or for an out-of-range stream index. The
    /// same value surfaces as the `avi:strh.<index>.flags` metadata
    /// key (`0xXXXXXXXX` upper-case hex, also omitted when absent),
    /// and round-trips a muxer-emitted override set via
    /// [`crate::muxer::AviMuxOptions::with_stream_flags`].
    ///
    /// The raw u32 is preserved verbatim — the demuxer does NOT mask
    /// bits outside the documented set (some legacy capture filters
    /// pack driver-private bits in the upper half-DWORD). For a
    /// structured decode of the documented `AVISF_*` bits, use
    /// [`Self::stream_flags_typed`] instead.
    pub fn stream_flags(&self, stream_index: u32) -> Option<u32> {
        self.stream_flags
            .get(stream_index as usize)
            .and_then(|s| *s)
    }

    /// Round-247: typed [`StrhFlags`] decode of `strh.dwFlags` (the
    /// same DWORD surfaced raw via [`Self::stream_flags`]).
    ///
    /// Returns `None` for streams whose strh carried the `0` "no
    /// flags set" default — so the absence stays observable via
    /// `Option::is_none()` — and for out-of-range stream indices.
    /// Streams with any non-zero bit return `Some(StrhFlags { ... })`
    /// with the two documented `AVISF_*` bits decoded into named
    /// fields and the raw u32 preserved in `StrhFlags::bits` so
    /// undocumented vendor / driver bits stay observable.
    pub fn stream_flags_typed(&self, stream_index: u32) -> Option<StrhFlags> {
        self.stream_flags(stream_index).map(StrhFlags::from_bits)
    }

    /// `(strh.dwScale, strh.dwRate)` raw timebase pair for a stream,
    /// as the two 32-bit DWORDs read little-endian off byte offsets 20
    /// + 24 of the 56-byte AVISTREAMHEADER (round-249).
    ///
    /// Per AVI 1.0 §"AVISTREAMHEADER" (`dwScale` row in
    /// `docs/container/riff/avi-riff-file-reference.md` line 241): *"Used
    /// with dwRate to specify the time scale that this stream will use.
    /// Dividing dwRate by dwScale gives the number of samples per
    /// second. For video streams, this is the frame rate. For audio
    /// streams, this rate corresponds to the time needed to play
    /// nBlockAlign bytes of audio, which for PCM audio is the just the
    /// sample rate."* The `dwRate` row (line 242) cross-references
    /// `dwScale` for the paired interpretation.
    ///
    /// The two DWORDs together define the stream's tick — a video
    /// stream with `dwScale = 1001, dwRate = 30000` ticks at the NTSC
    /// 29.97 fps, an audio stream with `dwScale = 1, dwRate = 48000`
    /// ticks at one sample per audio frame, etc. The accessor surfaces
    /// both raw values verbatim so callers that need byte-exact
    /// round-trip parity (preserved-on-disk rate / scale pair, not the
    /// `.max(1)`-clamped form used internally to keep
    /// `StreamInfo::time_base` decodable on degenerate files) can read
    /// the on-disk bytes back as written.
    ///
    /// Returns `None` when the strh carried a `0` in either DWORD —
    /// the writer-skips-it / mathematically-undefined `rate/scale`
    /// ratio (legitimate AVIs always populate both; a zero in either
    /// DWORD indicates a truncated or zero-padded header that the
    /// framework's `StreamInfo::time_base` derivation handles via
    /// `.max(1)` for decode purposes). The convention mirrors the
    /// round-247 `dwFlags` / round-229 `dwLength` / round-222
    /// `dwSampleSize` / round-217 `dwSuggestedBufferSize` / round-210
    /// `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
    /// round-176 `dwQuality` / round-153 `dwInitialFrames` / round-119
    /// `wLanguage` / round-115 `rcFrame` "default == absent" pattern,
    /// and an out-of-range stream index also returns `None`.
    ///
    /// The same values surface as the `avi:strh.<index>.scale` and
    /// `avi:strh.<index>.rate` decimal metadata keys (both omitted when
    /// absent), and round-trip a muxer-emitted timebase set via
    /// [`crate::muxer::AviMuxOptions::with_stream_timebase`].
    pub fn stream_timebase(&self, stream_index: u32) -> Option<(u32, u32)> {
        self.stream_rates
            .get(stream_index as usize)
            .and_then(|s| *s)
    }

    /// `strh.fccType` raw FOURCC for a stream, as the verbatim 4 bytes
    /// read off byte offset 0 of the 56-byte AVISTREAMHEADER per AVI
    /// 1.0 §"AVISTREAMHEADER" (round-253). Per the `fccType` row in
    /// `docs/container/riff/avi-riff-file-reference.md` (Appendix B
    /// line 235 + the `fcc` row at line 234 documenting the standard
    /// values): *"A FOURCC code that specifies the type of data
    /// contained in the stream. The following standard AVI values are
    /// defined: `auds` (audio stream), `mids` (MIDI stream), `txts`
    /// (text stream), `vids` (video stream)."*
    ///
    /// The FOURCC determines the stream's high-level media-kind
    /// routing inside the demuxer, but the raw on-disk byte pattern
    /// surfaces here verbatim so callers comparing against a
    /// separately-emitted writer's stamp, or stamping an identical
    /// FOURCC on re-mux, can do so byte-exactly. Non-standard
    /// FOURCCs outside the spec's `{auds, mids, txts, vids}` set
    /// (legacy capture filters occasionally invented vendor types
    /// such as `iavs` for interleaved DV streams) surface verbatim
    /// for the caller to interpret — the spec phrases the standard
    /// values as illustrative rather than exhaustive, and the
    /// demuxer does NOT validate membership in the standard set.
    ///
    /// Returns `None` when the strh carried the all-zero
    /// `[0, 0, 0, 0]` sentinel (a writer-skips-it / "no declared
    /// type" default) so an unspecified type reads the same as an
    /// absent one — mirroring the round-249 `(dwScale, dwRate)` /
    /// round-247 `dwFlags` / round-229 `dwLength` / round-222
    /// `dwSampleSize` / round-217 `dwSuggestedBufferSize` / round-210
    /// `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
    /// round-176 `dwQuality` / round-153 `dwInitialFrames` / round-119
    /// `wLanguage` / round-115 `rcFrame` "default == absent"
    /// convention this crate has carried since round-115, or for an
    /// out-of-range stream index. The same value surfaces as the
    /// `avi:strh.<index>.fcc_type` metadata key (also omitted when
    /// absent), and round-trips a muxer-emitted type set via
    /// [`crate::muxer::AviMuxOptions::with_stream_fcc_type`].
    pub fn stream_fcc_type(&self, stream_index: u32) -> Option<[u8; 4]> {
        self.stream_fcc_types
            .get(stream_index as usize)
            .and_then(|f| *f)
    }

    /// Digitization-date string from the optional `IDIT` chunk inside
    /// `LIST hdrl` (round-107).
    ///
    /// `IDIT` is a member of the RIFF *Hdrl Tags* namespace
    /// (`DateTimeOriginal`) per
    /// `docs/container/riff/metadata/exiftool-riff-tags.html` §"RIFF
    /// Hdrl Tags". Capture hardware writes the capture / digitization
    /// timestamp here; the on-disk text format is writer-defined and
    /// **not** pinned by the staged docs (an `asctime`-style "Wed Jan 02
    /// 02:03:55 2002" is common from VfW capture filters, while other
    /// tools emit ISO-8601), so the returned string is the chunk body
    /// verbatim with only trailing NUL / ASCII-whitespace bytes
    /// stripped and decoded UTF-8-lossy. No date parsing or
    /// normalisation is performed — the caller decides how to interpret
    /// the timestamp.
    ///
    /// Returns `None` when the file carried no `IDIT` chunk, or when the
    /// chunk body was empty / all-NUL / all-whitespace (so a present-
    /// but-empty chunk reads the same as an absent one). The same value
    /// surfaces under the `avi:idit` metadata key (also omitted when
    /// absent), and round-trips a muxer-emitted date set via
    /// [`crate::muxer::AviMuxOptions::with_digitization_date`].
    pub fn digitization_date(&self) -> Option<&str> {
        self.digitization_date.as_deref()
    }

    /// SMPTE-timecode string from the optional `ISMP` chunk inside
    /// `LIST hdrl` (round-112).
    ///
    /// `ISMP` is a member of the RIFF *Hdrl Tags* namespace (`TimeCode`)
    /// per `docs/container/riff/metadata/exiftool-riff-tags.html` §"RIFF
    /// Hdrl Tags", sitting directly alongside the `IDIT`
    /// digitization-date chunk. Capture hardware writes the SMPTE
    /// timecode of the first frame here; the on-disk text format is
    /// writer-defined and **not** pinned by the staged docs (the SMPTE
    /// non-drop-frame `"HH:MM:SS:FF"` colon form is common, while
    /// drop-frame writers use a `';'` before the frame field and some
    /// tools emit a fractional `"HH:MM:SS.ss"`), so the returned string
    /// is the chunk body verbatim with only trailing NUL /
    /// ASCII-whitespace bytes stripped and decoded UTF-8-lossy. No
    /// timecode parsing or normalisation is performed — the caller
    /// decides how to interpret the value.
    ///
    /// Returns `None` when the file carried no `ISMP` chunk, or when the
    /// chunk body was empty / all-NUL / all-whitespace (so a present-
    /// but-empty chunk reads the same as an absent one). The same value
    /// surfaces under the `avi:ismp` metadata key (also omitted when
    /// absent), and round-trips a muxer-emitted timecode set via
    /// [`crate::muxer::AviMuxOptions::with_smpte_timecode`].
    pub fn smpte_timecode(&self) -> Option<&str> {
        self.smpte_timecode.as_deref()
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

    /// `Idx1Flags`-aware seek to the first non-`AVIIF_NO_TIME`
    /// keyframe at-or-after `target_pts` (round-18 candidate 4).
    ///
    /// [`Self::seek_to_keyframe_strict`] (and the underlying
    /// [`Demuxer::seek_to`]) walk every idx1 entry whose
    /// `AVIIF_KEYFRAME` bit is set and pick the LAST one with
    /// `pts <= target` — which is the correct behaviour for
    /// presentation-clock-aligned playback: land on a real frame the
    /// decoder can decode standalone.
    ///
    /// But `idx1` also indexes side-band chunks (`xxpc`
    /// palette-change, `xxtx` text/subtitle, custom data) that the
    /// muxer flags with `AVIIF_NO_TIME` per Microsoft `vfw.h`: those
    /// entries do NOT increment the per-stream presentation clock.
    /// The legacy keyframe-only seek doesn't distinguish them from
    /// real video keyframes — a file whose primary video stream
    /// interleaves a palette-change chunk in front of every keyframe
    /// can land the cursor on the palette chunk instead of the
    /// frame, and a player has to walk forward to find a frame the
    /// decoder can actually decode.
    ///
    /// This helper closes that gap: it walks idx1 entries belonging
    /// to `stream_index` where BOTH `is_keyframe` is set AND
    /// `is_no_time` is NOT set, picks the first one with
    /// `pts >= target_pts`, and seeks the input there. The returned
    /// [`KeyframeSeekResult`] holds the originally-requested target,
    /// the landed pts (always at-or-after target — the first
    /// non-NO_TIME keyframe AT or AFTER the request), and the gap
    /// `landed_pts - target_pts` clamped to `>= 0` as
    /// `gop_distance`.
    ///
    /// Falls back to the LAST non-NO_TIME keyframe in the stream when
    /// no entry at-or-after target qualifies (e.g. caller asked for a
    /// pts past the last keyframe). Returns
    /// [`Error::Unsupported`] when the file has no `idx1` (use
    /// [`Self::seek_to_keyframe_strict_via_std_index`] for OpenDML-
    /// only files) or when the stream has no non-NO_TIME keyframe at
    /// all (e.g. an audio-only or palette-only stream).
    pub fn seek_to_first_video_keyframe_after(
        &mut self,
        stream_index: u32,
        target_pts: i64,
    ) -> Result<KeyframeSeekResult> {
        if (stream_index as usize) >= self.streams.len() {
            return Err(Error::invalid(format!(
                "AVI: stream index {stream_index} out of range"
            )));
        }
        if self.idx_table.is_empty() {
            return Err(Error::unsupported(
                "AVI: seek_to_first_video_keyframe_after requires idx1 \
                 (OpenDML ix## not yet supported in this helper)",
            ));
        }
        // First non-NO_TIME keyframe with pts >= target.
        let mut best: Option<&IdxEntry> = None;
        for e in &self.idx_table {
            if e.stream != stream_index {
                continue;
            }
            if (e.flags & AVIIF_KEYFRAME) == 0 {
                continue;
            }
            if (e.flags & AVIIF_NO_TIME) != 0 {
                // Side-band keyframe (palette/text/data marked
                // NO_TIME) — skip.
                continue;
            }
            if e.pts >= target_pts {
                best = match best {
                    Some(b) if b.pts <= e.pts => Some(b),
                    _ => Some(e),
                };
            }
        }
        // Fall back to the LAST non-NO_TIME keyframe in the stream
        // when nothing at-or-after target qualifies (caller asked
        // past EOF / past the last keyframe).
        if best.is_none() {
            for e in &self.idx_table {
                if e.stream != stream_index
                    || (e.flags & AVIIF_KEYFRAME) == 0
                    || (e.flags & AVIIF_NO_TIME) != 0
                {
                    continue;
                }
                best = match best {
                    Some(b) if b.pts >= e.pts => Some(b),
                    _ => Some(e),
                };
            }
        }
        let landed = best.ok_or_else(|| {
            Error::unsupported(format!(
                "AVI: no non-NO_TIME keyframes in idx1 for stream {stream_index}"
            ))
        })?;

        // Mirror seek_to's input-positioning + per-stream pts reset.
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
                self.per_stream_counter[s] = e.pts.max(0) as u64;
            }
        }

        let landed_pts = landed.pts;
        // gop_distance is the gap from target to the landed frame —
        // always >= 0 because we picked the first frame at-or-after
        // target. When the fallback path fired (no frame at-or-after
        // target), landed_pts < target and we clamp to 0 so callers
        // don't get a negative distance.
        let gop_distance = landed_pts.saturating_sub(target_pts).max(0);
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

        let parsed = parse_ix_chunk(*b"ix00", &body).unwrap();
        assert_eq!(&parsed.own_fourcc, b"ix00");
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

        let parsed = parse_ix_chunk(*b"ix00", &body).expect("2-field index must parse");
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

    /// Round-304: `parse_indx` must surface the super-index's own
    /// `wLongsPerEntry` WORD verbatim. Build a well-formed AVI 2.0
    /// super-index (Appendix F: `wLongsPerEntry = 4`, each entry 16 B)
    /// and a malformed one declaring a non-spec stride; the parser
    /// stores both verbatim into `SuperIndex::w_longs_per_entry`.
    fn build_indx_body(w_longs_per_entry: u16, entries: &[(u64, u32, u32)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&w_longs_per_entry.to_le_bytes()); // wLongsPerEntry
        body.push(0); // bIndexSubType
        body.push(AVI_INDEX_OF_INDEXES); // bIndexType (super)
        body.extend_from_slice(&(entries.len() as u32).to_le_bytes()); // nEntriesInUse
        body.extend_from_slice(b"00dc"); // dwChunkId
        body.extend_from_slice(&[0u8; 12]); // dwReserved[3]
        for (qw_offset, dw_size, dw_duration) in entries {
            body.extend_from_slice(&qw_offset.to_le_bytes());
            body.extend_from_slice(&dw_size.to_le_bytes());
            body.extend_from_slice(&dw_duration.to_le_bytes());
        }
        body
    }

    #[test]
    fn parse_indx_surfaces_default_longs_per_entry() {
        let body = build_indx_body(4, &[(0x1000, 0x200, 30), (0x4000, 0x200, 30)]);
        let sx = parse_indx(&body).unwrap();
        assert_eq!(
            sx.w_longs_per_entry, 4,
            "spec-default AVISUPERINDEX stride must round-trip verbatim"
        );
        assert_eq!(sx.entries.len(), 2);
        assert_eq!(sx.entries[0].qw_offset, 0x1000);
        assert_eq!(sx.entries[0].dw_duration, 30);
    }

    #[test]
    fn parse_indx_surfaces_nondefault_longs_per_entry() {
        // A super-index whose entry-stride WORD is *not* the spec's
        // `4`. The 16-byte entry walk still reads each
        // `(qwOffset, dwSize, dwDuration)` triple; the parser preserves
        // the declared stride so a reader can detect the malformed /
        // future-extended table.
        let body = build_indx_body(8, &[(0x2000, 0x100, 15)]);
        let sx = parse_indx(&body).unwrap();
        assert_eq!(
            sx.w_longs_per_entry, 8,
            "non-default AVISUPERINDEX stride must be surfaced verbatim, not normalised to 4"
        );
        assert_eq!(sx.entries.len(), 1);
        assert_eq!(sx.entries[0].qw_offset, 0x2000);
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
