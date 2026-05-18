# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
