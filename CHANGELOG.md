# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **OpenDML 2.0 `ix##` standard-index emit + parse + seek.** Muxer
  flushes one `AVISTDINDEX` (`ix##`) chunk per stream at the tail
  of every `RIFF AVIX` segment's `movi` LIST (spec/06 §"Index
  Locations"). Demuxer scans every `movi` segment for `ix##`
  chunks and uses them as a fallback for `seek_to` when the AVI
  1.0 `idx1` table is absent — the canonical case for files
  written by recent ffmpeg / VirtualDub2 with `--max_riff_size`
  set. The fallback walks every keyframe entry across all
  segments and lands on the latest one whose synthesised pts is
  ≤ the requested target. Per-stream PTS counters are reset to
  match the landed entry so `next_packet` resumes with correct
  timestamps.
- **`indx` super-index full parse.** Demuxer now decodes the
  per-stream `AVISUPERINDEX` (24-byte preamble + 16-byte entries
  with `qwOffset`/`dwSize`/`dwDuration`) into a structured
  `SuperIndex` per stream. Round-trip-paired with the existing
  emit path. Used to gate the `ix##` scan when the super-index
  declares one (some encoders emit `ix##` without an `indx`,
  which the scan still picks up).
- **`avih` AVIMAINHEADER metadata.** Demuxer surfaces the
  AVIMAINHEADER fields beyond duration (`width`, `height`,
  `streams`, `flags`, `suggested_buffer_size`,
  `max_bytes_per_sec`) under namespaced metadata keys
  (`avi:width`, `avi:height`, …) so a media-info dumper can
  inspect the global header without re-parsing.
- **`avi:truncated` metadata flag.** Demuxer detects when the
  declared top-level RIFF length exceeds the physical file
  length (capture-card crash dumps, copy-aborted recordings)
  and surfaces `avi:truncated=true` so a downstream player UI
  can warn the user. Distinct from the existing best-effort
  packet-walk tolerance — this is the "did clamping take
  effect" signal.

### Fixed

- **`avih.dwTotalFrames` patch offset (off-by-4).** The muxer's
  post-mux back-patch wrote `dwTotalFrames` at file offset 44
  (which lands inside `dwFlags`); the correct offset for our
  layout (RIFF preamble 12 + LIST hdrl preamble 12 + avih
  chunk-header 8 + body offset 16) is 48. Fixes a silent
  AVIMAINHEADER corruption visible only when consumers read
  `dwFlags` or `dwTotalFrames` back; existing tests passed
  because none asserted on these.

- **Truncated-head AVI tolerance.** Demuxer now best-effort parses
  AVI 1.0 files whose top-level `RIFF` / `LIST hdrl` / `LIST movi`
  size fields over-declare the bytes physically present (capture-card
  crash dumps, copy-aborted recordings). Frames wholly inside the
  truncated body are surfaced; the partial frame at the truncation
  boundary is dropped silently and `next_packet` returns
  `Error::Eof`. Genuinely-malformed inputs (wrong RIFF FourCC,
  empty file, non-AVI form-type, missing `movi`) still error
  cleanly. Implementation: probe file length at `open()` and clamp
  declared chunk-end offsets against it; lenient chunk-header read
  (returns `Ok(None)` on partial-tail) inside `walk_riff_body` and
  `next_packet`; translate `read_exact` UnexpectedEof on a packet
  body to `Error::Eof`. New integration tests in
  `tests/truncated_head.rs` (9 cases: 6 truncation fixtures + 3
  negative). Origin: `oxideav-vfw` round-15 (commit `1214299c`)
  hit this against `crashtest.avi` and added a parallel relaxation
  in its codec-test helper; per `docs/IMPLEMENTOR_ROUND.md`
  §"Crate-purpose discipline" the fix lives here so vfw can drop
  the duplicate once this crate publishes.

### Changed

- **Drop the `codec_map.rs` parallel codec table**; demuxer + muxer
  now resolve codec identity via `oxideav_core::CodecResolver`.
  Each codec crate is the source of truth for its own AVI FourCCs /
  WAVE format tags via `CodecInfo::tag(s)`. Replaces a 300-LOC
  hand-maintained table with the shared registry surface and
  removes a class of "container forgot to update its codec_map"
  bugs entirely.
- **Switch wire-tag resolution to `CodecParameters::tag`**.
  - **Demuxer**: stamps `params.tag = Some(CodecTag::fourcc(...))`
    (video, from `bmih.bi_compression`) /
    `Some(CodecTag::wave_format(...))` (audio, from `wfx.format_tag`)
    so a muxer re-emitting the stream round-trips the demuxed
    FourCC / wFormatTag byte-for-byte. Forward
    `CodecResolver::resolve_tag` direction is unchanged.
  - **Muxer**: new resolution priority — (1) `params.tag` if set,
    (2) printable `extradata[0..4]` as a legacy fallback,
    (3) `[0,0,0,0]` BI_RGB sentinel for `rgb24` (video) / PCM-family
    synthesis from codec_id (audio). The previous
    `CodecResolver::tag_for_codec` path is gone (removed in
    `oxideav-core` 0.1.26 — registering a codec_id's "first
    declared FourCC" was arbitrary on multi-tag codecs and broke
    round-trip). Multi-FourCC codecs (`mpeg4video` /
    `magicyuv`'s 17 native v7 variants) get the right FourCC by
    setting `params.tag` on the encoder side or letting the demuxer
    propagate it from the source file.
- **API surface**: dropped `muxer::open_with_codecs` and
  `muxer::open_with_codecs_and_kind` — the muxer no longer needs an
  `&dyn CodecResolver`. Use `muxer::open` / `muxer::open_with_kind`
  with `params.tag` set on each stream.

### Added

- OpenDML 2.0 super-index encode in the muxer. New `AviKind` enum
  (`Avi10` / `OpenDml(RiffSegmentLimit)`) and `RiffSegmentLimit` enum
  (`OneGiB` / `Bytes(u64)`) opt the muxer into multi-`RIFF AVIX`
  emission with an `indx` super-index in the first stream's `strl`.
  Per-stream `ix##` chunks are intentionally omitted (spec/06 §6.1
  carve-out: the codec consumes the sequence of packets one at a
  time; ix## is informational). Use `muxer::open_with_kind` to opt
  in; `muxer::open` continues to emit AVI 1.0 single-RIFF.
- Demuxer now parses `indx` super-index chunks under `strl` for
  validation (24-byte preamble + nEntriesInUse × 16 B). The existing
  `RIFF AVIX` continuation walker (which handles multi-segment
  decoding) was already in place; this round just adds the
  super-index awareness inside `strl`.
- MagicYUV native FourCC family (17 entries, spec/01 §4.1):
  `M8RG`/`M8RA`/`M8Y4`/`M8Y2`/`M8Y0`/`M8YA`/`M8G0` (8-bit),
  `M0RG`/`M0RA`/`M0Y4`/`M0Y2`/`M0Y0`/`M0G0` (10-bit),
  `M2RG`/`M2RA` (12-bit), `M4RG`/`M4RA` (14-bit) all map to codec_id
  `"magicyuv"`. Muxer side: codec_id `"magicyuv"` + an optional
  4-byte FourCC hint at the start of `extradata` picks the wire
  FourCC (default `M8RG`).

### Notes

- OpenDML-driven seeking from the `indx` super-index is a follow-up:
  the demuxer parses `indx` for visibility but `seek_to` still
  consults only the AVI 1.0 `idx1` table. AVI files without `idx1`
  (streamed / OpenDML-only) return `Error::Unsupported` for seek;
  linear decode still walks every `RIFF AVIX` continuation.


## [0.0.5](https://github.com/OxideAV/oxideav-avi/compare/v0.0.4...v0.0.5) - 2026-05-03

### Other

- require oxideav-mjpeg >= 0.1.2 for new VideoFrame API
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- adopt slim VideoFrame/AudioFrame shape
- pin release-plz to patch-only bumps

## [0.0.4](https://github.com/OxideAV/oxideav-avi/compare/v0.0.3...v0.0.4) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- support OpenDML AVIX extension segments
- bump oxideav-mjpeg dep to "0.1"
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- delegate codec-id lookup to CodecResolver (registry-backed)
