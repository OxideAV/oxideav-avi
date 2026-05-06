# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Drop the `codec_map.rs` parallel codec table**; demuxer + muxer
  now resolve codec identity via `oxideav_core::CodecResolver`.
  Each codec crate is the source of truth for its own AVI FourCCs /
  WAVE format tags via `CodecInfo::tag(s)`. Replaces a 300-LOC
  hand-maintained table with the shared registry surface and
  removes a class of "container forgot to update its codec_map"
  bugs entirely.
  - **Demuxer**: `biCompression` (video) and `wFormatTag` (audio)
    flow through `CodecResolver::resolve_tag` first; on a miss the
    demuxer surfaces synthetic placeholder ids
    (`avi:<fourcc>` for video, `avi:tag_<hex>` for audio, plus the
    depth-aware `pcm_*` fallback for `WAVE_FORMAT_PCM` /
    `WAVE_FORMAT_IEEE_FLOAT`) so downstream `make_decoder` lookup
    fails with a clean "codec not registered" error.
  - **Muxer**: `CodecParameters::codec_id` flows through
    `CodecResolver::tag_for_codec(codec_id, CodecTagKind::Fourcc)`
    (video) / `CodecTagKind::WaveFormat` (audio) for the inverse
    direction. The first 4 bytes of `extradata` are still honoured
    as a wire-FourCC hint (multi-FourCC codecs like `mpeg4video` or
    `magicyuv`'s 17 native v7 variants — the caller picks).
- New `muxer::open_with_codecs` and `muxer::open_with_codecs_and_kind`
  ctors accept an `&dyn CodecResolver`. The legacy
  `muxer::open` / `muxer::open_with_kind` overloads default to
  `&NullCodecResolver` and only succeed for codecs the muxer can
  derive without registry data (PCM families + extradata-hint
  codecs + uncompressed `rgb24`); compressed codecs require the
  registry-aware constructor.

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
