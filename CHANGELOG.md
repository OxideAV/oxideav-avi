# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **File-global `avih.dwTotalFrames` typed demux accessor +
  `avi:total_frames` metadata key (round 268).** Adds the typed
  `AviDemuxer::avih_total_frames() -> Option<u32>` raw accessor
  returning the verbatim 32-bit value at byte offset 16 of the
  56-byte AVIMAINHEADER body, plus the `avi:total_frames = "<N>"`
  decimal metadata key (omitted when the field carried the all-zero
  writer-skips-it / empty-file sentinel so absence stays observable).
  The `0` sentinel maps to `None` on the accessor, mirroring the
  round-260 / round-256 / round-249 etc. "default == absent" idiom.
  Clean-room source: `docs/container/riff/avi-riff-file-reference.md`
  §"AVIMAINHEADER" Appendix A `dwTotalFrames` row (line 199): *"Total
  number of frames of data in the file."*

  Pre-round-268 the demuxer already parsed this DWORD and consumed it
  internally to derive `duration_micros = total_frames *
  micro_sec_per_frame` (the source of `Demuxer::duration`), but never
  surfaced the raw value — neither a typed accessor nor a metadata
  key existed (a code comment even referenced the `avi:total_frames`
  key without it ever being emitted; that comment is now accurate).
  Round-268 closes both gaps so the on-disk byte pattern stays
  observable independent of the derived duration, completing the
  AVIMAINHEADER typed-accessor series alongside
  `micro_sec_per_frame` (offset 0, round-256), `max_bytes_per_sec`
  (offset 4, round-260), `padding_granularity` (offset 8, round-92),
  `avih_flags` (offset 12), `initial_frames` (offset 20, round-157)
  and `avih_suggested_buffer_size` (offset 28).

  The accessor is named `avih_total_frames` (not bare `total_frames`)
  to keep it unambiguous next to the OpenDML `dmlh_total_frames()`
  accessor: per OpenDML 2.0 §5.0 the avih DWORD only carries the
  primary `RIFF AVI ` segment's frame count while `dmlh` carries the
  cross-segment truth — the two are spec-independent and both
  round-trip verbatim. Muxer-side no change was needed: the
  long-standing `write_trailer` patch already stamps the first video
  stream's emitted packet count at body offset 16 (file offset 48),
  and that auto-derived stamp round-trips verbatim through the new
  surface.

  Tests in `tests/round268_avih_total_frames.rs` (6 cases) cover:
  mux→demux round-trip of the muxer's auto-derived stamp via accessor
  + metadata key, accessor / metadata agreement, hand-rolled fixtures
  stamping an explicit non-zero (`0xDEAD_BEEF`) / zero
  `dwTotalFrames` at body offset 16 of a 56-byte AVIMAINHEADER,
  independence from `dmlh.dwTotalFrames` (a fixture stamping avih=5 /
  dmlh=99 surfaces both verbatim), and independence from the
  neighbouring AVIMAINHEADER DWORDs (offsets 0 / 4 / 20).

- **File-global `avih.dwMaxBytesPerSec` typed demux accessor
  (round 260).** Adds the typed
  `AviDemuxer::max_bytes_per_sec() -> Option<u32>` raw accessor
  returning the verbatim 32-bit value at byte offset 4 of the 56-byte
  AVIMAINHEADER body (the file-global "approximate maximum data rate"
  hint). The all-zero writer-skips-it sentinel maps to `None`,
  mirroring the round-256 / round-249 / round-247 / round-229 etc.
  "default == absent" idiom. Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md` §"AVIMAINHEADER"
  Appendix A `dwMaxBytesPerSec` row (line 196): *"Approximate maximum
  data rate of the file. Number of bytes per second the system must
  handle to present an AVI sequence as specified by the other
  parameters in the main header and stream header chunks."*

  Pre-round-260 the demuxer already parsed this DWORD and surfaced
  it as the `avi:max_bytes_per_sec` decimal metadata key (round-14),
  and the muxer's `AviMuxOptions::with_max_bytes_per_sec` builder
  was already wired (round-14). Round-260 closes the typed-accessor
  gap so a downstream remuxer or capture-info dumper can reach
  `Option<u32>` without scanning the metadata Vec — matching the
  shape of `micro_sec_per_frame` (round-256) / `padding_granularity`
  (round-92) / `initial_frames` (round-157). The accessor and the
  metadata key agree on the on-disk byte pattern; the `0` sentinel
  is observable both as the accessor's `None` and as the absence of
  the metadata key. Round-trips byte-equal with
  `AviMuxOptions::with_max_bytes_per_sec(n)`.

  Tests in `tests/round260_avih_max_bytes_per_sec.rs` (8 cases)
  cover: mux→demux override round-trip via accessor + metadata,
  no-override baseline (accessor agrees with the metadata key on the
  muxer-computed value), builder idempotency (last call wins), the
  `0` zero override (accessor reads `None`; metadata key absent),
  `0xFFFF_FFFF` all-bits round-trip, independence from neighbouring
  AVIMAINHEADER fields (round-256 `dwMicroSecPerFrame`, round-92
  `dwPaddingGranularity`) and per-stream `(dwScale, dwRate)`
  (round-249), plus two hand-rolled fixtures stamping an explicit
  non-zero / zero `dwMaxBytesPerSec` at body offset 4 of a 56-byte
  AVIMAINHEADER and checking the demuxer surface.

- **File-global `avih.dwMicroSecPerFrame` demux accessor + mux
  override (round 256).** Adds the typed
  `AviDemuxer::micro_sec_per_frame() -> Option<u32>` raw accessor
  returning the verbatim 32-bit value at byte offset 0 of the 56-byte
  AVIMAINHEADER body, the `avi:micro_sec_per_frame = "<N>"` decimal
  metadata key (omitted when the field carried the all-zero
  writer-skips-it sentinel), and the
  `AviMuxOptions::with_micro_sec_per_frame(n)` builder writing the
  supplied 32 bits verbatim at byte offset 0 of the AVIMAINHEADER body.
  Clean-room source: `docs/container/riff/avi-riff-file-reference.md`
  §"AVIMAINHEADER" Appendix A `dwMicroSecPerFrame` row (line 195):
  *"Number of microseconds between frames. Indicates the overall
  timing for the file."*

  Pre-round-256 the demuxer already parsed this DWORD internally to
  derive `duration_micros = total_frames * micro_sec_per_frame` (the
  source of truth for `Demuxer::duration`), but did not surface the
  raw 32-bit value verbatim — only the derived duration reached
  callers. This round adds the raw surface so a downstream tool can
  inspect the file-global frame-period independently of the derived
  duration and independently of the per-stream `(dwScale, dwRate)`
  pair surfaced via `stream_timebase` (round-249).

  The two surfaces — file-global `avih.dwMicroSecPerFrame` and
  per-stream `(strh.dwScale, strh.dwRate)` — can disagree. A capture
  pipeline may stamp a non-standard frame-period in the avih, or
  leave the avih field `0` even when the per-stream pair is
  populated. The demuxer reports both verbatim so a downstream tool
  can detect or repair any mismatch.

  Without an override the muxer keeps its long-standing computed
  default: derive the file-global frame period from the first video
  stream's `(dwScale, dwRate)` pair as `1_000_000 * scale / rate`, or
  `0` when no video stream is present (audio-only files). The
  override is `avih`-only — it does NOT touch the per-stream
  `(strh.dwScale, strh.dwRate)` pair (which a caller can override
  independently via `with_stream_timebase`, round-249), nor the
  muxer's internal duration / `dwMaxBytesPerSec` derivation, which
  both continue to source the frame period from the same
  first-video-stream packaging pair as before. Stamping a value that
  disagrees with `1_000_000 * stream0_scale / stream0_rate` is
  internally inconsistent on purpose — the long-standing convention
  that file-global byte-stamp overrides are byte-stamp-only.

  Mapping the all-zero sentinel to `None` on the accessor keeps the
  absent / writer-skipped case observable in `Option::is_none()` and
  omits the metadata key, mirroring the round-253 `fccType` /
  round-249 `(dwScale, dwRate)` / round-247 `dwFlags` / round-229
  `dwLength` / round-222 `dwSampleSize` / round-217
  `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
  `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
  round-157 file-global `dwInitialFrames` / round-153 per-stream
  `dwInitialFrames` / round-119 `wLanguage` / round-115 `rcFrame`
  "default == absent" convention this crate has carried since
  round-115.

  10 new tests in `tests/round256_avih_micro_sec_per_frame.rs`
  exercise: builder→writer→demuxer round-trip of a non-default frame
  period; the muxer's computed-default baseline (25fps video →
  40000us); audio-only baseline (no video stream → 0 → None); the
  override on an audio-only file stamping a nominal period anyway;
  builder idempotency; explicit-zero round-trip as `None`;
  `0xFFFF_FFFF` all-bits round-trip; independence from the per-stream
  `(scale, rate)` pair; and a pair of hand-rolled fixtures asserting
  the exact LE-byte-stamping at body offset 0 of the AVIMAINHEADER.

- **Per-stream `strh.fccType` demux accessor + mux override
  (round 253).** Adds the typed
  `AviDemuxer::stream_fcc_type(stream_index) -> Option<[u8; 4]>`
  raw-FOURCC accessor surfacing the 4 bytes at byte offset 0 of the
  56-byte AVISTREAMHEADER verbatim, the `avi:strh.<n>.fcc_type =
  "<fourcc-or-hex>"` metadata key (printable-vs-hex rendering, omitted
  when the strh carried the all-zero sentinel), and the
  `AviMuxOptions::with_stream_fcc_type(stream_index, fcc_type)`
  builder writing the supplied 4 bytes verbatim at byte offset 0 of
  the strh. Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md` §"AVISTREAMHEADER"
  (`fccType` row line 235 + the `fcc` row line 234): *"A FOURCC code
  that specifies the type of data contained in the stream. The
  following standard AVI values are defined: `auds` (audio stream),
  `mids` (MIDI stream), `txts` (text stream), `vids` (video stream)."*

  Without an override the muxer keeps its packaging-derived default
  (`vids` for video streams, `auds` for audio streams, per
  `packaging::StrfEntry::strh_type`). The override replaces the
  4 bytes verbatim at the byte-stamp site; it does NOT alter the
  muxer's media-kind routing (which is driven by the framework's
  `StreamInfo::params.media_type`, not the on-disk strh `fccType`),
  does NOT touch any sibling strh DWORD, and is NOT cross-validated
  against the encoder's chosen media kind. Stamping a `txts` type on
  a stream that's actually carrying PCM audio is internally
  inconsistent on purpose — the long-standing convention that
  side-band byte stamps are byte-stamp-only.

  Mapping the all-zero `[0, 0, 0, 0]` sentinel to `None` on the
  accessor keeps the absent / writer-skipped case observable in
  `Option::is_none()` and omits the metadata key, mirroring the
  round-249 `(dwScale, dwRate)` / round-247 `dwFlags` / round-229
  `dwLength` / round-222 `dwSampleSize` / round-217
  `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
  `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
  round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
  `rcFrame` "default == absent" convention this crate has carried
  since round-115. Non-standard FOURCCs outside the spec's documented
  `{auds, mids, txts, vids}` set (e.g. the legacy `iavs` interleaved
  DV stream FOURCC) surface verbatim — the spec phrases the standard
  values as illustrative rather than exhaustive, and the demuxer
  does NOT validate membership in the standard set. The metadata
  rendering follows the same printable-vs-hex split the
  `avi:strh.<n>.handler` key uses (`0x20..=0x7e` ASCII renders as
  four printable characters, otherwise `0xHHHHHHHH` lower-case hex).

  Tests in `tests/round253_strh_fcc_type.rs` (12 cases) cover the
  no-override packaging-default baseline (video `vids`, audio
  `auds`), video `mids` override round-trip, audio `txts` override
  round-trip, builder idempotency (last `with_stream_fcc_type` for
  a given index wins), vendor-FOURCC `iavs` round-trip, per-stream
  independence (an override on one stream doesn't perturb the
  other's readback), sibling-DWORD independence (`dwFlags` /
  `dwLength` / `dwSampleSize` / `dwSuggestedBufferSize` /
  `fccHandler` / `dwStart` / `wPriority` / `dwQuality` /
  `dwInitialFrames` / `wLanguage` / `(dwScale, dwRate)` all stay at
  their packaging defaults), hand-rolled fixtures for the
  printable / all-zero decode paths, non-printable bytes rendering
  as `0x00112233` hex in the metadata, and out-of-range stream
  index returning `None`. All 483 crate tests pass; `cargo fmt
  --check` and `cargo clippy --all-targets --no-deps -- -D warnings`
  clean.

- **Per-stream `(strh.dwScale, strh.dwRate)` demux accessor + mux
  override (round 249).** Adds the typed
  `AviDemuxer::stream_timebase(stream_index) -> Option<(u32, u32)>`
  raw-DWORD accessor surfacing the paired 32-bit DWORDs at byte
  offsets 20 + 24 of the 56-byte AVISTREAMHEADER verbatim, the
  `avi:strh.<n>.scale = "<N>"` + `avi:strh.<n>.rate = "<N>"` decimal
  metadata keys (both omitted when either DWORD is zero), and the
  `AviMuxOptions::with_stream_timebase(stream_index, scale, rate)`
  builder writing the supplied pair verbatim at byte offsets 20 / 24
  of the strh. Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md` §"AVISTREAMHEADER"
  (`dwScale` row line 241 + `dwRate` row line 242): *"Used with dwRate
  to specify the time scale that this stream will use. Dividing
  dwRate by dwScale gives the number of samples per second. For video
  streams, this is the frame rate. For audio streams, this rate
  corresponds to the time needed to play nBlockAlign bytes of audio,
  which for PCM audio is the just the sample rate."*

  Without an override the muxer keeps its packaging-derived defaults
  (video: per-stream `frame_rate` pair, audio: `sample_rate / 1`),
  matching the framework's [`oxideav_core::StreamInfo::time_base`]
  derivation. The override replaces both DWORDs verbatim at the
  byte-stamp site; it does NOT alter the muxer's `(scale, rate)`-
  derived `dwLength` computation for audio streams (which still uses
  the packaging-derived `t.entry.{scale,rate}` to convert running
  samples into `dwLength` units), does NOT touch
  `avih.dwMicroSecPerFrame` (the file-global frame-rate hint, which
  the muxer derives independently from the first video stream's
  packaging pair), and does NOT cross-validate against the per-stream
  `dwLength` or `dwStart`. Stamping an audio sample-rate pair on a
  video stream is internally inconsistent on purpose — the round-3
  long-standing convention that side-band byte stamps are
  byte-stamp-only.

  Mapping the writer-skips-it sentinel (`0` in either DWORD, the
  mathematically-undefined `rate/scale` ratio) to `None` on the
  accessor keeps the absent / degenerate case observable in
  `Option::is_none()` and omits both metadata keys, mirroring the
  round-247 `dwFlags` / round-229 `dwLength` / round-222
  `dwSampleSize` / round-217 `dwSuggestedBufferSize` / round-210
  `fccHandler` / round-203 `dwStart` / round-182 `wPriority` /
  round-176 `dwQuality` / round-153 `dwInitialFrames` / round-119
  `wLanguage` / round-115 `rcFrame` "default == absent" convention
  this crate has carried since round-115. The internal
  `StreamInfo::time_base` derivation still applies `.max(1)`
  separately to each DWORD so a degenerate file remains decodable;
  the raw-DWORD surface keeps the on-disk byte pattern observable
  for round-trip parity.

  Logically distinct from the framework-level
  `StreamInfo::time_base` (typed as `Rational` with `i64` members):
  the framework's `time_base` is the normalised time-base the
  framework's rescale / PTS arithmetic uses, while the raw `u32`
  surface keeps the value byte-exact for callers that need to
  compare against a separately-emitted writer's stamp or stamp an
  identical pair on re-mux. The two values agree whenever the strh
  pair has both members non-zero (which is the universal case in
  legitimate AVIs — the `0` sentinel is the truncated / zero-padded
  / hand-crafted edge case the `.max(1)` clamp covers).

  Tests in `tests/round249_strh_timebase.rs` (14 cases) cover the
  no-override packaging-default baseline (video 25 fps, audio 48 kHz),
  video NTSC `(1001, 30000)` round-trip, audio CD `(1, 44100)`
  round-trip, builder idempotency (last `with_stream_timebase` for
  a given index wins), `u32::MAX` boundary on both members,
  per-stream independence (an override on one stream doesn't
  perturb the other's readback), sibling-DWORD independence
  (`dwFlags` / `dwLength` / `dwSampleSize` /
  `dwSuggestedBufferSize` / `fccHandler` / `dwStart` / `wPriority`
  / `dwQuality` / `dwInitialFrames` / `wLanguage` all stay at
  their packaging defaults), the override shifting the
  framework-level `StreamInfo::time_base`, hand-rolled fixtures for
  the non-zero / zero-scale / zero-rate / zero-both decode paths,
  and out-of-range stream index returning `None`. All 471 crate
  tests pass; `cargo fmt --check` and `cargo clippy --all-targets
  --no-deps -- -D warnings` clean.

- **Per-stream `strh.dwFlags` (`AVISF_*`) demux accessors + mux
  override (round 247).** Adds the typed
  `AviDemuxer::stream_flags(stream_index) -> Option<u32>` raw
  accessor + the typed
  `AviDemuxer::stream_flags_typed(stream_index) -> Option<StrhFlags>`
  decoded accessor exposing the two AVI 1.0-documented `AVISF_*` bits
  as named `bool` fields, the public `AVISF_DISABLED`
  (`0x0000_0001`) / `AVISF_VIDEO_PALCHANGES` (`0x0001_0000`)
  constants, the `avi:strh.<n>.flags = "0xXXXXXXXX"` upper-case-hex
  metadata key (omitted on the `0` "no flags set" legacy default),
  and the `AviMuxOptions::with_stream_flags(stream_index, flags)`
  builder writing the supplied 32-bit value verbatim into byte
  offset 8 of the 56-byte AVISTREAMHEADER. Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md` §"AVISTREAMHEADER"
  (`dwFlags` row line 237) + the *dwFlags values* table at lines
  252–255: *"AVISF_DISABLED — Indicates this stream should not be
  enabled by default."* and *"AVISF_VIDEO_PALCHANGES — Indicates this
  video stream contains palette changes. This flag warns the playback
  software that it will need to animate the palette."*

  The typed-decode `StrhFlags` struct mirrors the round-10 candidate-3
  `AvihFlags` shape: every documented bit gets its own `bool` and the
  raw DWORD is preserved in `bits` so undocumented vendor / driver
  bits in the upper half-DWORD (some legacy capture filters tag
  driver-private state there) stay observable instead of being
  silently masked. The demuxer does NOT validate against the spec's
  two documented bits and the muxer does NOT cross-validate against
  other strh fields — stamping `AVISF_VIDEO_PALCHANGES` on an audio
  stream is internally inconsistent on purpose, mirroring the round-3
  long-standing convention that side-band byte stamps are
  byte-stamp-only.

  Mapping the `0` default to `None` (rather than `Some(0)`) on the
  accessor keeps the "no flags set" case observable in
  `Option::is_none()` and omits the `avi:strh.<n>.flags` metadata
  key, mirroring the round-229 `dwLength` / round-222 `dwSampleSize`
  / round-217 `dwSuggestedBufferSize` / round-210 `fccHandler` /
  round-203 `dwStart` / round-182 `wPriority` / round-176 `dwQuality`
  / round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
  `rcFrame` "default == absent" convention this crate has carried
  since round-115. The override only changes the byte stamp at strh
  offset 8 and does NOT touch the file-global `avih.dwFlags` already
  surfaced via `avih_flags()` / `AvihFlags` (round-10 candidate 3) —
  the two flag DWORDs are spec-independent. The pre-round-247 muxer
  has always stamped `0` here; the override pairs with the existing
  per-packet palette-change `xxpc` chunk emission
  (`AviMuxer::write_palette_change`) so a caller can produce a fully
  AVI-1.0-conformant palette-animating video stream by stamping
  `AVISF_VIDEO_PALCHANGES` alongside the per-packet palette records.

- **OpenDML `LIST odml dmlh.dwTotalFrames` muxer-side override
  (round 234).** Adds the
  `AviMuxOptions::with_dmlh_total_frames(n)` builder writing the
  supplied 32-bit value verbatim into the `dmlh` chunk body at the
  `write_trailer` patch site, replacing the long-standing
  auto-derived primary-video-stream `packet_count` default
  (`TrackState::packet_count` is not reset across segments, so the
  default already folds every AVIX continuation packet). Clean-room
  source: `docs/container/riff/opendml-avi-2.0.pdf` §5.0 "Extended
  AVI Header" defines `dmlh.dwTotalFrames` as the "real total frame
  count across every `RIFF AVIX` segment", whereas
  `avih.dwTotalFrames` only counts the primary segment.

  The two counts can legitimately disagree in edge cases the
  auto-derived value can't reach: a writer that knows the full
  sequence length ahead of time (fixed-budget capture pre-allocating
  a target frame count, an edit-list trimming the physical packet
  stream, a streamer rounding to a known playlist boundary); a
  chained AVIX continuation file that was emitted by a separate
  process and concatenated post-hoc; or a fuzz / regression fixture
  deliberately exercising the demuxer's
  `super_index_duration_violations` cross-check against a stamped
  mismatch. The override is dmlh-only — it does NOT touch
  `avih.dwTotalFrames` (the primary-segment count, derived from the
  video stream's `packet_count`), does NOT touch any per-stream
  `strh.dwLength`, and does NOT alter any downstream `idx1` / `ix##`
  derivation, so a stamp that disagrees with the actual segment
  frame totals is internally inconsistent on purpose and surfaces
  through `super_index_duration_violations()` on re-demux. Only
  meaningful in `AviKind::OpenDml` mode; silently a no-op in
  `AviKind::Avi10` (no `LIST odml` is emitted at all). Duplicate
  builder calls replace the prior value; passing `0` stamps a
  structurally-present `dmlh` chunk with a zero body — the typed
  `AviDemuxer::dmlh_total_frames()` returns `Some(0)` and the
  `avi:total_frames_all_segments` metadata key surfaces as `"0"`
  (the absence-vs-zero distinction is *whether the chunk is
  emitted*, controlled by the envelope variant, not the stamped
  value).

  Covered by a 12-test suite exercising the auto-derived baseline,
  override round-trip via the typed accessor + metadata key,
  builder idempotency (last call per builder wins), explicit `0`
  round-tripping as `Some(0)` with the metadata key emitted as
  `"0"`, boundary values (`1` / `u32::MAX` / 90 000 frames =
  60 minutes @ 25 fps), mismatch-surfaces-via-violations on a
  4 KiB-ceiling multi-segment file, `avih.dwTotalFrames` invariance
  through the `duration_micros` derived value, `AviKind::Avi10`
  no-op (the typed accessor returns `None` and the metadata key is
  omitted), idx1 entry count invariance, and a hand-rolled minimal
  RIFF fixture confirming the dmlh DWORD decodes verbatim via the
  typed accessor.

- **Per-stream `strh.dwLength` parse + emit + round-trip
  (round 229).** Surfaces the `dwLength` field at byte offset 32 of
  the 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER".
  Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md`
  (`dwLength` row, line 244): *"Length of this stream. The units are
  defined by the dwRate and dwScale members of the stream's
  header."* The 32 raw byte was already read internally as the
  `length` local used to derive `StreamInfo::duration`; this round
  adds the public per-stream surface:

  * typed `AviDemuxer::stream_length(stream_index) -> Option<u32>`
    accessor mapping the `0` "no length declared" value back to
    `None` so an unspecified length reads the same as an absent one
    — mirroring the round-222 `dwSampleSize` / round-217
    `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
    `rcFrame` / round-80 `strn` / round-107 `IDIT` "default ==
    absent" convention,
  * `avi:strh.<n>.length` metadata key (omitted for the `0` value
    to keep absence observable),
  * `AviMuxOptions::with_stream_length(stream_index, n)` builder
    writing the supplied 32-bit value verbatim at byte offset 32 of
    the strh at the `write_trailer` / `patch_post_counts` site,
    replacing the auto-derived per-stream packet / sample count
    (last call per stream-index wins via retain-then-push; no
    validation against the actual chunk count).

  Pre-round-229 the muxer always patched the auto-derived value
  (video: `packet_count`; audio PCM / CBR: running `sample_count`
  from the muxer's `size / sample_size` formula) — those byte
  writes are preserved verbatim when no override is supplied. The
  override only changes the byte stamp at offset 32; it does NOT
  touch `avih.dwTotalFrames` (per-stream length and the file-global
  total are spec-independent fields), and does NOT alter any
  downstream `idx1` / `ix##` / `dmlh` derivation, so a caller that
  stamps a `dwLength` incompatible with their actual chunk count is
  creating an internally-inconsistent file on purpose (e.g. to
  reproduce a half-written legacy capture dump, a fixed-budget
  streamer's playlist-boundary stamp, or a pathological writer for
  fuzz / regression purposes). The `StreamInfo::duration` exposed
  by `Demuxer::streams` continues to track the raw stamp (the
  framework already derives duration from this same DWORD).

  Logically distinct from `StreamInfo::duration` already exposed
  by `oxideav_core::Demuxer::streams` (also derived from this same
  DWORD but typed as `Option<i64>` for the framework-level
  duration model); the raw-u32 surface keeps the value observable
  verbatim for callers that need byte-exact round-trip semantics
  or comparison against a separately-emitted writer's stamp.
  Covered by a 14-test suite exercising the auto-derived baseline
  (video: 1 packet ⇒ `Some(1)`; audio: 8-byte PCM payload /
  nBlockAlign=4 ⇒ `Some(2)`), video override round-trip (typed
  accessor + metadata key), audio override replacing the
  auto-derived `sample_count`, builder idempotency, the explicit
  `0` round-tripping as `None` with the metadata key omitted,
  boundary values (`1` / `u32::MAX` / a typical 90 000-frame
  long-form-capture count), independence across streams,
  independence from sibling strh DWORDs (`dwSampleSize` /
  `dwSuggestedBufferSize` / `fccHandler` / `dwStart` / `wPriority`
  / `dwQuality` / `dwInitialFrames` / `wLanguage` all unaffected),
  `StreamInfo::duration` agreement with the raw stamp, hand-rolled
  fixtures for explicit non-zero / zero `dwLength` decode, and
  the out-of-range stream index returning `None` on the typed
  accessor.

- **Per-stream `strh.dwSampleSize` parse + emit + round-trip
  (round 222).** Surfaces the `dwSampleSize` indicator at byte offset
  44 of the 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER".
  Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md`
  (`dwSampleSize` row, line 247): *"The size of a single sample of
  data. This is set to zero if the samples can vary in size. If this
  number is nonzero, then multiple samples of data can be grouped into
  a single chunk within the file. If it is zero, each sample of data
  (such as a video frame) must be in a separate chunk. For video
  streams, this number is typically zero, although it can be nonzero
  if all video frames are the same size. For audio streams, this
  number should be the same as the nBlockAlign member of the
  WAVEFORMATEX structure describing the audio."* The 44 raw byte was
  already captured internally for the round-14 C2 audio sample-size
  invariant (the VBR / CBR consistency check that fires at `open_avi`
  time); this round adds the public per-stream surface:

  * typed `AviDemuxer::stream_sample_size(stream_index) -> Option<u32>`
    accessor mapping the spec-documented `0` "samples can vary in
    size" sentinel back to `None` so an unspecified hint reads the
    same as an absent one — mirroring the round-217
    `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
    `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
    round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
    `rcFrame` / round-80 `strn` / round-107 `IDIT` "default == absent"
    convention,
  * `avi:strh.<n>.sample_size` metadata key (omitted for the `0`
    sentinel to keep absence observable),
  * `AviMuxOptions::with_stream_sample_size(stream_index, n)` builder
    writing the supplied 32-bit value verbatim at byte offset 44 of
    the strh (last call per stream-index wins via retain-then-push;
    no validation against `WAVEFORMATEX.nBlockAlign` or any observed
    chunk-size pattern in `movi`).

  Pre-round-222 the muxer always stamped the packaging-derived default
  (`nBlockAlign` for PCM / CBR audio, `0` for VBR audio, `0` for
  video) — those byte writes are preserved verbatim when no override
  is supplied. The override only changes the byte stamp at offset 44;
  it does NOT alter the muxer's own `dwLength` derivation (the audio
  `size / sample_size` formula keeps using the packaging-derived
  `entry.sample_size`), so a caller that stamps a `dwSampleSize`
  incompatible with their packet stream is creating an internally-
  inconsistent file on purpose and will need `open_avi_lenient` to
  read it back (the round-14 C2 invariant correctly rejects PCM
  streams with `dwSampleSize == 0` and VBR streams with
  `dwSampleSize > 0` under strict open). Covered by a 12-test suite
  exercising the audio-PCM baseline (auto-derived `nBlockAlign = 4`
  for 2-channel s16le), video override round-trip (typed accessor +
  metadata key), builder idempotency, the explicit-`0`-on-audio
  invariant fire under strict open + `None` readback under lenient
  open, boundary values (`1` / `u32::MAX` / a typical 1280×720 raw
  frame size), independence across streams, independence from sibling
  strh DWORDs (`dwSuggestedBufferSize` / `fccHandler` / `dwStart` /
  `wPriority` / `dwQuality` / `dwInitialFrames` / `wLanguage` all
  unaffected), and hand-rolled fixtures for explicit non-zero / zero
  `dwSampleSize` decode.

- **Per-stream `strh.dwSuggestedBufferSize` parse + emit + round-trip
  (round 217).** Surfaces the `dwSuggestedBufferSize` read-ahead hint
  at byte offset 36 of the 56-byte AVISTREAMHEADER per AVI 1.0
  §"AVISTREAMHEADER". Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md`
  (`dwSuggestedBufferSize` row, line 245): *"How large a buffer should
  be used to read this stream. Typically, this contains a value
  corresponding to the largest chunk present in the stream. Using the
  correct buffer size makes playback more efficient. Use zero if you
  do not know the correct buffer size."* The muxer already
  auto-derived this DWORD from `t.max_chunk_size` (the largest body
  observed on the stream during `write_packet`) and patched it into
  the strh at the end of `write_trailer`; this round adds the typed
  `AviDemuxer::stream_suggested_buffer_size(stream_index) -> Option<u32>`
  accessor (mapping the spec-documented `0` "do not know the correct
  buffer size" sentinel back to `None` so an unspecified hint reads
  the same as an absent one, mirroring the round-210 `fccHandler` /
  round-203 `dwStart` / round-182 `wPriority` / round-176 `dwQuality`
  / round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
  `rcFrame` / round-80 `strn` / round-107 `IDIT` "default == absent"
  convention), the `avi:strh.<n>.suggested_buffer_size` metadata key
  (omitted for the `0` sentinel to keep absence observable), and the
  muxer builder
  `AviMuxOptions::with_stream_suggested_buffer_size(stream_index, n)`
  writing the supplied 32-bit value verbatim at byte offset 36 (last
  call per stream-index wins via retain-then-push; no validation
  against the actual largest chunk observed in `movi`, since
  over-declaration is the documented intent of the field and some
  legacy capture tools stamp a fixed read-ahead budget independent of
  their occasional peak). The new accessor is logically distinct
  from the file-global `AviDemuxer::avih_suggested_buffer_size()`
  already exposed for the `avih.dwSuggestedBufferSize` DWORD — the
  avih flavour covers the largest chunk across every stream, the
  strh flavour is a per-stream upper bound, and the two are
  spec-independent (writers may stamp consistent values, set only
  one, or leave both at the `0` sentinel). Fourteen regression
  tests cover the mux→demux round-trip via the typed accessor and
  the metadata key, the no-override baseline (auto-derived
  `t.max_chunk_size` surfacing per stream), builder idempotency
  (last call per index wins), the explicit `0` override stamping
  the spec-documented "do not know" sentinel (demuxer maps to
  `None`, metadata key omitted), boundary values `1` and
  `u32::MAX`, over-declaration of the hint relative to the actual
  largest chunk (round-trips verbatim per the spec's *upper bound*
  framing), under-declaration of the hint (also round-trips
  verbatim — the demuxer does not second-guess the writer),
  per-stream independence (an override on one stream doesn't
  perturb another's auto-derived default), an out-of-range
  stream-index accessor returning `None`, sibling-DWORD
  independence (stamping `dwSuggestedBufferSize` leaves
  `fccHandler` / `dwStart` / `wPriority` / `dwQuality` /
  `dwInitialFrames` / `wLanguage` readbacks at their own defaults),
  spec-independence from the file-global `avih.dwSuggestedBufferSize`
  (the per-stream strh value and the file-global avih value
  round-trip without bleeding into each other), and two hand-rolled
  fixtures (a 56-byte strh with a non-zero `dwSuggestedBufferSize`
  of `0xDEAD_BEEF` decoding verbatim, and an all-zero
  `dwSuggestedBufferSize` parsing as `None`).

- **Per-stream `strh.fccHandler` parse + emit + round-trip (round 210).**
  Surfaces the `fccHandler` driver-handler FourCC at byte offset 4 of
  the 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER".
  Clean-room source: `docs/container/riff/avi-riff-file-reference.md`
  Appendix B (`fccHandler` row, line 236): *"An optional FOURCC that
  identifies a specific data handler. The data handler is the
  preferred handler for the stream. For audio and video streams, this
  specifies the codec for decoding the stream."* The muxer already
  wrote a packaging-derived FourCC here on every stream (video streams
  mirror `BITMAPINFOHEADER.biCompression`, audio streams default to
  the all-zero `\0\0\0\0` "no preferred handler" value); this round
  adds the typed
  `AviDemuxer::stream_handler(stream_index) -> Option<[u8; 4]>`
  accessor (mapping the all-zero default — per the spec's *optional
  FOURCC* qualifier — back to `None` so an unspecified hint reads
  the same as an absent one, mirroring the round-203 `dwStart` /
  round-182 `wPriority` / round-176 `dwQuality` / round-153
  `dwInitialFrames` / round-119 `wLanguage` / round-115 `rcFrame` /
  round-80 `strn` / round-107 `IDIT` "default == absent"
  convention), the `avi:strh.<n>.handler` metadata key (printable
  four-character ASCII when every byte is in the `0x20..=0x7e`
  range so e.g. `MJPG` / `iv32` / `DIB ` round-trip legibly,
  `0xHHHHHHHH` lower-case hex form otherwise so a binary or
  vendor-specific driver token still round-trips uniquely; omitted
  for the all-zero default case to keep absence observable), and
  the muxer builder
  `AviMuxOptions::with_stream_handler(stream_index, fourcc)` writing
  the supplied 4 bytes verbatim at byte offset 4 (last call per
  stream-index wins via retain-then-push; no printability validation
  — the spec's *optional FOURCC* phrasing does not pin it; passing
  `[0, 0, 0, 0]` is equivalent to omitting the override for audio
  streams whose default is also all-zero, and explicitly zeroes the
  field on video streams overriding the `biCompression`-mirror
  default). Thirteen regression tests cover the mux→demux round-trip
  via the typed accessor and the metadata key, the no-override
  baseline (video mirrors `biCompression`, audio reads back as
  `None`), builder idempotency (last call per index wins), the
  explicit `[0, 0, 0, 0]` override clearing the video stream's
  `biCompression` mirror, an explicit audio-stream stamp on a stream
  whose default would be all-zero, three boundary forms (a
  printable `DIB ` rendering as `"DIB "`, a non-printable
  `[0xFF; 4]` rendering as `"0xffffffff"`, a mixed-byte
  `[A,0x1f,C,D]` rendering as `"0x411f4344"` per the helper's
  all-or-nothing range check), per-stream independence (a handler
  on one stream doesn't perturb another's packaging default), an
  out-of-range stream-index accessor returning `None`, sibling-DWORD
  independence (stamping `fccHandler` leaves `dwStart` /
  `wPriority` / `dwQuality` / `dwInitialFrames` / `wLanguage`
  readbacks at their own defaults), and two hand-rolled fixtures
  validating that an `iv32` byte-offset-4 FourCC decodes to the
  expected raw `[u8; 4]` and an all-zero one parses as `None`. The
  `fccHandler` field is logically distinct from
  `BITMAPINFOHEADER.biCompression` (video) and `WAVEFORMATEX.wFormatTag`
  (audio); writers in the wild typically mirror `biCompression`
  into `fccHandler` on video streams, but the spec does not require
  the two to match and the override path lets callers preserve a
  driver-suite identifier distinct from `biCompression`.

- **Per-stream `strh.dwStart` parse + emit + round-trip (round 203).**
  Surfaces the `dwStart` starting-time DWORD at byte offset 28 of the
  56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER".
  Clean-room source: `docs/container/riff/avi-riff-file-reference.md`
  Appendix B (`dwStart` row, line 243): *"Starting time for this
  stream. The units are defined by the dwRate and dwScale members in
  the main file header. Usually, this is zero, but it can specify a
  delay time for a stream that does not start concurrently with the
  file."* The muxer already wrote `0` here since round 3; this round
  adds the typed
  `AviDemuxer::stream_start(stream_index) -> Option<u32>` accessor
  (mapping the `0` legacy writer default — the spec-documented "starts
  concurrently with the file" value — back to `None` so an unspecified
  start reads the same as an absent one, mirroring the round-182
  `wPriority` / round-176 `dwQuality` / round-153 `dwInitialFrames` /
  round-119 `wLanguage` / round-115 `rcFrame` / round-80 `strn` /
  round-107 `IDIT` "default == absent" convention), the
  `avi:strh.<n>.start` metadata key (omitted for the default-zero case
  to keep absence observable), and the muxer builder
  `AviMuxOptions::with_stream_start(stream_index, start)` writing the
  supplied 32-bit value verbatim at byte offset 28 (last call per
  stream-index wins via retain-then-push; no validation against the
  per-stream `dwLength` — the unit is the stream's own
  `(dwRate / dwScale)` tick and the demuxer surfaces the raw u32
  verbatim with no rate-conversion). Eleven regression tests cover
  the mux→demux round-trip via the typed accessor and the metadata
  key, the no-override baseline, builder idempotency, the explicit
  `0` override mapping back to `None`, the boundary values `1` and
  `u32::MAX` (the spec pins no range so neither extreme is
  special-cased), per-stream independence, the out-of-range
  stream-index accessor case, independence from sibling per-stream
  DWORDs (`wPriority` / `dwQuality` / `dwInitialFrames` / `wLanguage`
  all stay decoupled), and hand-rolled fixtures that pin the exact
  byte-offset-28 layout.

- **OpenDML `indx` super-index `bIndexSubType` surface (round 197).**
  Per the AVISUPERINDEX layout in
  `docs/container/riff/avi-riff-file-reference.md` Appendix F
  (`bIndexSubType` row: *"The index subtype. The value must be zero
  or AVI_INDEX_SUB_2FIELD."*) the super-index inherits the sub-type
  of the pointed-to per-segment `ix##` standard indexes — so an
  OpenDML reader that sees `AVI_INDEX_SUB_2FIELD` on the super-index
  knows the pointed-to segments will carry 12-byte 2-field
  `(dwOffset, dwSize, dwOffsetField2)` entries. The muxer (round-4
  P1) already stamps this byte for streams registered via
  `AviMuxOptions::with_field2_stream`; the demuxer parsed it but
  never surfaced it, so callers had to wait for the in-`movi` `ix##`
  scan to fire the existing `avi:ix.<n>.is_2field` hint. Round-197
  adds the typed
  `AviDemuxer::super_index_sub_type(stream_index) -> Option<u8>`
  accessor (returns the raw byte verbatim; `None` distinguishes "no
  super-index declared" from "super-index sub-type 0"), the
  `AviDemuxer::super_index_is_2field(stream_index) -> bool`
  convenience boolean, and the `avi:indx.<n>.sub_type_2field`
  metadata key (only emitted when the byte is
  `AVI_INDEX_SUB_2FIELD == 0x01`; the `0` default is omitted so
  absence stays observable, mirroring the round-176/153/119/115/107
  "default == absent" convention). Three regression tests cover the
  2-field round-trip, the default-subtype default-suppression, and
  the AVI 1.0 "no super-index" case.

## [0.0.8](https://github.com/OxideAV/oxideav-avi/compare/v0.0.7...v0.0.8) - 2026-05-29

### Other

- per-stream strh.wPriority parse + emit (round 182)
- per-stream strh.dwQuality parse + emit (round 176)
- typed WAVEFORMATEXTENSIBLE.dwChannelMask surface (round 163)
- file-global avih.dwInitialFrames parse + emit (round 157)
- per-stream strh.dwInitialFrames parse + emit (round 153)
- strh.wLanguage per-stream LANGID parse + emit (round 119)

### Added

- **Per-stream `strh.wPriority` parse + emit + round-trip (round 182).**
  Surfaces the `wPriority` selection-hint field at byte offset 12 of
  the 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER".
  Clean-room source: `docs/container/riff/avi-riff-file-reference.md`
  Appendix B (`wPriority` row, line 238): *"Priority of a stream type.
  For example, in a file with multiple audio streams, the one with the
  highest priority might be the default stream."* The muxer already
  wrote `0` here since round 3; this round adds the typed
  `AviDemuxer::stream_priority(stream_index) -> Option<u16>` accessor
  (mapping the `0` legacy writer default back to `None` so an
  unspecified priority reads the same as an absent one, mirroring the
  round-176 `strh.dwQuality` / round-153 `dwInitialFrames` /
  round-119 `wLanguage` / round-115 `rcFrame` / round-80 `strn` /
  round-107 `IDIT` "default == absent" convention), the
  `avi:strh.<n>.priority` metadata key (omitted entirely when the
  value is the `0` default), and the
  `AviMuxOptions::with_stream_priority(stream_index, p)` builder that
  stamps a non-default selection hint into byte offset 12 of the strh
  so the demuxer can round-trip it. The spec describes the field as
  a per-`fccType` selection hint (the multi-audio-stream illustration
  picks the default-playback stream), not a sortable global priority
  — it pins no value range or tie-break rule, so the demuxer surfaces
  the raw 16-bit DWORD verbatim and applications that use the field
  for ad-hoc tagging round-trip exactly. Adds 10 new tests in
  `tests/round182_strh_priority.rs` covering the mux→demux round-trip
  via the typed accessor and the metadata key, the no-override
  baseline (legacy `0` reads as absent), builder idempotency, the
  explicit `0` override, the boundary values `1` and `u16::MAX`,
  per-stream independence, the out-of-range accessor case,
  independence from sibling per-stream DWORDs (`dwQuality` /
  `dwInitialFrames` / `wLanguage`), and hand-rolled fixtures for the
  exact byte-offset-12 layout.
- **Per-stream `strh.dwQuality` parse + emit + round-trip (round 176).**
  Surfaces the `dwQuality` quality-indicator field at byte offset 40 of
  the 56-byte AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER".
  Clean-room source: `docs/container/riff/avi-riff-file-reference.md`
  Appendix B (`dwQuality` row, line 246): *"Indicator of the quality of
  the data in the stream. Quality is represented as a number between 0
  and 10,000. For compressed data, this typically represents the value
  of the quality parameter passed to the compression software. If set
  to -1, drivers use the default quality value."* The muxer already
  wrote `0xFFFF_FFFF` (= `-1` as i32, the documented "use default
  driver quality" sentinel) here since round 3; this round adds the
  typed `AviDemuxer::stream_quality(stream_index) -> Option<u32>`
  accessor (mapping the documented `-1` sentinel back to `None` so an
  unspecified quality reads the same as an absent one, mirroring the
  round-153 `strh.dwInitialFrames` / round-119 `wLanguage` / round-115
  `rcFrame` / round-80 `strn` / round-107 `IDIT` "default == absent"
  convention), the `avi:strh.<n>.quality` metadata key (omitted
  entirely when the value is the `-1` sentinel), and the
  `AviMuxOptions::with_stream_quality(stream_index, q)` builder that
  stamps any 32-bit value verbatim at byte offset 40. The spec's
  documented `[0, 10_000]` range is informational only — values in
  that range surface verbatim, but `0` is *not* treated as default
  (only the explicit `0xFFFF_FFFF` sentinel is), and out-of-range
  writers round-trip exactly without clamp. The per-stream
  `dwQuality` is independent of the file-global `avih`-side DWORDs and
  of the round-153 per-stream `dwInitialFrames` / round-119
  `wLanguage` / round-115 `rcFrame` siblings — none bleed into each
  other. Covered by 11 new tests in `tests/round176_strh_quality.rs`:
  mux→demux round-trip, default baseline (sentinel == absent), builder
  idempotency (per-index `retain`-then-`push`), explicit `-1`
  override, documented-range endpoints (`0` and `10_000`), out-of-spec
  values (`0x0001_0000`, `0x7FFF_FFFE`), per-stream independence,
  out-of-range stream-index accessor, sibling-DWORD independence, and
  hand-rolled fixtures (explicit non-default + all-ones controlling
  the exact strh bytes at offset 40).
- **Typed `WAVEFORMATEXTENSIBLE.dwChannelMask` surface (round 163).**
  New `ChannelMask` newtype + `Speaker` enum + `ChannelLayout`
  recogniser in `stream_format`, plus
  `AviDemuxer::stream_channel_mask_typed(stream) -> Option<ChannelMask>`
  and `AviDemuxer::stream_channel_layout(stream) -> Option<ChannelLayout>`
  accessors. Clean-room source:
  `docs/container/riff/waveformatextensible/README.md` (Microsoft Learn
  mirror, 2026-05-18) — verbatim from the "Channel-mask channel
  ordering" and "Standard layouts" tables. `Speaker` covers the 18
  documented positional `SPEAKER_*` bits (`FrontLeft` 0x00001 through
  `TopBackRight` 0x20000) plus the `SpeakerAll` (0x80000000)
  top-bit catch-all; `ChannelMask::iter_speakers` enumerates them in
  PCM byte-stream channel order (lowest set bit first per docs §
  "Channel-mask channel ordering"). `ChannelLayout` recognises the
  seven docs-table named layouts: `Mono` (FC, 0x00004), `Stereo`
  (FL|FR, 0x00003), `TwoPointOne` (FL|FR|LFE, 0x0000B), `Quad`
  (FL|FR|BL|BR, 0x00033), `FivePointOneBack` (Microsoft 5.1:
  FL|FR|FC|LFE|BL|BR, 0x0003F), `FivePointOneSide` (DVD-style 5.1:
  FL|FR|FC|LFE|SL|SR, 0x0060F), and `SevenPointOne` (7.1:
  FL|FR|FC|LFE|BL|BR|SL|SR, 0x0063F). `ChannelMask::reserved_bits`
  isolates bits in the Microsoft `SPEAKER_RESERVED` gap (between
  `TopBackRight` and `SpeakerAll`) so a stereo stream that also has
  a stray reserved bit still classifies as `Stereo` while the caller
  can detect the anomaly out-of-band. Two new metadata keys land on
  every extensible audio stream alongside `avi:auds.<n>.channel_mask`:
  `avi:auds.<n>.channel_speakers` (comma-joined `Speaker::abbrev()`
  abbreviations like `"FL,FR,FC,LFE,BL,BR"`, surfaced for any non-empty
  mask) and `avi:auds.<n>.channel_layout` (named-layout label like
  `"stereo"` / `"5.1(back)"` / `"5.1(side)"` / `"7.1"`, omitted when
  the mask doesn't match one of the seven named layouts so absence
  of a key stays observable — mirrors the `avi:strn` / `avi:strd` /
  `avi:idit` "default == absent" metadata convention). Legacy 18-byte
  `WAVEFORMATEX` audio streams (`wFormatTag != 0xFFFE`) return `None`
  for both new typed accessors, matching the existing
  `stream_channel_mask` precondition. 12 new tests (6 unit
  + 6 integration in `tests/round163_channel_layout.rs`) cover the
  full bit-order table, the seven named-layout round-trips, reserved-
  bit isolation, the `SPEAKER_ALL` catch-all, the new metadata keys,
  and the legacy-`WAVEFORMATEX` `None`-gating path.

- **File-global `avih.dwInitialFrames` parse + emit + round-trip
  (round 157).** Surfaces the `dwInitialFrames` interleave-skew field
  at byte offset 16 of the 56-byte AVIMAINHEADER body (byte 24 of the
  `avih` chunk). Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md` Appendix A
  (`dwInitialFrames` row, line 200): *"Initial frame for interleaved
  files. Noninterleaved files should specify zero. If creating
  interleaved files, specify the number of frames in the file prior to
  the initial frame of the AVI sequence."* The muxer already wrote `0`
  here since round 3; this round adds the typed
  `AviDemuxer::initial_frames() -> Option<u32>` accessor (mapping the
  `0` "noninterleaved file" sentinel to `None` so an unspecified skew
  reads the same as an absent one, mirroring the round-153 per-stream
  `strh.dwInitialFrames` / round-119 `wLanguage` / round-115 `rcFrame`
  / round-80 `strn` "default == absent" convention), the
  `avi:initial_frames` metadata key (omitted entirely when the value
  is `0`, like `avi:padding_granularity`), and the
  `AviMuxOptions::with_initial_frames(n)` builder that stamps any
  32-bit value verbatim at body offset 16. The new field is the
  file-global counterpart of the per-stream `strh.dwInitialFrames`
  (round 153); the two DWORDs are independent per spec and round-trip
  without bleeding into each other. Covered by 8 new tests in
  `tests/round157_avih_initial_frames.rs`: mux→demux round-trip,
  default-baseline (zero == absent), builder idempotency,
  explicit-zero override, all-ones round-trip, file-global vs
  per-stream independence (3 sub-scenarios), and hand-rolled fixtures
  (non-zero + all-zero) controlling the exact `avih` bytes at offset
  16.
- **Per-stream `strh.dwInitialFrames` parse + emit + round-trip
  (round 153).** Surfaces the `dwInitialFrames` interleave-skew field at
  byte offset 16 of the 56-byte AVISTREAMHEADER. Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md` §"AVISTREAMHEADER"
  (`dwInitialFrames` row): *"How far audio data is skewed ahead of the
  video frames in interleaved files. Typically, this is about 0.75
  seconds. If creating interleaved files, set the value of this member
  to the number of frames in the file prior to the initial frame of the
  AVI sequence in this member."* AVIMAINHEADER §`dwInitialFrames` adds:
  *"Initial frame for interleaved files. Noninterleaved files should
  specify zero."* The muxer already wrote `0` here for every stream;
  this round adds the typed `AviDemuxer::stream_initial_frames(stream)
  -> Option<u32>` accessor returning the raw 32-bit value from byte
  offset 16, the `avi:strh.<n>.initial_frames` metadata key (omitted
  when zero so absence stays observable, mirroring the round-119
  `wLanguage` / round-115 `rcFrame` / round-80 `strn` "default ==
  absent" convention), and the
  `AviMuxOptions::with_stream_initial_frames(stream_index, frames)`
  builder that stamps any 32-bit value verbatim. The unit is the
  stream's own `dwRate` / `dwScale` tick (typically frames for video,
  blocks for audio); no rate-conversion or validation against the
  per-stream `dwLength`. Covered by 10 new tests in
  `tests/round153_initial_frames.rs`: mux→demux round-trip on video and
  audio streams, default baseline, builder dedup, explicit-zero
  override, per-stream independence, out-of-range index, all-ones
  round-trip, and hand-rolled fixtures (non-zero + all-zero) controlling
  the exact strh bytes.

## [0.0.7](https://github.com/OxideAV/oxideav-avi/compare/v0.0.6...v0.0.7) - 2026-05-24

### Other

- make round104 vprp temp-file names collision-proof
- strh.rcFrame destination rectangle parse + emit (round 115)
- ISMP SMPTE-timecode chunk parse + emit (round 112)
- IDIT digitization-date chunk parse + emit (round 107)
- typed frame-aspect-ratio accessor (round 104)
- OpenDML super-index dwDuration accessor + dmlh cross-check
- CBR-audio ix## standard-index block-alignment validator
- avih.dwPaddingGranularity + JUNK-aligned packet emission
- per-stream `strd` codec-driver data chunk (AVI 1.0)
- per-stream `strn` name chunk (AVI 1.0 §"AVI Stream Headers")
- WAVEFORMATEXTENSIBLE (wFormatTag 0xFFFE) demux + mux
- top-down DIB round-trip + BI_BITFIELDS color-mask exposure

### Added

- **`strh.rcFrame` destination-rectangle parse + emit + round-trip
  (round 115).** Surfaces the `rcFrame` field of the 56-byte
  AVISTREAMHEADER — the last documented `strh` field, sitting at byte
  offset 48 as four little-endian signed WORDs in
  `[left, top, right, bottom]` order. Clean-room source:
  `docs/container/riff/avi-riff-file-reference.md` §"AVISTREAMHEADER"
  (`rcFrame` row): *"Destination rectangle for a text or video stream
  within the movie rectangle specified by the dwWidth and dwHeight
  members of the AVI main header structure … typically used in support
  of multiple video streams … Units for this member are pixels. The
  upper-left corner of the destination rectangle is relative to the
  upper-left corner of the movie rectangle."* The muxer already wrote
  `rcFrame` (the default `0,0,width,height` for video, all-zero for
  non-video) but the demuxer dropped it, so a muxer-set rect was lost on
  re-demux; this round closes that round-trip. Demuxer side: `build_stream`
  now reads the four WORDs (when the strh is the full 56-byte form; the
  accepted short 48-byte form carries no `rcFrame`) and the new
  `AviDemuxer::stream_frame_rect(stream_index) -> Option<(i16, i16, i16,
  i16)>` accessor surfaces it (same value under the
  `avi:strh.<index>.frame_rect = "left,top,right,bottom"` metadata key).
  The all-zero "whole movie rectangle" writer default maps to `None` so a
  default / unspecified rect reads the same as an absent one (mirroring
  the round-80 `strn` / round-107 `IDIT` "empty == absent" convention);
  the metadata key is omitted entirely when absent so its non-presence is
  observable. Muxer side: `AviMuxOptions::with_stream_frame_rect(stream_index,
  left, top, right, bottom)` (last call per index wins) overrides the
  default rect for any stream type — letting a caller place a
  picture-in-picture second video stream or a subtitle overlay box at an
  arbitrary sub-rectangle inside the movie rectangle. The override is
  written verbatim, so a `0,0,0,0` override reads back as `None` on
  re-demux. Ten new tests (`tests/round115_rcframe.rs`) cover a custom
  video-rect round-trip (accessor + metadata key), the default
  `0,0,width,height` video rect surfacing while the audio stream's
  all-zero rect parses as `None`, an override on a non-video stream, an
  explicit all-zero override reading as `None`, negative coordinates
  surviving the i16 round-trip, builder dedup, a hand-rolled non-zero
  `rcFrame` decode, a hand-rolled all-zero `rcFrame` parsing as `None`, a
  hand-rolled short 48-byte strh (no `rcFrame` field) parsing as `None`,
  and an out-of-range stream index returning `None`.

- **`ISMP` SMPTE-timecode chunk parse + emit + round-trip (round
  112).** Implements the `ISMP` *Hdrl Tag* — the other direct child of
  `LIST hdrl` documented alongside `IDIT` in the RIFF *Hdrl Tags*
  namespace — that carries the file's first-frame SMPTE timecode as text.
  Clean-room source: `docs/container/riff/metadata/exiftool-riff-tags.html`
  §"RIFF Hdrl Tags" maps `'ISMP'` → `TimeCode`, listing it directly
  beside `'IDIT'` (`DateTimeOriginal`) and `LIST odml` as the recognised
  direct children of `hdrl`. Demuxer side: `parse_hdrl` now handles the
  `b"ISMP"` chunk and the new `AviDemuxer::smpte_timecode() ->
  Option<&str>` accessor surfaces it (same value under the `avi:ismp`
  metadata key). The staged docs do **not** pin a canonical on-disk text
  format — capture pipelines write the SMPTE non-drop-frame colon form
  "HH:MM:SS:FF", the drop-frame semicolon form "HH:MM:SS;FF", or a
  fractional "HH:MM:SS.ss" — so the parser is deliberately
  format-agnostic (mirroring the round-107 `IDIT` treatment): it strips
  trailing NUL / ASCII-whitespace bytes and decodes UTF-8-lossy,
  returning the timecode verbatim for the caller to interpret (no
  timecode parsing or normalisation). An empty / all-NUL / all-whitespace
  body yields `None` so a present-but-empty chunk reads the same as an
  absent one; the `avi:ismp` key is omitted entirely when absent so its
  non-presence is observable. Muxer side:
  `AviMuxOptions::with_smpte_timecode(tc)` (last call wins) emits an
  `ISMP` chunk inside `hdrl` after the strls / `LIST odml` / nested
  `LIST INFO` / any `IDIT` so existing strl offsets stay stable for
  `patch_post_counts`; the body is the caller's string + a NUL terminator
  (RIFF word-pad applied for odd lengths). The round-trip is byte-faithful
  regardless of the chosen text format, and `ISMP` + `IDIT` coexist
  independently in the same file. Eight new tests
  (`tests/round112_ismp.rs`) cover a SMPTE non-drop-frame round-trip
  (accessor + metadata key), a drop-frame round-trip
  (format-agnosticism), a no-ISMP baseline (accessor `None`, no
  `avi:ismp` key, smaller file), an empty-string body parsing as `None`,
  builder dedup, ISMP + IDIT coexistence, a hand-rolled fixture whose
  ISMP body carries a trailing newline + multi-NUL padding (peeled to the
  bare timecode), and a hand-rolled all-whitespace body parsing as `None`.

- **`IDIT` digitization-date chunk parse + emit + round-trip (round
  107).** Implements the `IDIT` *Hdrl Tag* — a direct child chunk of
  `LIST hdrl` (a sibling of `avih` / `strl` / `LIST odml` / `LIST
  INFO`) — that carries the capture / digitization timestamp as text.
  Clean-room source: `docs/container/riff/metadata/exiftool-riff-tags.html`
  §"RIFF Hdrl Tags" maps `'IDIT'` → `DateTimeOriginal`, listing it
  alongside `ISMP` (TimeCode) and `LIST odml` as the recognised direct
  children of `hdrl`. Demuxer side: `parse_hdrl` now handles the
  `b"IDIT"` chunk and the new `AviDemuxer::digitization_date() ->
  Option<&str>` accessor surfaces it (same value under the `avi:idit`
  metadata key). The staged docs do **not** pin a canonical on-disk text
  format — capture hardware commonly emits a C `asctime`-style "Wed Jan
  02 02:03:55 2002" (often with a trailing newline + NUL) while other
  tools use ISO-8601 — so the parser is deliberately format-agnostic:
  it strips trailing NUL / ASCII-whitespace bytes and decodes
  UTF-8-lossy, returning the timestamp verbatim for the caller to
  interpret (no date parsing or normalisation). An empty / all-NUL /
  all-whitespace body yields `None` so a present-but-empty chunk reads
  the same as an absent one (mirroring the round-80 `strn` convention);
  the `avi:idit` key is omitted entirely when absent so its non-presence
  is observable. Muxer side: `AviMuxOptions::with_digitization_date(date)`
  (last call wins) emits an `IDIT` chunk inside `hdrl` after the strls /
  `LIST odml` / nested `LIST INFO` so existing strl offsets stay stable
  for `patch_post_counts`; the body is the caller's string + a NUL
  terminator (RIFF word-pad applied for odd lengths). The round-trip is
  byte-faithful regardless of the chosen text format. Seven new tests
  (`tests/round107_idit.rs`) cover an asctime round-trip (accessor +
  metadata key), an ISO-8601 round-trip (format-agnosticism), a no-IDIT
  baseline (accessor `None`, no `avi:idit` key, smaller file), an
  empty-string body parsing as `None`, builder dedup, a hand-rolled
  fixture whose IDIT body carries a trailing newline + multi-NUL padding
  (peeled to the bare timestamp), and a hand-rolled all-whitespace body
  parsing as `None`.

- **Typed `vprp` active-frame-aspect-ratio accessor (round 104).**
  New `AviDemuxer::vprp_frame_aspect_ratio(stream_index) -> Option<(u16,
  u16)>` returns the OpenDML 2.0 §5.0 *"Active Frame Aspect Ratio"*
  (`dwFrameAspectRatio`) unpacked into a numeric `(x, y)` pair — the high
  WORD is the x term, the low WORD the y term, so the on-disk
  `0x0004_0003` decodes to `(4, 3)` and `0x0010_0009` to `(16, 9)`
  (clean-room source `docs/container/riff/opendml-avi-2.0.pdf`, §5.0
  "Source and Header Information Storage" → "Video Properties Header
  (vprp)" → "Active Frame Aspect Ratio": *"The aspect ratio is stored as
  a DWORD value with a word each storing the x:y ratio… This value can be
  used with the frame width and height to calculate the pixel aspect
  ratio"*). This is the typed companion to the existing
  `avi:vprp.<index>.frame_aspect_ratio` metadata key (which formats the
  same field as the human-readable string `"x:y"`); callers computing a
  pixel aspect ratio from the active frame dimensions now get the two
  WORDs directly instead of re-parsing the metadata string. Returns
  `None` when the stream carries no `vprp` chunk (presence gated on
  `nbFieldPerFrame > 0`, matching the metadata surface), when its
  `dwFrameAspectRatio` is `0` (writer left it unspecified — the metadata
  surface omits the key in that case too, so absence stays observable),
  or for an out-of-range stream index. The pair round-trips a
  muxer-emitted ratio set via `VprpConfig::with_aspect` /
  `VprpConfig::with_frame_aspect_ratio`. Joins the round-9
  `vprp_field_descs` typed accessor so both the per-field-rect tail and
  the scalar aspect ratio are reachable without walking metadata strings.
  Four new tests (`tests/round104_vprp_aspect.rs`) cover a custom 16:9
  round-trip (with typed-pair ↔ metadata-string agreement), the NTSC
  preset's 4:3 default, an AVI-1.0 no-`vprp` stream (and out-of-range
  index) returning `None`, and a byte-patched zero-`dwFrameAspectRatio`
  `vprp` returning `None` while the chunk is otherwise present.

- **OpenDML super-index `dwDuration` accessor + `dmlh` cross-check
  (round 101).** Surfaces the per-segment `_avisuperindex_entry.dwDuration`
  field — *"time span in stream ticks"* per OpenDML 2.0 §"AVI Super Index
  Chunk" (clean-room source `docs/container/riff/opendml-avi-2.0.pdf`) —
  which the demuxer parsed since round 9 but never exposed. New
  `AviDemuxer::super_index_segment_durations(stream_index) -> Vec<u32>`
  returns the values in segment order, and
  `AviDemuxer::super_index_duration_violations() -> Vec<SuperIndexDurationViolation>`
  cross-checks them: for a one-tick-per-frame video stream the per-segment
  durations partition the file's total frame count, so their sum must equal
  the §5.0 `dmlh.dwTotalFrames` extended-header value (the real frame total
  across every `RIFF AVIX` segment, vs. `avih.dwTotalFrames`'s primary-only
  count). One `SuperIndexDurationViolation { stream_index,
  super_index_duration_total, dmlh_total_frames }` is returned per video
  stream whose sum disagrees. The check fires only when both counts are
  independently recorded (a non-empty `indx` super-index for the stream
  **and** a `dmlh` for the file); audio/data streams (whose ticks need not
  be one-per-frame), super-index-less files, and `dmlh`-less files are
  skipped. Like the round-96 block-alignment validator it is purely
  informational and never affects `open()`. Two supporting fixes make the
  cross-check exact: (1) the muxer now writes the **indexed** stream's
  per-segment frame count into `dwDuration` instead of the all-stream
  packet total (which over-counted video+audio files); (2) `parse_indx`
  now retains the legitimate `qwOffset == 0` primary-segment entry within
  `nEntriesInUse` (the OpenDML "unused entry" sentinel only applies to
  slots *beyond* `nEntriesInUse`; the primary `RIFF AVI ` segment starts
  at file offset 0 so its recorded RIFF offset is 0). Seeking is
  unaffected — it resolves chunk locations via the in-`movi` `ix##` scan,
  never the super-index `qwOffset`. Four new tests
  (`tests/round101_super_index_duration.rs`) cover video+audio durations
  summing to `dmlh` (the multi-stream case the old all-stream count would
  have tripped), video-only, a byte-patched-`dmlh` mismatch flagged on the
  video stream, and the AVI-1.0 no-super-index/no-`dmlh` empty case.

- **CBR-audio `ix##` standard-index block-alignment validator (round 96).**
  Implements a reader-side cross-check from OpenDML 2.0 §3.0 ("AVI
  Standard Index Chunk", clean-room source
  `docs/container/riff/opendml-avi-2.0.pdf`): each
  `AVISTDINDEX_ENTRY.dwSize` is the byte length of the indexed data
  chunk, so for a constant-bit-rate audio stream (PCM / A-law / µ-law /
  IMA-ADPCM) every indexed chunk must hold a whole number of
  `WAVEFORMATEX.nBlockAlign` sample blocks (`dwSize % nBlockAlign == 0`).
  New `AviDemuxer::cbr_audio_block_alignment_violations() ->
  Vec<BlockAlignViolation>` walks every `ix##` standard index, correlates
  each to its stream via the `dwChunkId` ASCII digits, and returns one
  `BlockAlignViolation { stream_index, entry_index, dw_size, block_align }`
  per offending entry (the `entry_index` is the per-stream ordinal counted
  across every `ix##` chunk in file order). VBR streams, video / data
  streams, and CBR streams whose `nBlockAlign` is 0 or 1 are skipped;
  AVI 1.0 files with no `ix##` chunks return an empty Vec. The check is
  informational and never affects `open()` — it complements the coarse
  round-14 VBR/CBR `dwSampleSize` invariant (enforced at open time) with a
  finer, index-level companion callers invoke when they want to trust
  `ix##` offsets for sample-accurate audio seeking. `AudioStrhInfo` gains
  a `block_align` field carrying the parsed `nBlockAlign`. Four new tests
  (`tests/round96_block_align.rs`) cover aligned (no violation),
  misaligned (exactly one flagged entry with correct `stream_index` /
  `entry_index` / `dw_size` / `block_align`), VBR (never flagged), and
  AVI-1.0-no-`ix##` (empty) cases, driving real multi-segment OpenDML
  output through the muxer.

- **`avih.dwPaddingGranularity` + JUNK-aligned packet emission (round 92).**
  Implements AVI 1.0 §"AVIMAINHEADER" line 197: *"Alignment for data,
  in bytes. Pad the data to multiples of this value."* paired with
  §"Other Data Chunks" line 179: *"Data can be aligned in an AVI file
  by inserting 'JUNK' chunks as needed."* (clean-room source at
  `docs/container/riff/avi-riff-file-reference.md`). New muxer builder
  `AviMuxOptions::with_padding_granularity(n)` stamps the granularity
  into `avih.dwPaddingGranularity` and, before every packet chunk in
  `movi`, emits a `JUNK` chunk sized so the upcoming chunk's 8-byte
  header lands at a file-absolute offset divisible by `n`. The JUNK
  body is zero-filled; per spec readers ignore its content. `n` must
  be a power of two in `[2, 65536]` — other values reset the field to
  the legacy `None` / `dwPaddingGranularity = 0` behaviour. Typical
  values: 512 (filesystem sector), 2048 (CD-ROM sector), 4096 (modern
  filesystem page). Demuxer round-trip: new
  `AviDemuxer::padding_granularity() -> u32` accessor returns the
  parsed value; same data surfaces under the `avi:padding_granularity`
  metadata key (omitted entirely when the value is the legacy 0
  sentinel so absence is observable). The demuxer's existing
  embedded-`JUNK` walker (round 3 originals) handles the inserted
  chunks transparently; packets round-trip byte-equal through the
  padded layout regardless of granularity. Six new tests
  (`tests/round92_padding_granularity.rs`) cover avih round-trip,
  per-packet alignment promise at 16 / 64 / 512 / 2048 / 4096-byte
  granularities, payload byte-equality through the JUNK layout,
  the no-opt baseline (no JUNK chunks emitted, accessor returns 0,
  metadata key absent), builder validation (only powers of two in
  `[2, 65536]` take effect — other values fall back to None), and
  the legacy `dwPaddingGranularity = 0` "no key in metadata" path.

- **Per-stream `strd` codec-driver-data chunk demux + mux (round 89).**
  Implements the AVI 1.0 §"AVI Stream Headers" optional `strd` chunk
  per the Microsoft Learn AVI RIFF File Reference (clean-room source
  at `docs/container/riff/avi-riff-file-reference.md`): "If the
  stream-header data ('strd') chunk is present, it follows the
  stream format chunk. The format and content of this chunk are
  defined by the codec driver. Typically, drivers use this
  information for configuration. Applications that read and write
  AVI files do not need to interpret this information; they simple
  transfer it to and from the driver as a memory block." Demuxer:
  new `AviDemuxer::stream_header_data(stream_index) -> Option<&[u8]>`
  typed accessor returning the raw codec-driver bytes verbatim
  (zero interpretation per spec) + `avi:strd.<n>.len` metadata key
  reporting the body length only (the metadata Vec deliberately
  doesn't hexdump opaque driver bytes into a String value). Empty
  payload `strd` (`cb=0`) parses as `Some(&[])` so "no `strd` chunk
  at all" stays distinguishable from "explicit empty driver blob".
  Muxer: new `AviMuxOptions::with_stream_header_data(stream_index,
  bytes)` builder emitting one `strd` chunk per registered stream
  after `indx`/`vprp` and before `strn` in the strl LIST. The chunk
  body is the caller-supplied bytes verbatim, RIFF word-padded with
  one trailing zero byte when odd-length per RIFF §"data is always
  padded to nearest WORD boundary"; duplicate calls for the same
  `stream_index` keep only the last entry (consistent with the
  round-80 `with_stream_name` and round-75 `with_extensible_audio`
  dedup pattern). The no-strd file byte layout is identical to
  pre-round-89. Round-trips arbitrary 4 / 5 (odd-length) / 8 / 12 /
  16-byte blobs byte-for-byte across two streams; explicit
  empty-blob round-trip surfaces `Some(&[])` and `avi:strd.0.len=0`.
  Tests in `tests/round89_strd.rs`.

- **Per-stream `strn` name chunk demux + mux (round 80).** Implements
  the AVI 1.0 §"AVI Stream Headers" optional `strn` chunk per the
  Microsoft Learn AVI RIFF File Reference (clean-room source at
  `docs/container/riff/avi-riff-file-reference.md`): a
  null-terminated text string describing each stream, sitting beside
  `strh` / `strf` / `strd` inside the per-stream `strl` LIST. Demuxer:
  new `AviDemuxer::stream_name(stream_index) -> Option<&str>` typed
  accessor + `avi:strn.<n>` metadata key (UTF-8-lossy decode so
  legacy Latin-1 / CP1252 capture-tool names round-trip without
  failing the parse; multi-trailing-NUL bodies strip cleanly; an
  empty payload — `cb=0` or `cb=1` carrying just the NUL terminator —
  is treated as "no name" so absence stays distinguishable from an
  empty-string name). Muxer: new
  `AviMuxOptions::with_stream_name(stream_index, name)` builder that
  emits one `strn` chunk per registered stream after `indx`/`vprp` in
  the strl. The chunk body is the UTF-8 bytes of the name followed
  by a NUL terminator, RIFF-word-padded; duplicate calls for the
  same `stream_index` keep only the last entry (consistent with the
  round-75 `with_extensible_audio` dedup pattern). Round-trips
  ASCII, multi-byte UTF-8 (Japanese tested), and the no-name baseline
  byte-for-byte. Tests in `tests/round80_strn.rs`.

- **WAVEFORMATEXTENSIBLE (`wFormatTag = 0xFFFE`) demux + mux (round 75).**
  Implements the 22-byte `cbSize` extension that carries the
  `Samples.wValidBitsPerSample` union member, `dwChannelMask`
  speaker-assignment bitmap, and SubFormat GUID per Microsoft
  `mmreg.h` § "WAVEFORMATEXTENSIBLE" and the docs Microsoft Learn
  mirror at `docs/container/riff/waveformatextensible/`. New
  `stream_format::WaveFormatExtensible { wfx, valid_bits_per_sample,
  channel_mask, subformat }` struct + `parse_waveformatextensible` /
  `write_waveformatextensible` helpers; new `stream_format::Guid`
  newtype with `display()` (canonical
  `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` form) + `is_ksdataformat_base()`
  / `ksdataformat_tag()` for legacy `wFormatTag` recovery; new
  `WAVE_FORMAT_EXTENSIBLE = 0xFFFE` constant. Seven well-known
  `KSDATAFORMAT_SUBTYPE_*` GUIDs documented in the spec table land
  as public constants (`PCM` / `IEEE_FLOAT` / `DRM` / `ALAW` /
  `MULAW` / `ADPCM` / `MPEG`) with depth-aware codec-id resolution
  via `subformat_codec_hint(guid, bits)` — PCM SubFormat with 24
  valid bits resolves to `pcm_s24le` even when the WAVEFORMATEX
  container size is 32 (the canonical 24-in-32 carriage). Demuxer:
  new `AviDemuxer::stream_audio_strf(stream) -> Option<AudioStrfInfo>`
  + `stream_channel_mask` / `stream_valid_bits_per_sample` /
  `stream_subformat` convenience accessors, plus four metadata keys
  per extensible audio stream — `avi:auds.<n>.channel_mask`,
  `avi:auds.<n>.valid_bits_per_sample`, `avi:auds.<n>.subformat`,
  `avi:auds.<n>.subformat_wformat_tag`. Muxer: new
  `AviMuxOptions::with_extensible_audio(stream_index, channel_mask,
  valid_bps, subformat_guid)` builder; `params.tag =
  WaveFormat(0xFFFE)` without the helper is now rejected at
  `open_avi` with `Error::Invalid` (was previously emitting a broken
  18-byte WAVEFORMATEX). Mux→demux round-trips 5.1 PCM (24-in-32)
  and stereo IEEE_FLOAT byte-equal on every captured field.
- **Top-down DIB orientation round-trip (round 19 C1).** New public
  `BitmapInfoHeader.top_down: bool` field on the parsed `strf` body
  preserves the sign of the on-wire `biHeight` per VfW `wingdi.h`
  §"biHeight sign rules" (positive ⇒ bottom-up DIB origin
  lower-left; negative ⇒ top-down DIB origin upper-left). New
  helper `stream_format::write_bitmap_info_header_oriented(width,
  height, compression, bit_count, extradata, top_down)` stamps a
  negative `biHeight` for the top-down case so a parse → emit cycle
  preserves orientation byte-for-byte. The convenience
  `write_bitmap_info_header` wrapper keeps the old positive-only
  behaviour. Demuxer surfaces the side-info via the new
  `AviDemuxer::stream_top_down(stream) -> Option<bool>` accessor
  (`None` for non-video streams or streams whose `strf` was too
  short to parse a BMIH) and the metadata key
  `avi:vids.<n>.top_down = "true"` (only emitted when the flag is
  set so absence is observable). Muxer-side: new
  `AviMuxOptions::with_top_down_video(stream_index)` builder pushes
  a per-stream flag; the muxer honours it only for uncompressed RGB
  streams (`BI_RGB` all-zero FourCC) since the VfW spec REQUIRES
  positive `biHeight` for compressed FourCCs and YUV bitmaps are
  always top-down regardless of sign. Pairs with the
  `BitmapInfoHeader.top_down` parse-side flag for full
  `parse → mutate → emit` round-trips on top-down RGB streams (the
  capture-card / desktop-grabber convention).
- **`BI_BITFIELDS` color-mask exposure (round 19 C2).** New public
  `stream_format::BI_BITFIELDS = [3, 0, 0, 0]` constant and
  `stream_format::parse_bitfields_masks(&[u8]) -> Option<(u32, u32,
  u32)>` helper that reads the three little-endian DWORDs the
  spec requires immediately after the 40-byte BMIH whenever
  `biCompression == BI_BITFIELDS` per VfW `wingdi.h` §"Color
  tables (palettes)". Demuxer-side: when an uncompressed RGB
  stream declares `BI_BITFIELDS`, the parsed `(red_mask,
  green_mask, blue_mask)` triple is now surfaced via the new
  `AviDemuxer::stream_bitfields_masks(stream) -> Option<(u32, u32,
  u32)>` accessor and the metadata key
  `avi:vids.<n>.bitfields = "r=0x<R>,g=0x<G>,b=0x<B>"`. Returns
  `None` / no key for any other compression (FourCC bitstreams,
  `BI_RGB`, etc.), for non-video streams, or when extradata was
  shorter than 12 bytes. Common masks per VfW §"biCompression":
  `(0xF800, 0x07E0, 0x001F)` ⇒ 16-bpp RGB565; `(0x7C00, 0x03E0,
  0x001F)` ⇒ 16-bpp RGB555; `(0x00FF_0000, 0x0000_FF00,
  0x0000_00FF)` ⇒ 32-bpp BGRA. Closes a long-standing parse-side
  gap that silently discarded the per-pixel channel layout for
  16/32-bpp uncompressed RGB AVIs.
- **`VideoStrfInfo` typed side-info struct.** New
  `oxideav_avi::demuxer::VideoStrfInfo { top_down, bitfields_masks
  }` aggregates the round-19 C1 + C2 BMIH-derived facts the AVI
  spec exposes per video stream. Indexed parallel to
  [`AviDemuxer::streams`] internally; only the two typed
  accessors above need to be called for the public API.

## [0.0.6](https://github.com/OxideAV/oxideav-avi/compare/v0.0.5...v0.0.6) - 2026-05-09

### Other

- open_avi_strict + Idx1Flags-aware NO_TIME-skip seek + per-stream dwMaxBytesPerSec cap
- typed Idx1Flags AVIIF_* accessors + idx1↔ix## cross-validator
- idx1-from-ix synthesiser + wider WAVE_FORMAT_* constants
- audio-only dwMaxBytesPerSec fallback + xxtx typed iter + over-budget warning
- dwMaxBytesPerSec populator + audio sample_size validator + lazy palette-change iterator
- typed PaletteChange round-trip + dwSuggestedBufferSize populator + named AVIF_* builders
- side-band data accessors + avih.dwFlags builder + str-keyed all_info_for rows
- side-band data accessors + avih.dwFlags builder + str-keyed all_info_for
- top-level LIST INFO + std-index strict seek + xxtx/xxpc emit
- xxtx text-chunk skip + vprp field-desc override + AvihFlags accessor
- vprp per-field rect array + dmlh_total_frames + strict-keyframe seek
- O(1) idx1-flags cache + LIST INFO read accessor + xxpc skip metadata
- round-7 feature matrix updates (mid-movi ix## + LIST INFO mux)
- mid-movi ix## periodic flush + multi-value INFO parsing
- 2-field idx1 flag bits + LIST INFO emit + super-index capacity opt-in
- Drop cross-crate dev-deps; tests register synthetic codecs
- per-packet field2 accessor + idx1 2-field hint + VBR duration + super-index overflow signalling
- feature matrix entries for round-4 additions
- AVI_INDEX_2FIELD encoder + vprp populator + rec byte budget
- OpenDML 2.0 dmlh + vprp + 2-field index + LIST rec clusters
- OpenDML 2.0 ix## std-index emit + parse + seek
- best-effort parse for truncated-head AVI 1.0
- prefer params.tag; drop tag_for_codec usage
- route codec resolution through oxideav-core CodecResolver
- OpenDML 2.0 super-index emit + parse, MagicYUV FourCC family
- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-avi/pull/502))

### Added

- **Strict idx1↔ix## cross-validator (round 18 C3).** New
  `oxideav_avi::demuxer::open_avi_strict(read, codecs)` entry point:
  when both an `idx1` table (AVI 1.0 §3.4) and per-segment `ix##`
  standard indexes (OpenDML 2.0) are present and they disagree on a
  packet's `(file-offset, payload-size)`, fails fast with
  `Error::InvalidData` carrying `"idx1↔ix## offset divergence at
  seq=N on stream <s>: idx1=offset_<a>_size_<sa>
  ix##=offset_<b>_size_<sb>"` instead of the round-17 lenient
  `avi:idx1.<n>.divergent_offsets` metadata key. Existing
  `open_avi` (lenient) preserves the metadata-only behaviour;
  `open_avi_lenient` still skips the round-14 audio sample-size
  validator. Pairs with the canonical "OpenDML ix## is more
  reliable than idx1" handoff for callers wanting fail-fast
  (validate-then-ship pipelines, strict players refusing
  recovered captures whose stale idx1 disagrees with reality).
- **`Idx1Flags`-aware first-non-`AVIIF_NO_TIME` keyframe seek
  (round 18 C4).** New
  `AviDemuxer::seek_to_first_video_keyframe_after(stream, target)
  -> Result<KeyframeSeekResult>` walks idx1 entries where BOTH
  `is_keyframe` is set AND `is_no_time` is NOT set, picks the first
  one with `pts >= target_pts`, and seeks the input there.
  Returns the same [`KeyframeSeekResult`] shape as the existing
  round-9 C4 `seek_to_keyframe_strict` so callers get
  `(target_pts, landed_pts, gop_distance)` symmetric to the prior
  helper. Falls back to the LAST non-NO_TIME keyframe when
  nothing at-or-after target qualifies (caller asked past EOF).
  Closes a seek-correctness gap where `seek_to_keyframe_strict`
  could land the cursor on a `xxpc` palette-change /
  `xxtx` text-subtitle / custom NO_TIME-tagged side-band
  keyframe instead of a real video frame the decoder can decode
  standalone — per Microsoft `vfw.h`, `AVIIF_NO_TIME` (0x0100) is
  set on entries whose presentation is gated by the next "real"
  video chunk's PTS rather than carrying their own time.
- **Per-stream `dwMaxBytesPerSec` cap helper (round 18 C1).** New
  `AviMuxOptions::with_per_stream_max_bytes_per_sec(stream_index,
  bytes_per_sec)` builder + `AviMuxer::over_budget_streams() ->
  &[(u32, u64, u32)]` accessor. After
  [`oxideav_core::Muxer::write_trailer`] the muxer compares each
  registered stream's observed `total_bytes * 1_000_000 /
  duration_micros` against the configured per-track cap and
  surfaces every breach as `(stream_idx, observed_bps, cap_bps)`.
  Pair with `with_strict_per_stream_budget(true)` to promote the
  first breach into a hard `Error::InvalidData` from
  `write_trailer`. Closes the per-track-budget hole left by the
  round-14 file-wide `with_max_bytes_per_sec` builder: VBR streams
  with strict per-track playback budgets (an AC-3 stream that
  must stay under 384 kbit/s for a downstream hardware decoder; a
  Motion-JPEG video stream stamped with a per-track recording
  allowance) need to know which track exceeded its cap, not just
  that the file-wide sum is too large. Builder calls for the same
  `stream_index` replace the prior cap; `bytes_per_sec == 0`
  removes any prior cap for that stream. Skipped silently when
  the file's `duration_micros` is zero (no video stream / zero
  per-frame timing — the muxer can't compute a meaningful
  per-stream rate without it).
- **Typed `Idx1Flags` decode + public `AVIIF_*` constants (round 17 C3).**
  New public newtype `oxideav_avi::demuxer::Idx1Flags { is_list,
  is_keyframe, is_first_part, is_last_part, is_no_time, bits }` plus
  `compressor_bits()` accessor for the `AVIIF_COMPRESSOR` upper-half
  mask, paired with public `AVIIF_LIST` (`0x0001`),
  `AVIIF_KEYFRAME` (`0x0010`, promoted from private),
  `AVIIF_FIRSTPART` (`0x0020`), `AVIIF_LASTPART` (`0x0040`),
  `AVIIF_NO_TIME` (`0x0100`), and `AVIIF_COMPRESSOR` (`0x0FFF_0000`)
  constants per AVI 1.0 §3.4 + Microsoft `vfw.h`. New
  `AviDemuxer::idx1_typed_flags_for_packet(stream, seq) ->
  Option<Idx1Flags>` decodes one entry's `dwFlags` DWORD into the
  structured shape, mirrored on the existing
  `idx1_flags_for_packet` raw u32 accessor. Closes the previously-
  hidden flag-bit gap surfaced by round-12's keyframe-only seek
  exposure: callers needing palette-change / text-chunk timing
  semantics (`AVIIF_NO_TIME`), multi-part packet detection
  (`AVIIF_FIRSTPART` / `AVIIF_LASTPART`), or codec-private bits
  (`compressor_bits()`) no longer have to hand-mask the raw flags.
- **`idx1` ↔ `ix##` cross-validator (round 17 C4).** When a file
  carries both an `idx1` table (AVI 1.0 §3.4) and per-segment
  `ix##` standard indexes (OpenDML 2.0), the demuxer's `open()`
  now walks them in parallel and compares per-packet `(offset,
  size)`. On disagreement it surfaces
  `avi:idx1.<n>.divergent_offsets = "seq=<i>
  idx1=offset_<a>_size_<sa> ix##=offset_<b>_size_<sb>"` under the
  metadata map. Real-world capture-card files sometimes ship a
  stale `idx1` (recovered from a crash, rebuilt by a non-conformant
  tool, or copied from a different cut) that disagrees with the
  truth in `ix##`; per OpenDML 2.0 §"Index Locations" the `ix##`
  view is canonical (64-bit offsets, per-segment) so callers
  detecting the metadata key should prefer it. The comparison only
  spans the primary RIFF (`idx1`'s 32-bit offsets can't address an
  AVIX continuation), so multi-segment files compare just the
  primary's `ix##` slice; single-segment OpenDML files where the
  std-index scan didn't trigger silently no-op. Length mismatches
  (idx1 has more or fewer entries than the primary `ix##`) are
  themselves divergences and surface at the first beyond-shared-
  prefix slot.
- **`idx1`-from-`ix##` synthesiser (round 16 C1).** New
  `AviMuxOptions::synthesise_idx1_from_ix(true)` opt-in: when set on
  an `AviKind::OpenDml` mux, the primary segment's `idx1` body is
  rebuilt from each stream's `ix##` standard-index records (one
  16-B `idx1` entry per packet) instead of the muxer's running
  `IndexEntry` collection. Per AVI 1.0 + OpenDML 2.0 §"Index
  Locations": AVI 1.0-only readers (Windows Media Player on XP,
  ffplay's strict AVI 1.0 path) honour `idx1` alone — they don't
  walk OpenDML `ix##` super-indexes — so an OpenDML-muxed file
  without `idx1` can't be seeked by them. Closes the long-deferred
  AVI 1.0 / OpenDML reader-compat gap. AVIX continuation packets
  are NOT included (idx1 offsets are 32-bit and primary-only). The
  per-packet snapshot bookkeeping is paid only when the option is
  on; default `false` and `AviKind::Avi10` mode are byte-equal
  no-ops vs. round-15.
- **Wider WAVE_FORMAT_\* constants + VBR validator (round 16 C4).**
  New public constants in `oxideav_avi::demuxer`: `WAVE_FORMAT_AC3`
  (`0x2000`), `WAVE_FORMAT_DTS` (`0x2001`), `WAVE_FORMAT_WMA1`
  (`0x0160`), `WAVE_FORMAT_WMA2` (`0x0161`), `WAVE_FORMAT_WMA_PRO`
  (`0x0162`), `WAVE_FORMAT_WMA_LOSSLESS` (`0x0163`),
  `WAVE_FORMAT_OPUS` (`0x704F`), `WAVE_FORMAT_AAC_ADTS` (`0x1601`)
  per Microsoft `mmreg.h` + the Xiph Opus-in-AVI assignment. The
  round-14 C2 VBR/CBR validator's lookup table now classifies all
  eight tags as VBR (require `strh.dwSampleSize == 0`); a non-zero
  sample size is rejected at `open_avi` with `Error::InvalidData`
  naming the offending stream and tag. The lenient demuxer
  entry-point (`open_avi_lenient`) still skips the validator for
  re-mux / inspection of malformed files.
- **Audio-only `dwMaxBytesPerSec` fallback (round 15 C2).** Closes the
  round-14 "audio-only file surfaces 0" reporting gap. When no video
  stream is present the per-frame-timing path returns 0, so the
  populator now falls back to summing every audio track's
  WAVEFORMATEX `nAvgBytesPerSec` (strf body bytes 8..12, LE) per AVI
  1.0 §3.1. For PCM s16le stereo @ 48 kHz this lands the spec-blessed
  `48_000 × 4 = 192_000`. Returns 0 only when there are no audio
  tracks (or every audio track had a zero `avg_bytes_per_sec`). The
  `AviMuxOptions::with_max_bytes_per_sec(n)` override stays
  authoritative when set.
- **`text_chunk_typed_iter` + `TextChunk` typed round-trip (round 15
  C3).** Mirrors the round-14 `palette_change_typed_iter` pattern
  symmetrically for the `xxtx` text/subtitle chunk family. New
  `demuxer::TextChunk { codepage, language, dialect, body }` typed
  struct with `parse(&[u8]) -> Option<Self>` and `to_bytes() ->
  Vec<u8>` per Microsoft `vfw.h`'s 6-byte text-chunk header
  (`wCodePage` / `wLanguage` / `wDialect` + raw payload). Codepage
  `0` and `65001` decode/encode as UTF-8 (lossy on invalid
  sequences); any other code page uses a Latin-1 byte pass-through so
  a `parse → to_bytes` cycle on the same buffer is byte-exact. New
  `AviDemuxer::text_chunk_typed(stream) -> Vec<TextChunk>` (eager)
  and `text_chunk_typed_iter(stream) -> TextChunkTypedIter<'_>`
  (lazy, ExactSizeIterator) accessors. New
  `AviMuxer::with_text_chunk_typed(stream, &TextChunk)` writes the
  typed struct via the existing raw-bytes `write_text_chunk` path.
- **`avi:over_budget` warning metadata (round 15 C1).** Demuxer
  surfaces a new `avi:over_budget = "expected_max=N stamped=M"`
  metadata key when the file's stamped `avih.dwMaxBytesPerSec` is
  smaller than `sum(audio.avg_bytes_per_sec) +
  computed_video_bytes_per_sec` (the per-stream demand a capture-card
  player must allocate disk-read pacing for). Audio bytes-per-sec
  comes from each `auds` stream's parsed WAVEFORMATEX (preserved on
  `params.bit_rate / 8`); video bytes-per-sec from the sum of idx1
  entry sizes for `vids` streams divided by the file's
  `duration_micros`. Warning is skipped silently when the avih is
  absent, the stamp is 0 (writer didn't bother), there's no usable
  duration, or there's no idx1 (no video bitrate term) — so no false
  positives on minimal / corner-case files.
- **`avih.dwMaxBytesPerSec` populator (round 14 C1).**
  `AviMuxer::write_trailer` now patches `avih.dwMaxBytesPerSec` (body
  offset 4, file offset 36) with the file's approximate maximum data
  rate per AVI 1.0 §3.1, computed as
  `sum(per_track_total_bytes) * 1_000_000 / (total_video_frames *
  micro_sec_per_frame)`. Pre-round-14 the field was hard-coded to 0,
  forcing capture-card players to fall back to a worst-case heuristic
  when sizing their disk-read pacing budget. The populator surfaces 0
  for audio-only files (no usable per-frame timing) and for files with
  zero packets, so the pre-round-14 baseline is preserved on the empty
  case. New `AviMuxOptions::with_max_bytes_per_sec(n)` builder stamps
  an explicit value when the encoder already knows its target peak
  rate.
- **`strh.dwSampleSize` VBR/CBR validator at `open_avi` (round 14
  C2).** Per AVI 1.0 / WAVEFORMATEX, VBR codecs (MPEG / MP3 / AAC —
  `wFormatTag` 0x0050 / 0x0055 / 0x00FF) require `dwSampleSize == 0`;
  CBR codecs (PCM / G.711 a-law / G.711 µ-law / IMA-ADPCM —
  `wFormatTag` 0x0001 / 0x0006 / 0x0007 / 0x0011) require
  `dwSampleSize > 0`. A mismatch surfaces as `Error::InvalidData`
  naming the offending stream and tag instead of letting the file
  through to break downstream `strh.dwLength` derivations later. New
  public `WAVE_FORMAT_PCM` / `_ALAW` / `_MULAW` / `_DVI_ADPCM` /
  `_MPEG` / `_MPEGLAYER3` / `_AAC` constants per `mmreg.h`. Format
  tags outside both sets pass through unchecked. New
  `demuxer::open_avi_lenient(read, codecs)` skips the validator for
  callers re-muxing a malformed legacy file.
- **`palette_change_typed_iter` lazy iterator (round 14 C3).** New
  `AviDemuxer::palette_change_typed_iter(stream) ->
  PaletteChangeTypedIter<'_>` returns one `Result<PaletteChange>` per
  `next()` call, decoding the typed shape on demand instead of
  materialising the full Vec. Useful for palette-animated screen
  captures where each second of footage may carry hundreds of palette
  deltas — the eager `Vec` form clones every `Vec<PaletteEntry>` even
  when the consumer only needs to walk once. Implements
  `ExactSizeIterator` so callers can pre-allocate a sink without first
  counting; bodies that fail to parse surface `Some(Err(_))` and the
  iterator advances past them.
- **Typed `xxpc` palette-change round-trip (round 13 C1).** New
  `demuxer::PaletteChange { first_entry, num_entries, flags, entries }`
  + `demuxer::PaletteEntry { red, green, blue, flags }` typed structs
  with `parse(&[u8]) -> Option<Self>` and `to_bytes() -> Vec<u8>`
  helpers per AVI 1.0 / `vfw.h`'s `PALCHANGE` shape (BYTE bFirstEntry,
  BYTE bNumEntries, WORD wFlags, PALETTEENTRY entries[]). New
  `AviDemuxer::palette_change_typed(stream) -> Vec<PaletteChange>`
  decodes every `xxpc` body buffered by round 12 C1 (bodies that fail
  to parse are skipped rather than aborting). New
  `AviMuxer::with_palette_change_typed(stream, &PaletteChange)`
  serialises the typed struct via the existing
  `write_palette_change` raw-bytes path. Closes the typed round-trip
  pair so callers don't have to hand-pack BITMAPINFO palette deltas.
- **`avih.dwSuggestedBufferSize` populator (round 13 C2).**
  `AviMuxer::write_trailer` now patches
  `avih.dwSuggestedBufferSize` (body offset 28, file offset 60) with
  the largest packet body observed across every stream, rounded up
  to the next 4-byte boundary, per AVI 1.0 §3.1's read-ahead
  allocation hint. Pre-round-13 the field was hard-coded to 0;
  capture-card players that allocate a single read buffer up-front
  now get a real allocation hint instead of falling back to a
  worst-case heuristic. New
  `AviMuxOptions::with_suggested_buffer_size(n)` builder lets the
  caller stamp an explicit value (skipping the per-track walk) when
  the encoder's peak packet budget is already known. New
  `AviDemuxer::avih_suggested_buffer_size() -> u32` typed accessor
  pairs the read side; the existing `avi:suggested_buffer_size`
  metadata key still reports the same value.
- **Named per-bit `AVIF_*` muxer builders (round 13 C3).** Six new
  fluent builder methods on `AviMuxOptions` —
  `with_has_index(bool)`, `with_must_use_index(bool)`,
  `with_is_interleaved(bool)`, `with_trust_ck_type(bool)`,
  `with_was_capture_file(bool)`, `with_copyrighted(bool)` — toggle
  the corresponding `AVIF_*` bit in `avih.dwFlags` without
  requiring callers to import the bit constants. Passing `false`
  masks the bit out so a baseline-on flag like `AVIF_TRUSTCKTYPE`
  can be cleared. The starting baseline is the current override (or
  `DEFAULT_AVIH_FLAGS` when none was set), so the named setters
  compose with `with_avih_flags` / `with_avih_flag_bit`.
- **Side-band chunk data accessors (round 12 C1).** Closes the byte
  round-trip with round 11 C3's `write_palette_change` /
  `write_text_chunk` muxer write helpers. New
  `AviDemuxer::palette_change_data(stream) -> &[Vec<u8>]` and
  `AviDemuxer::text_chunk_data(stream) -> &[Vec<u8>]` accessors return
  every `xxpc` / `xxtx` chunk body for a given stream in file order.
  Bodies are populated eagerly from `idx1` at `open()` time when
  present (the AVI 1.0 default), so callers can inspect palette /
  caption metadata without paying for a full `next_packet` walk; for
  `idx1`-less (OpenDML-only) files the lazy `next_packet` path
  appends each chunk body as it sees it. Slice length matches the
  existing `palette_change_count` / `text_chunk_count` accessors.
- **`avih.dwFlags` builder (round 12 C2).**
  `AviMuxOptions::with_avih_flags(bits)` stamps a verbatim u32 into
  `avih.dwFlags`; `AviMuxOptions::with_avih_flag_bit(bit)` ORs a
  single `AVIF_*` bit on top of the muxer's `DEFAULT_AVIH_FLAGS`
  baseline (`AVIF_HASINDEX | AVIF_TRUSTCKTYPE`). Pairs with the round
  10 C3 `AviDemuxer::avih_flags()` typed accessor so a builder →
  writer → demuxer round-trip can preserve flag bits like
  `AVIF_ISINTERLEAVED` (0x0100), `AVIF_WASCAPTUREFILE`
  (0x0001_0000), `AVIF_COPYRIGHTED` (0x0002_0000), and
  `AVIF_MUSTUSEINDEX` (0x0020) that the legacy round-6 default
  omits. Public `DEFAULT_AVIH_FLAGS = 0x0000_0810` constant lets
  callers reference the baseline by name.
- **String-keyed `LIST INFO` accessor (round 12 C3).**
  `AviDemuxer::all_info_for(fourcc: &str) -> Vec<&str>` is a sibling
  of round 8 C2's `info_all_for([u8; 4])` that accepts the FourCC as
  a `&str` (e.g. `"INAM"`, `"IART"`, `"ICMT"`) instead of a byte
  literal. Returns every matching value in file order. Non-4-character
  keys return an empty Vec; valid 4-char keys delegate to
  `info_all_for` so behaviour stays consistent across both lookup
  shapes.
- **Top-level `LIST INFO` muxer write path (round 11 C1).**
  `AviMuxOptions::with_top_level_info(true)` now emits the metadata
  `LIST INFO` chunk as a sibling of `LIST hdrl` (between hdrl and
  movi inside the outer `RIFF AVI ` form) instead of nested inside
  hdrl. Both placements are spec-compliant per the AVI 1.0
  reference; the sibling layout matches the recommended placement
  in Microsoft's Multimedia File Reference and several modern
  authoring tools. The demuxer's existing `b"INFO" if is_primary`
  walker arm recognises both layouts so the metadata payload
  round-trips byte-equally regardless of which the muxer chose.
  Default `false` keeps the round 6 nested-in-hdrl byte layout for
  existing callers.
- **OpenDML-only strict-keyframe seek variant (round 11 C2).**
  `AviDemuxer::seek_to_keyframe_strict_via_std_index(stream, pts)`
  is a parallel of round 9 C4's `seek_to_keyframe_strict` that
  always walks the OpenDML 2.0 `ix##` standard-index collection,
  bypassing the AVI 1.0 `idx1` table even when one is present.
  Returns the same `KeyframeSeekResult` (target_pts / landed_pts /
  gop_distance) so callers can plan a decode-and-discard loop after
  the seek. Use this variant when working with OpenDML-only files
  (no `idx1` chunk) to get a compile-time guarantee the seek used
  the std-index path, or as a sanity check on muxer fidelity for
  dual-indexed files. Errors with `Unsupported` when no `ix##`
  chunks are present (e.g. an `AviKind::Avi10` envelope).
- **`xxtx` / `xxpc` muxer write helpers (round 11 C3).** Closes the
  muxer side of round 8 C3 (`xxpc` palette-change) and round 10 C1
  (`xxtx` text/subtitle) read paths. New `AviMuxer::write_text_chunk(stream, data)`
  and `AviMuxer::write_palette_change(stream, data)` methods emit
  `NN<suffix>` side-band chunks into the current `movi` LIST,
  honour active `LIST rec ` clustering and OpenDML segment-rolling,
  record an `idx1` entry (no `AVIIF_KEYFRAME` so the demuxer's
  suffix-scanner picks it up under the per-stream side-band
  counter), and stamp an `ix##` standard-index entry when in
  `AviKind::OpenDml` mode. Side-band chunks do NOT bump
  `strh.dwLength` for the parent stream — they live alongside the
  stream's regular packets without being counted as one of them.
  The demuxer's `palette_change_count` / `text_chunk_count`
  accessors close the round-trip.
- **`xxtx` text/subtitle chunk recognition (round 10 C1).** Mirror of
  round 8 C3 (`xxpc` palette-change handling) for the text-stream
  FourCC family per `mmsystem.h`'s `ckidAVITextSF`. `xxtx` chunks are
  skipped from the regular packet stream the same way `xxpc` chunks
  are; the demuxer counts them per stream via both the static idx1
  scan and the runtime `next_packet` walk and surfaces the count via
  `avi:text_chunk.<n>` metadata + the new typed
  `AviDemuxer::text_chunk_count(stream_index) -> u32` accessor. Same
  shape as `palette_change_count`, including zero-suppression of the
  metadata key when no `xxtx` chunks were seen.
- **`VprpConfig::with_field_descs([..])` muxer override (round 10 C2).**
  Round 9 C1 always synthesised the trailing `VIDEO_FIELD_DESC[]`
  records from frame dimensions + a hard-coded PAL-flavoured
  `half_height + 23` second-line. That's wrong for NTSC (line 285)
  and any other broadcast standard with non-PAL first-line
  conventions. Round 10 lets callers supply each field's eight DWORDs
  verbatim via the new `VprpFieldDescOverride` struct so a re-mux
  doesn't lie about the signal-domain offsets. The override only
  takes effect when the supplied `Vec` covers every active field
  (`>= nb_field_per_frame.max(1)`) — a shorter Vec falls through to
  the synthesised default so a partial override doesn't silently
  truncate the array. `VprpConfig` switches from `Copy` to `Clone` to
  carry the `Vec`.
- **`AvihFlags` typed accessor for `AVIMAINHEADER.dwFlags` (round 10 C3).**
  `AviDemuxer::avih_flags() -> AvihFlags` decodes each documented
  `AVIF_*` bit per Microsoft's `vfw.h` (`AVIF_HASINDEX` /
  `AVIF_MUSTUSEINDEX` / `AVIF_ISINTERLEAVED` / `AVIF_TRUSTCKTYPE` /
  `AVIF_WASCAPTUREFILE` / `AVIF_COPYRIGHTED`) into per-bit `bool`s,
  with the raw u32 retained on the struct so callers wanting to
  inspect undocumented vendor-extension bits don't lose information.
  Same source as the existing `avi:flags` hex-string metadata key but
  in typed form so callers can branch on individual bits without
  string parsing.
- **`vprp` per-field `VIDEO_FIELD_DESC[]` round-trip (round 9 C1).**
  Round 8 only surfaced the 9 fixed DWORDs of the OpenDML 2.0 §5.0
  Video Properties Header and dropped the trailing
  `VIDEO_FIELD_DESC FieldInfo[nbFieldPerFrame]` array (8 DWORDs per
  field). Round 9 reads + surfaces them via the
  `avi:vprp.<i>.field<j>.<key>` metadata namespace and the new typed
  `AviDemuxer::vprp_field_descs(stream_index) -> &[VprpFieldDesc]`
  accessor, with `VprpFieldDesc` exposing all 8 DWORDs (compressed
  bitmap dims + valid-rect dims + offsets + signal-domain x/y). The
  muxer is also fixed to emit one record per field instead of always
  writing a single full-frame placeholder (a 2-field PAL/NTSC stream
  was declaring `nbFieldPerFrame=2` but writing only one rect; round
  9 emits half-height records with alternating
  `VideoYValidStartLine` per field).
- **`AviDemuxer::dmlh_total_frames() -> Option<u64>` (round 9 C3).**
  Typed accessor for the OpenDML 2.0 §5.0 `dmlh.dwTotalFrames`
  value. Returns `Some(total)` when a `LIST odml dmlh` extended
  header was parsed (typical for OpenDML multi-segment files) and
  `None` for AVI 1.0. Mirrors the existing
  `avi:total_frames_all_segments` metadata key but in typed form
  so callers can do arithmetic against pts/duration without parsing
  string values out of `metadata()`.
- **`AviDemuxer::seek_to_keyframe_strict(stream, pts) -> KeyframeSeekResult`
  (round 9 C4).** Backward-walking strict keyframe seek. Returns a
  `KeyframeSeekResult` carrying `target_pts`, `landed_pts`, and
  `gop_distance = target_pts - landed_pts` (clamped to ≥ 0). The
  underlying landing logic is identical to `Demuxer::seek_to` (last
  keyframe at-or-before target; falls back to the first keyframe if
  the request precedes it), but the structured result lets callers
  detect mid-GOP requests, plan a decode-and-discard loop to reach
  the originally-requested pts, or fail the seek when the gap is
  larger than they're willing to walk.
- **`AviDemuxer::info_for(id)` / `info_all_for(id)` (round 8 C2).**
  Public `LIST INFO` round-trip read accessors keyed by 4-byte
  FourCC. `info_for(*b"INAM")` returns the first value the muxer
  wrote via `with_info` regardless of whether it landed under a
  canonical key (`"title"`) or the namespaced fallback
  (`"avi:info.<fourcc>"`); `info_all_for` returns every value in
  file order for FourCCs that occur multiple times (`LIST INFO` is
  a flat list, not a map). Closes the muxer→demuxer round-trip gap
  for the round-7 `with_info` builder so callers can verify INFO
  metadata without re-parsing `metadata()`.
- **`xxpc` palette-change recognition (round 8 C3).** The demuxer
  now explicitly counts VfW `NNpc` palette-change chunks per stream
  (per `aviriff.h`'s `cktypePALchange = "PC"`) instead of silently
  skipping them. Two paths feed the counter: a fast static scan of
  raw idx1 at `open()` time (covers AVI 1.0 files with idx1) and a
  runtime increment in `next_packet` for files lacking an index.
  New `AviDemuxer::palette_change_count(stream) -> u32` accessor +
  `avi:palette_change.<stream>` metadata key (omitted when zero,
  to keep the namespace tidy). Palette-change chunks are still
  excluded from the regular packet stream — they're not video data.

### Changed

- **`idx1_flags_for_packet` is now O(1).** The round-6 accessor
  previously walked the entire `idx_table` linearly per call,
  giving callers walking every packet O(N²) cost. `open()` now
  builds a per-stream `Vec<Vec<u32>>` lookup table once, indexed
  by `(stream_index, packet_seq)`. Behaviour identical to the
  prior implementation; only the access cost changes.

- **Mid-`movi` `ix##` index emit (round 7 C1).** New
  `AviMuxOptions::with_mid_movi_index(stream_index, packets_per_flush)`
  builder enables periodic inline standard-index flushes for the
  named stream while the `movi` LIST is still open. Per OpenDML 2.0
  §"Index Locations in RIFF File", inline `ix##` chunks (e.g. `02ix`
  for stream 2) are spec-blessed for streams whose consumers
  benefit from sub-segment random-access (timecode, sparse
  subtitles). When the stream's pending entry count hits the
  cadence the muxer flushes a single-stream `ix##` chunk inside
  `movi` (closing any open `LIST rec ` cluster first so the chunk
  lands at the body level). Entries flushed inline are removed from
  the per-track buffer so the segment-tail `flush_ix_chunks` only
  emits the residual tail. `packets_per_flush == 0` clears any
  prior cadence; only meaningful for `AviKind::OpenDml`. The
  demuxer's `scan_ix_in_movi` already walks `movi` segments for
  `ix##` chunks regardless of position, so inline indexes round-trip
  unchanged.
- **Multi-value INFO parsing — unknown FourCCs (round 7 C2).**
  `parse_info_list` now surfaces `LIST INFO` sub-chunks whose
  FourCC isn't in the well-known map under `avi:info.<fourcc>`
  rather than dropping them. Mirrors the `avi:tag_<hex>` fallback
  for unrecognised codec tags. Callers wanting full INFO fidelity
  (e.g. video editors round-tripping capture-card metadata) can now
  read every entry via `Demuxer::metadata()`. Duplicate FourCCs
  (spec-legal — `LIST INFO` is a flat list, not a map) surface as
  multiple ordered metadata entries with the same key.
- **2-field idx1 entry-flag emission (round 6 C1).** The muxer now
  stamps `AVIIF_FIRSTPART | AVIIF_LASTPART` (= 0x60 per vfw.h) on
  every idx1 entry for streams registered via
  `AviMuxOptions::with_field2_stream`, in addition to the existing
  `AVIIF_KEYFRAME` bit. Demuxer adds
  `AviDemuxer::idx1_flags_for_packet(stream, pkt_seq) -> Option<u32>`
  surfacing the raw flags per entry, plus an `avi:idx1.<n>.is_2field`
  hint derived from the bits alone so AVI-1.0-only readers (no
  `ix##` super-index) can detect 2-field carriage from idx1 alone.
  Public flag constants `AVIIF_KEYFRAME` / `AVIIF_FIRSTPART` /
  `AVIIF_LASTPART` re-exported from `muxer`.
- **`LIST INFO` muxer-side emit (round 6 C2).** New
  `AviMuxOptions::with_info(id: [u8; 4], value: impl Into<String>)`
  builder accumulates `(FourCC, value)` pairs (e.g. `*b"INAM"` ->
  title, `*b"IART"` -> artist, `*b"IPRD"` -> album, `*b"ICMT"` ->
  comment, `*b"ICRD"` -> date, `*b"ISFT"` -> encoder). On
  `write_header`, a `LIST INFO` chunk is emitted inside `hdrl`
  carrying NUL-terminated values per the AVI 1.0 spec. Demuxer's
  `parse_hdrl` recurses into the nested `LIST INFO` so both
  hdrl-nested and top-level placements round-trip via
  `Demuxer::metadata()` under the standard key names.
- **OpenDML super-index capacity opt-in (round 6 C3).** New
  `AviMuxOptions::with_super_index_capacity(n)` builder raises the
  reserved `indx` slot count past the default 256. Public
  constants `OPENDML_SUPER_INDEX_DEFAULT_CAPACITY` (256) and
  `OPENDML_SUPER_INDEX_MIN_CAPACITY` (16) document the bounds.
  Files muxed with a raised capacity round-trip through the
  unmodified demuxer (zero-padded tail entries are skipped on
  parse). Per-segment fidelity scales linearly: 1024 slots = 1 TiB
  at 1 GiB / segment.
- **2-field-aware per-packet accessor (round 5 C1).** New
  `AviDemuxer::field2_offset_for_packet(stream, pkt_id) -> Option<u32>`
  surfaces the per-packet `dwOffsetField2` directly, parallel to
  the comma-joined `avi:ix.<n>.field2_offsets` metadata key. New
  `demuxer::open_avi(...) -> AviDemuxer` concrete-type entry
  point so callers can hold the typed handle alongside the
  `Demuxer` trait it implements. Returns `None` for non-2-field
  streams, out-of-range `pkt_id`, or unknown `stream` indexes.
- **idx1 + 2-field correlation hint (round 5 C2).** When an
  `idx1` table is present alongside an `ix##` carrying
  `bIndexSubType == AVI_INDEX_2FIELD` for the same stream, the
  demuxer surfaces an `avi:idx1.<n>.is_2field = "true"` metadata
  key so consumers seeking via the legacy idx1 path know the
  entries describe interlaced frames. The AVI 1.0 `AVIINDEXENTRY`
  layout itself doesn't define field-2 columns; this hint makes
  the OpenDML interpretation visible at the idx1 layer too.
- **VBR audio framing via `Packet.duration` (round 5 C3).** Non-PCM
  audio (`strh.dwSampleSize == 0`) now accumulates each packet's
  `Packet.duration` (in stream ticks) into the running sample
  count so `strh.dwLength` reflects a real frame count rather
  than the round-3 "1 per packet" placeholder. PCM audio still
  uses the block-align-driven `size / sample_size` path; VBR
  streams that don't set `Packet.duration` keep the legacy
  fallback.
- **OpenDML super-index overflow signalling (round 5 C4).**
  Muxer-side: new
  `AviMuxer::truncated_super_index_segments() -> usize` returns the
  number of segments that overflowed the 256-slot
  `OPENDML_SUPER_INDEX_CAPACITY` reserve (silently truncated by
  `patch_super_index` until now). Demuxer-side: an
  `avi:indx.<stream>.overflow_entries = "<count>"` metadata key
  is emitted when a parsed `indx` super-index declares more
  entries than the conventional 256-slot soft cap, so downstream
  inspectors can flag files with degraded super-index fidelity.
- **2-field interlaced encoder (round 4 P1).** New
  `AviMuxOptions::with_field2_stream(idx)` plus the concrete-type
  entry point `open_avi(...) -> AviMuxer` so callers can invoke
  `AviMuxer::set_field2_offset(payload_off)` immediately before
  `write_packet`. The muxer then stamps `bIndexSubType =
  AVI_INDEX_2FIELD` on the `indx` super-index AND emits each
  `ix##` standard-index with `wLongsPerEntry = 3` and 12-byte
  entries `(dwOffset, dwSize, dwOffsetField2)` per OpenDML 2.0 §3.0
  "AVI Field Index Chunk" / "Super Index Chunk". Default-off; no
  output change for non-2-field callers.
- **`vprp` per-stream populator API (round 4 P2).** New
  `VprpConfig` struct + `AviMuxOptions::with_vprp(stream_idx,
  config)` builder. Presets `VprpConfig::ntsc()` / `pal()` /
  `secam()` fill in the well-known §5.0 token + 60/50 Hz refresh
  + interlaced framing + 4:3 aspect. Builders
  `with_aspect(x, y)` / `with_frame_aspect_ratio(packed)` /
  `with_nb_field_per_frame(n)` for individual overrides. Public
  constants `VIDEO_FORMAT_*` and `VIDEO_STANDARD_*` mirror the
  §5.0 enums. Zero override fields fall back to the round-3
  defaults so a partial override (e.g. just the standard token)
  doesn't lose the muxer's stream-derived refresh rate.
- **`dwOffsetField2` surfaced via `Demuxer::metadata()` (round 4
  P3).** The demuxer emits `avi:ix.<index>.is_2field = "true"` and
  `avi:ix.<index>.field2_offsets = "<comma-separated u32 list>"`
  for every stream whose `ix##` carries
  `bIndexSubType == AVI_INDEX_2FIELD`. Offsets are
  `qwBaseOffset`-relative — same byte-offset space as the
  std-index entries themselves. The `ix##` scan now also fires
  when the super-index alone declares `AVI_INDEX_2FIELD`, fixing
  a pre-existing single-segment-OpenDML scan-skip caused by the
  spec's "qwOffset = 0 is unused" convention dropping the
  primary-segment slot.
- **`LIST rec ` cluster threshold by byte budget (round 4 P4).**
  New `AviMuxOptions::with_rec_cluster_bytes(n)` (`n < 256`
  treated as no clustering). Cluster closes as soon as the next
  packet would push its body past `n` bytes. May be combined with
  `with_rec_cluster_packets(k)` — whichever cap fires first
  closes the cluster. Useful for VBR streams where a fixed
  packet count produces wildly varying cluster sizes.
- **OpenDML 2.0 `LIST odml dmlh` extended header (round 3 P1).** The
  muxer emits a `LIST odml` containing a `dmlh` chunk inside `hdrl`
  whenever `AviKind::OpenDml` is selected; its single `dwTotalFrames`
  DWORD is back-patched in `write_trailer` with the cross-segment
  total (per OpenDML 2.0 §5.0 "Required Information / Extended AVI
  Header"). The demuxer parses `LIST odml dmlh` when present and
  surfaces the value as `avi:total_frames_all_segments` metadata so
  callers can distinguish primary-segment-only `avih.dwTotalFrames`
  from the OpenDML truth on multi-segment files.
- **OpenDML 2.0 `vprp` Video Properties Header (round 3 P3).** The
  muxer emits a 68-byte `vprp` chunk (9 fixed DWORDs + 1 default
  `VIDEO_FIELD_DESC`) inside each video stream's `strl` for
  `AviKind::OpenDml` files. Defaults: `FORMAT_UNKNOWN`,
  `STANDARD_UNKNOWN`, refresh rate = fps, 4:3 aspect ratio,
  `nbFieldPerFrame = 1`. The demuxer parses `vprp` when present and
  surfaces every field under `avi:vprp.<index>.*` metadata keys
  (`video_format_token`, `video_standard`, `vertical_refresh_rate`,
  `frame_aspect_ratio` formatted as "X:Y", `frame_width_in_pixels`,
  `frame_height_in_lines`, `nb_field_per_frame`, …).
- **AVI_INDEX_2FIELD parse for interlaced `ix##` chunks (round 3
  P2).** The demuxer's `parse_ix_chunk` now branches on
  `bIndexSubType == AVI_INDEX_SUB_2FIELD` (per OpenDML 2.0 §3.0
  "AVI Field Index Chunk"): when set, entries are 12 bytes
  (`dwOffset`, `dwSize`, `dwOffsetField2`) with `wLongsPerEntry == 3`.
  The decoded `dwOffsetField2` is held on `StdIndexEntry` and the
  parent `StdIndex` carries the `b_index_sub_type` byte for callers
  that need to distinguish progressive from interlaced indexes. The
  muxer continues to emit single-field indexes (interlaced encoder
  support is a round-4 candidate).
- **Optional `LIST rec ` cluster grouping (round 3 P4).** New
  `AviMuxOptions::with_rec_cluster_packets(n)` plus
  `open_with_options` entry point: when set, the muxer groups every
  `n` consecutive `movi` packets into a `LIST rec ` cluster
  (per AVI RIFF §"Stream Data ('movi' List)" /
  OpenDML 2.0 spec/06). Default OFF — every existing caller gets
  the same byte output. Both the AVIX-segment closer and
  `write_trailer` close any open cluster before flushing `ix##` or
  `idx1` so the index chunks land at the tail of `movi`, not nested
  inside a cluster.
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
