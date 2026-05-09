# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
