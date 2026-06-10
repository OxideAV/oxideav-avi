# oxideav-avi

Pure-Rust **AVI (RIFF)** container — demuxer + muxer for the legacy
Microsoft AVI 1.0 format with a wide FourCC / WAVEFORMATEX mapping table
into stable oxideav codec ids. Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-container = "0.1"
oxideav-avi = "0.0"
```

## Feature matrix

| Feature                                          | Demux | Mux  |
|--------------------------------------------------|:-----:|:----:|
| RIFF/AVI 1.0 (hdrl/strl/movi/idx1)               | yes   | yes  |
| Multi-stream (video + audio + ...)               | yes   | yes  |
| `LIST INFO` metadata (title/artist/album/...)    | yes (known FourCCs + `avi:info.<fourcc>` for unknowns; nested-in-hdrl + sibling-of-hdrl placements; `info_for` / `info_all_for` byte-keyed + `all_info_for` string-keyed accessors) | yes (`AviMuxOptions::with_info` + `with_top_level_info` for sibling layout) |
| Duration from `avih` (microseconds-per-frame)    | yes   | n/a  |
| File-global `avih.dwMicroSecPerFrame` (AVI 1.0 §"AVIMAINHEADER") | yes (`micro_sec_per_frame() -> Option<u32>` raw-DWORD accessor returning the verbatim 32-bit value at byte offset 0 of the 56-byte AVIMAINHEADER body + `avi:micro_sec_per_frame = "<N>"` decimal metadata key; per AVI 1.0 §"AVIMAINHEADER" Appendix A line 195: *"Number of microseconds between frames. Indicates the overall timing for the file."*; the writer-skips-it `0` sentinel parses as `None` so a degenerate / unspecified value reads the same as an absent one, mirroring the round-249/247/229/222/217/210/203/182/176/153/119/115 "default == absent" convention; the demuxer already folds this DWORD into the internal `duration_micros = total_frames * micro_sec_per_frame` derivation surfaced via `Demuxer::duration` — this raw accessor keeps the on-disk byte pattern observable independent of any derived duration, and is the file-global complement of the per-stream `(dwScale, dwRate)` pair surfaced via `stream_timebase` (round-249); the two surfaces can disagree — a capture pipeline may stamp a non-standard frame period here, or leave it `0` even when the per-stream pair is populated — and the demuxer reports both verbatim so a downstream tool can detect or repair any mismatch) | yes (`AviMuxOptions::with_micro_sec_per_frame(n)` — last builder call wins; writes the supplied 32 bits verbatim at byte offset 0 of the AVIMAINHEADER body, replacing the muxer's default derivation from the first video stream's `(dwScale, dwRate)` pair (`1_000_000 * scale / rate`, or `0` for audio-only files); useful for audio-only fixtures that still want to advertise a nominal frame period to a downstream player, or fixtures exercising the demuxer's `micro_sec_per_frame` accessor on a non-standard period; the override is `avih`-only — does NOT touch the per-stream `(strh.dwScale, strh.dwRate)` pair (round-249), nor the muxer's internal duration / `dwMaxBytesPerSec` derivation, which both continue to source the frame period from the same first-video-stream packaging pair as before; stamping a value that disagrees with `1_000_000 * stream0_scale / stream0_rate` is internally inconsistent on purpose — the long-standing convention that file-global byte-stamp overrides are byte-stamp-only; an explicit `0` stamps the writer-skips-it sentinel — the demuxer maps that back to `None`) |
| File-global `avih.dwMaxBytesPerSec` (AVI 1.0 §"AVIMAINHEADER") | yes (`max_bytes_per_sec() -> Option<u32>` typed raw-DWORD accessor returning the verbatim 32-bit value at byte offset 4 of the 56-byte AVIMAINHEADER body + `avi:max_bytes_per_sec = "<N>"` decimal metadata key; per AVI 1.0 §"AVIMAINHEADER" Appendix A line 196: *"Approximate maximum data rate of the file. Number of bytes per second the system must handle to present an AVI sequence as specified by the other parameters in the main header and stream header chunks."*; the writer-skips-it `0` sentinel parses as `None` so a degenerate / unspecified rate reads the same as an absent one, mirroring the round-256/249/247/229/222/217/210/203/182/176/153/119/115 "default == absent" convention; round-260 closes the typed-accessor gap on top of the existing round-14 metadata-key + computed-default-or-override muxer wiring, so a downstream remuxer / capture-info dumper can reach `Option<u32>` without scanning the metadata Vec; the accessor and the metadata key agree on the on-disk byte pattern, and round-trip byte-equal with `AviMuxOptions::with_max_bytes_per_sec`) | yes (existing round-14 wiring: `AviMuxOptions::with_max_bytes_per_sec(n)` overrides the muxer's auto-computed `sum(per_track_total_bytes) / file_duration_seconds`; default with no override stamps the computed value verbatim; `0` stamps the writer-skips-it sentinel — the demuxer's round-260 typed accessor maps that back to `None`) |
| File-global `avih.dwTotalFrames` (AVI 1.0 §"AVIMAINHEADER") | yes (`avih_total_frames() -> Option<u32>` typed raw-DWORD accessor returning the verbatim 32-bit value at byte offset 16 of the 56-byte AVIMAINHEADER body + `avi:total_frames = "<N>"` decimal metadata key; per AVI 1.0 §"AVIMAINHEADER" Appendix A line 199: *"Total number of frames of data in the file."*; the writer-skips-it / empty-file `0` sentinel parses as `None` so a degenerate / unspecified count reads the same as an absent one, mirroring the round-260/256/249/247/229/222/217/210/203/182/176/153/119/115 "default == absent" convention; pre-round-268 the demuxer already consumed this DWORD internally to derive `duration_micros = total_frames * micro_sec_per_frame` (the source of `Demuxer::duration`) but never surfaced the raw value — round-268 closes both the typed-accessor and metadata-key gaps so the on-disk byte pattern stays observable independent of any derived duration; for a multi-segment OpenDML file this field only carries the **primary** `RIFF AVI ` segment's frame count (per OpenDML 2.0 §5.0) — the cross-segment truth is the spec-independent `dmlh.dwTotalFrames` surfaced separately via `dmlh_total_frames()` / `avi:total_frames_all_segments`, and the demuxer reports both verbatim so a downstream tool can detect or repair any mismatch) | yes (long-standing auto-derived stamp: `write_trailer` patches the first video stream's emitted packet count — first-track count for video-less files — into body offset 16 at file offset 48; no override builder, the file-global stamp tracks actual emitted packets; round-trips verbatim through the round-268 demuxer surface) |
| `avih.dwFlags` (`AVIF_*` bits)                   | yes (typed `avih_flags()` decode + raw `avi:flags` metadata) | yes (`AviMuxOptions::with_avih_flags` / `with_avih_flag_bit`) |
| `idx1` legacy index — parse for keyframe seek    | yes   | yes  |
| `idx1` offsets — file-absolute + movi-relative   | yes   | yes  |
| `idx1` entry `dwFlags` (`AVIIF_*` bits)          | yes (typed `idx1_typed_flags_for_packet` decode + raw `idx1_flags_for_packet` + public `AVIIF_LIST` / `AVIIF_KEYFRAME` / `AVIIF_FIRSTPART` / `AVIIF_LASTPART` / `AVIIF_NO_TIME` / `AVIIF_COMPRESSOR` constants) | yes (`AVIIF_KEYFRAME` on every keyframe; `AVIIF_FIRSTPART | AVIIF_LASTPART` stamp on 2-field interlaced) |
| `idx1` ↔ `ix##` cross-validator                  | yes (lenient: multi-segment OpenDML files surface `avi:idx1.<n>.divergent_offsets`; strict: `open_avi_strict` returns `Error::InvalidData`) | n/a |
| `LIST rec ` packet grouping inside `movi`        | yes   | yes (packet-cap or byte-budget) |
| OpenDML 2.0 multi-`RIFF AVIX` continuation       | yes   | yes  |
| OpenDML 2.0 `indx` super-index in `strl`         | yes (parse) | yes (emit) |
| OpenDML 2.0 `ix##` per-segment std-index in `movi` | yes (parse) | yes (segment-tail emit + opt-in mid-`movi` periodic flush via `AviMuxOptions::with_mid_movi_index`) |
| `idx1`-from-`ix##` synthesis (AVI 1.0 reader compat) | n/a | yes (`AviMuxOptions::synthesise_idx1_from_ix(true)` rebuilds primary segment's `idx1` from per-packet `ix##` records) |
| OpenDML 2.0 `LIST odml dmlh` extended header     | yes (parse + typed `dmlh_total_frames() -> Option<u64>` accessor + `avi:total_frames_all_segments` metadata key — the OpenDML 2.0 §5.0 "real total frame count across every `RIFF AVIX` segment" that `avih.dwTotalFrames` can't carry) | yes (auto-derived primary-video-stream `packet_count` at `write_trailer` patch site, folding every AVIX continuation packet via `TrackState::packet_count`; explicit `AviMuxOptions::with_dmlh_total_frames(n)` override stamps the supplied 32-bit value verbatim at the same patch site — for fixed-budget capture writers, edit-list trims, chained AVIX continuations, or fuzz / regression fixtures exercising the demuxer's `super_index_duration_violations` cross-check; override is dmlh-only — does NOT touch `avih.dwTotalFrames` or any `idx1` / `ix##` derivation, so a stamped mismatch surfaces through that demuxer accessor on re-demux; no-op in `AviKind::Avi10` mode) |
| OpenDML 2.0 `vprp` Video Properties Header       | yes (parse + `avi:vprp.<n>.*` metadata + typed `vprp_field_descs` per-field rects + typed `vprp_frame_aspect_ratio` returning the §5.0 `dwFrameAspectRatio` unpacked as a numeric `(x, y)` pair) | yes (NTSC/PAL/SECAM presets + custom aspect) |
| OpenDML 2.0 `AVI_INDEX_2FIELD` interlaced std-index | yes (parse + metadata surface + per-packet `field2_offset_for_packet` accessor) | yes (`open_avi` + `set_field2_offset`) |
| OpenDML 2.0 super-index overflow signalling      | yes (`avi:indx.<n>.overflow_entries`) | yes (`AviMuxer::truncated_super_index_segments()`) |
| OpenDML 2.0 super-index `bIndexSubType` (Appendix F / Appendix E §"Sub-types") | yes (`super_index_sub_type(stream) -> Option<u8>` raw-byte accessor + `super_index_is_2field(stream) -> bool` convenience + `avi:indx.<n>.sub_type_2field = "true"` metadata key — emitted only when the byte is `AVI_INDEX_SUB_2FIELD == 0x01`, the `0` default is omitted so absence stays observable; the accessor returns `None` for streams without an `indx` so "no super-index declared" stays distinguishable from "super-index sub-type 0") | yes (already stamped on stream 0's super-index by `AviMuxOptions::with_field2_stream(0)` since round-4 P1) |
| VBR audio framing via `Packet.duration`          | n/a   | yes (drives `strh.dwLength` for non-PCM) |
| OpenDML-driven seeking (`ix##` std-index)        | yes (no-idx1 fallback + explicit `seek_to_keyframe_strict_via_std_index` returning `KeyframeSeekResult`) | n/a |
| Idx1Flags-aware non-`AVIIF_NO_TIME` keyframe seek | yes (`seek_to_first_video_keyframe_after` skips palette/text/data side-band entries that carry `AVIIF_NO_TIME`) | n/a |
| Per-stream `dwMaxBytesPerSec` cap                | n/a   | yes (`AviMuxOptions::with_per_stream_max_bytes_per_sec` + `AviMuxer::over_budget_streams` + `with_strict_per_stream_budget` for hard `write_trailer` error) |
| Uncompressed `db` video chunks                   | yes   | yes  |
| Variable stream interleave                       | yes   | yes  |
| Palette-change (`xxpc`) chunks                   | skip + per-stream `palette_change_count(stream)` + `palette_change_data(stream)` body accessors + `avi:palette_change.<n>` metadata | yes (`AviMuxer::write_palette_change`) |
| Text/subtitle (`xxtx`) chunks                    | skip + per-stream `text_chunk_count(stream)` + `text_chunk_data(stream)` body accessors + `avi:text_chunk.<n>` metadata | yes (`AviMuxer::write_text_chunk`) |
| Truncated-head tolerance (capture-card crash dumps) | yes | n/a |
| Top-down DIB orientation (negative `biHeight`) | yes (`stream_top_down` accessor + `avi:vids.<n>.top_down` metadata; preserved across parse) | yes (`AviMuxOptions::with_top_down_video`; honoured only for `BI_RGB` per VfW §"biHeight sign rules") |
| `BI_BITFIELDS` color masks (16/32-bpp RGB) | yes (`stream_bitfields_masks` accessor + `avi:vids.<n>.bitfields = "r=...,g=...,b=..."` metadata) | n/a |
| `WAVE_FORMAT_EXTENSIBLE` (`0xFFFE`) — 22-byte `cbSize` extension | yes (`stream_audio_strf` / `stream_channel_mask` / `stream_valid_bits_per_sample` / `stream_subformat` accessors + `avi:auds.<n>.{channel_mask,valid_bits_per_sample,subformat,subformat_wformat_tag}` metadata; depth-aware codec-id resolution for the 7 documented `KSDATAFORMAT_SUBTYPE_*` GUIDs, incl. `pcm_s24le` for 24-in-32 container PCM; round-163 typed `dwChannelMask` surface: `stream_channel_mask_typed(stream) -> Option<ChannelMask>` + `stream_channel_layout(stream) -> Option<ChannelLayout>` named-layout recognition for the 7 docs-table standard layouts (Mono / Stereo / 2.1 / Quad / 5.1 (Microsoft back, mask `0x0000_003F`) / 5.1 (DVD-style side, mask `0x0000_060F`) / 7.1, mask `0x0000_063F`), `ChannelMask::iter_speakers` walking the 18 documented `SPEAKER_*` bits in lowest-set-bit-first PCM byte-stream channel order with `Speaker::abbrev()` docs-table labels, `ChannelMask::reserved_bits` isolating bits in the Microsoft `SPEAKER_RESERVED` gap (`0x0004_0000..=0x4000_0000`), plus `SPEAKER_ALL` (`0x8000_0000`) catch-all surfaced separately; emits `avi:auds.<n>.channel_speakers` (comma-joined abbreviations in PCM byte-stream order, e.g. `"FL,FR,FC,LFE,BL,BR"`) for any non-empty mask + `avi:auds.<n>.channel_layout` (named-layout label, e.g. `"stereo"` / `"5.1(back)"` / `"5.1(side)"` / `"7.1"`) only when the mask matches one of the 7 named layouts so absence of a key stays observable) | yes (`AviMuxOptions::with_extensible_audio(stream, channel_mask, valid_bps, subformat_guid)`) |
| Per-stream `strn` name chunk (AVI 1.0 §"AVI Stream Headers") | yes (`stream_name(stream_index)` accessor + `avi:strn.<n>` metadata; UTF-8-lossy decode; multi-trailing-NUL tolerated; empty-payload `strn` parses as `None` so absence stays distinguishable) | yes (`AviMuxOptions::with_stream_name(stream_index, name)` — last builder call per index wins; NUL terminator added) |
| `IDIT` digitization-date chunk (RIFF *Hdrl Tags* `DateTimeOriginal`) | yes (`digitization_date()` accessor + `avi:idit` metadata; direct child of `LIST hdrl`; trailing NUL/whitespace stripped, UTF-8-lossy; format-agnostic — `asctime` and ISO-8601 both round-trip verbatim; empty/all-whitespace body parses as `None` so absence stays observable) | yes (`AviMuxOptions::with_digitization_date(date)` — last builder call wins; NUL terminator added) |
| `ISMP` SMPTE-timecode chunk (RIFF *Hdrl Tags* `TimeCode`) | yes (`smpte_timecode()` accessor + `avi:ismp` metadata; direct child of `LIST hdrl`, sibling of `IDIT`; trailing NUL/whitespace stripped, UTF-8-lossy; format-agnostic — non-drop-frame `"HH:MM:SS:FF"`, drop-frame `"HH:MM:SS;FF"` and fractional forms all round-trip verbatim; empty/all-whitespace body parses as `None` so absence stays observable) | yes (`AviMuxOptions::with_smpte_timecode(tc)` — last builder call wins; NUL terminator added) |
| Per-stream `strd` codec-driver data chunk (AVI 1.0 §"AVI Stream Headers") | yes (`stream_header_data(stream_index)` accessor returning raw bytes verbatim + `avi:strd.<n>.len` metadata; empty-payload `strd` (`cb=0`) parses as `Some(&[])` so "no chunk" stays distinguishable from "empty driver blob") | yes (`AviMuxOptions::with_stream_header_data(stream_index, bytes)` — last builder call per index wins; RIFF word-pad applied to odd lengths) |
| Per-stream `strh.rcFrame` destination rectangle (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_frame_rect(stream_index) -> Option<(i16,i16,i16,i16)>` accessor returning `(left, top, right, bottom)` from byte offset 48 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.frame_rect = "l,t,r,b"` metadata; the all-zero "whole movie rectangle" default and the short 48-byte strh both parse as `None` so a default/unspecified rect stays observable) | yes (`AviMuxOptions::with_stream_frame_rect(stream_index, l, t, r, b)` — last builder call per index wins; overrides the default `0,0,width,height` for video / all-zero for non-video, for any stream type) |
| Per-stream `strh.wLanguage` LANGID (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_language(stream_index) -> Option<u16>` accessor returning the raw 16-bit value from byte offset 14 of the AVISTREAMHEADER + `avi:strh.<n>.language` metadata; the `0` "LANG_NEUTRAL / SUBLANG_NEUTRAL" writer default parses as `None` so an unspecified tag stays observable; the spec notes AVI does not normatively pin a registry, so the demuxer surfaces the raw u16 verbatim with no LANGID decoding) | yes (`AviMuxOptions::with_stream_language(stream_index, langid)` — last builder call per index wins; writes the supplied 16-bit value verbatim at byte offset 14, no registry validation; an explicit `0` is equivalent to omitting the override) |
| Per-stream `strh.dwInitialFrames` interleave skew (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_initial_frames(stream_index) -> Option<u32>` accessor returning the raw 32-bit value from byte offset 16 of the AVISTREAMHEADER + `avi:strh.<n>.initial_frames` metadata; the `0` "noninterleaved file" writer default — per AVIMAINHEADER §`dwInitialFrames`: *"Noninterleaved files should specify zero"* — parses as `None` so an unspecified skew stays observable; unit is the stream's own `dwRate`/`dwScale` tick and the demuxer surfaces the raw u32 verbatim with no rate-conversion) | yes (`AviMuxOptions::with_stream_initial_frames(stream_index, frames)` — last builder call per index wins; writes the supplied 32-bit value verbatim at byte offset 16, no validation against per-stream `dwLength`; an explicit `0` is equivalent to omitting the override) |
| File-global `avih.dwInitialFrames` interleave skew (AVI 1.0 §"AVIMAINHEADER") | yes (`initial_frames() -> Option<u32>` accessor returning the raw 32-bit value from byte offset 16 of the 56-byte AVIMAINHEADER body, i.e. byte 24 of the `avih` chunk + `avi:initial_frames` metadata; the `0` "noninterleaved file" writer default — per AVI 1.0 §"AVIMAINHEADER" line 200: *"Noninterleaved files should specify zero. If creating interleaved files, specify the number of frames in the file prior to the initial frame of the AVI sequence."* — parses as `None` so an unspecified skew stays observable; file-global counterpart of the per-stream `strh.dwInitialFrames` DWORD — the two are spec-independent and round-trip without bleeding into each other) | yes (`AviMuxOptions::with_initial_frames(n)` — last builder call wins; writes the supplied 32-bit value verbatim at body offset 16, no rate-conversion and no validation against any per-stream `dwLength`; an explicit `0` is equivalent to omitting the override) |
| Per-stream `strh.dwQuality` quality indicator (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_quality(stream_index) -> Option<u32>` accessor returning the raw 32-bit value from byte offset 40 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.quality` metadata; the documented `-1` (`0xFFFF_FFFF` u32) "use default driver quality" sentinel — per AVI 1.0 §"AVISTREAMHEADER" line 246: *"Indicator of the quality of the data in the stream. Quality is represented as a number between 0 and 10,000. ... If set to -1, drivers use the default quality value."* — parses as `None` so an unspecified quality reads the same as an absent one, mirroring the round-153/119/115 "default == absent" convention; values in the documented `[0, 10_000]` range surface verbatim and `0` is *not* treated as default; out-of-range writers round-trip exactly — the demuxer does not clamp or normalise) | yes (`AviMuxOptions::with_stream_quality(stream_index, q)` — last builder call per index wins; writes the supplied 32-bit value verbatim at byte offset 40, no clamp to the documented `[0, 10_000]` range; an explicit `0xFFFF_FFFF` is equivalent to omitting the override) |
| Per-stream `strh.wPriority` selection-hint DWORD (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_priority(stream_index) -> Option<u16>` accessor returning the raw 16-bit value from byte offset 12 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.priority` metadata; the `0` legacy writer default — per AVI 1.0 §"AVISTREAMHEADER" Appendix B line 238: *"Priority of a stream type. For example, in a file with multiple audio streams, the one with the highest priority might be the default stream."* — parses as `None` so an unspecified priority reads the same as an absent one, mirroring the round-176/153/119/115 "default == absent" convention; the spec describes a per-`fccType` selection hint, not a sortable global priority, so the demuxer surfaces the raw u16 verbatim and pins no value range or tie-break rule) | yes (`AviMuxOptions::with_stream_priority(stream_index, priority)` — last builder call per index wins; writes the supplied 16-bit value verbatim at byte offset 12, no validation; an explicit `0` is equivalent to omitting the override) |
| Per-stream `strh.dwStart` starting-time DWORD (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_start(stream_index) -> Option<u32>` accessor returning the raw 32-bit value from byte offset 28 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.start` metadata; the `0` legacy writer default — per AVI 1.0 §"AVISTREAMHEADER" `dwStart` row line 243: *"Starting time for this stream. The units are defined by the dwRate and dwScale members in the main file header. Usually, this is zero, but it can specify a delay time for a stream that does not start concurrently with the file."* — parses as `None` so an unspecified start reads the same as an absent one, mirroring the round-182/176/153/119/115 "default == absent" convention; the unit is the stream's own `(dwRate / dwScale)` tick and the demuxer surfaces the raw u32 verbatim with no rate-conversion) | yes (`AviMuxOptions::with_stream_start(stream_index, start)` — last builder call per index wins; writes the supplied 32-bit value verbatim at byte offset 28, no validation against the per-stream `dwLength`; an explicit `0` is equivalent to omitting the override) |
| Per-stream `strh.fccType` stream-type FourCC (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_fcc_type(stream_index) -> Option<[u8; 4]>` accessor returning the raw 4 bytes from byte offset 0 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.fcc_type` metadata; the all-zero `\0\0\0\0` writer-skips-it sentinel — per AVI 1.0 §"AVISTREAMHEADER" Appendix B `fccType` row line 235 + the `fcc` row line 234: *"A FOURCC code that specifies the type of data contained in the stream. The following standard AVI values are defined: `auds` (audio stream), `mids` (MIDI stream), `txts` (text stream), `vids` (video stream)."* — parses as `None` so an unspecified type reads the same as an absent one, mirroring the round-249/247/229/222/217/210/203/182/176/153/119/115 "default == absent" convention; metadata-string form renders as the printable four-character ASCII when every byte is in the `0x20..=0x7e` range (so `vids` / `auds` / `mids` / `txts` round-trip legibly) and as a lower-case `0xHHHHHHHH` hex form otherwise; non-standard FOURCCs outside the spec's documented `{auds, mids, txts, vids}` set (e.g. the legacy `iavs` interleaved DV stream FOURCC) surface verbatim — the spec phrases the standard values as illustrative rather than exhaustive, and the demuxer does NOT validate membership in the standard set; the field is the on-disk source of truth for the demuxer's media-kind routing inside `build_stream`, but this raw-FOURCC surface keeps the bytes observable verbatim for round-trip parity, independent of the typed `StreamInfo::params.media_type` exposed via `Demuxer::streams`) | yes (`AviMuxOptions::with_stream_fcc_type(stream_index, fcc_type)` — last builder call per index wins; writes the supplied 4 bytes verbatim at byte offset 0 of the strh, replacing the packaging-derived default (`vids` for video streams, `auds` for audio streams, per `packaging::StrfEntry::strh_type`); the override does NOT alter the muxer's media-kind routing (which is driven by the framework's `StreamInfo::params.media_type`, not the on-disk strh `fccType`), does NOT touch any sibling strh DWORD, and is NOT cross-validated against the encoder's chosen media kind, so stamping `txts` on a stream that's actually carrying PCM audio is internally inconsistent on purpose; an explicit `[0, 0, 0, 0]` stamps the all-zero sentinel — the demuxer maps that back to `None`) |
| Per-stream `strh.fccHandler` driver-handler FourCC (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_handler(stream_index) -> Option<[u8; 4]>` accessor returning the raw 4 bytes from byte offset 4 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.handler` metadata; the all-zero `\0\0\0\0` writer default — per AVI 1.0 §"AVISTREAMHEADER" Appendix B `fccHandler` row line 236: *"An optional FOURCC that identifies a specific data handler. The data handler is the preferred handler for the stream. For audio and video streams, this specifies the codec for decoding the stream."* — parses as `None` so an unspecified hint reads the same as an absent one, mirroring the round-203/182/176/153/119/115 "default == absent" convention; metadata-string form renders as the printable four-character ASCII when every byte is in the `0x20..=0x7e` range and as a lower-case `0xHHHHHHHH` hex form otherwise; the field is logically distinct from `BITMAPINFOHEADER.biCompression` (video) and `WAVEFORMATEX.wFormatTag` (audio) — writers typically mirror `biCompression` into `fccHandler` on video streams but the spec does not require the two to match, and audio writers almost always leave it zero) | yes (`AviMuxOptions::with_stream_handler(stream_index, fourcc)` — last builder call per index wins; writes the supplied 4 bytes verbatim at byte offset 4, no printability validation; packaging defaults remain — video streams mirror `BITMAPINFOHEADER.biCompression`, audio streams default to all-zero — so an explicit `[0, 0, 0, 0]` is equivalent to omitting the override for audio but explicitly clears the `biCompression`-mirror default on video) |
| Per-stream `strh.dwSuggestedBufferSize` read-ahead hint (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_suggested_buffer_size(stream_index) -> Option<u32>` accessor returning the raw 32-bit value from byte offset 36 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.suggested_buffer_size` metadata; the spec-documented `0` "do not know the correct buffer size" sentinel — per AVI 1.0 §"AVISTREAMHEADER" `dwSuggestedBufferSize` row line 245: *"How large a buffer should be used to read this stream. Typically, this contains a value corresponding to the largest chunk present in the stream. Using the correct buffer size makes playback more efficient. Use zero if you do not know the correct buffer size."* — parses as `None` so an unspecified hint reads the same as an absent one, mirroring the round-210/203/182/176/153/119/115 "default == absent" convention; the per-stream strh value is spec-independent from the file-global `avih.dwSuggestedBufferSize` already surfaced via `avih_suggested_buffer_size()` — the avih flavour is meant to cover the largest chunk across every stream, the strh flavour is a per-stream upper bound; the demuxer surfaces the raw u32 verbatim with no validation against the actual largest chunk seen in `movi` since over-declaration is the documented intent of the field) | yes (`AviMuxOptions::with_stream_suggested_buffer_size(stream_index, n)` — last builder call per index wins; writes the supplied 32-bit value verbatim at byte offset 36, no validation against the actual largest chunk observed in `movi`; without an override the muxer keeps its long-standing auto-derived default of `t.max_chunk_size` patched into the strh at the end of `write_trailer`; an explicit `0` stamps the spec-documented "do not know" sentinel — the demuxer maps that back to `None`) |
| Per-stream `strh.dwSampleSize` sample-size indicator (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_sample_size(stream_index) -> Option<u32>` accessor returning the raw 32-bit value from byte offset 44 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.sample_size` metadata; the spec-documented `0` "samples can vary in size" sentinel — per AVI 1.0 §"AVISTREAMHEADER" `dwSampleSize` row line 247: *"The size of a single sample of data. This is set to zero if the samples can vary in size. If this number is nonzero, then multiple samples of data can be grouped into a single chunk within the file. … For video streams, this number is typically zero, although it can be nonzero if all video frames are the same size. For audio streams, this number should be the same as the nBlockAlign member of the WAVEFORMATEX structure describing the audio."* — parses as `None` so an unspecified hint reads the same as an absent one, mirroring the round-217/210/203/182/176/153/119/115 "default == absent" convention; for audio streams the field doubles as the VBR / CBR switch the round-14 C2 invariant validates (PCM / G.711 / IMA-ADPCM require `nBlockAlign`, MP3 / AAC / MPEG require `0`); the demuxer surfaces the raw u32 verbatim with no validation against `WAVEFORMATEX.nBlockAlign` for audio nor against any observed chunk-size pattern in `movi`) | yes (`AviMuxOptions::with_stream_sample_size(stream_index, n)` — last builder call per index wins; writes the supplied 32-bit value verbatim at byte offset 44; without an override the packaging-derived default (audio PCM / CBR: `nBlockAlign`, audio VBR: `0`, video: `0`) is preserved; the override only changes the byte stamp and does NOT alter the muxer's own `dwLength` derivation, so a caller that stamps a `dwSampleSize` incompatible with their packet stream is creating an internally-inconsistent file on purpose and will need `open_avi_lenient` to read it back) |
| Per-stream `strh.dwLength` stream length (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_length(stream_index) -> Option<u32>` accessor returning the raw 32-bit value from byte offset 32 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.length` metadata key; the `0` "no length declared" value — per AVI 1.0 §"AVISTREAMHEADER" `dwLength` row line 244: *"Length of this stream. The units are defined by the dwRate and dwScale members of the stream's header."* — parses as `None` so an empty / unspecified stream reads the same as an absent one, mirroring the round-222/217/210/203/182/176/153/119/115 "default == absent" convention; the unit is the stream's own `(dwRate / dwScale)` tick — frames for video, samples-or-blocks for audio — and the demuxer surfaces the raw u32 verbatim with no rate-conversion; logically distinct from the `StreamInfo::duration` already exposed by `Demuxer::streams` — both are derived from this same DWORD but the framework's duration is typed as `Option<i64>` while the raw-u32 surface keeps the value observable verbatim for callers that need byte-exact round-trip semantics or comparison against a separately-emitted writer's stamp; the two values agree whenever the strh stamp fits in `i64`) | yes (`AviMuxOptions::with_stream_length(stream_index, n)` — last builder call per index wins; writes the supplied 32-bit value verbatim at byte offset 32 at the `write_trailer` / `patch_post_counts` site, replacing the long-standing auto-derived per-stream packet / sample count (video: `packet_count`, audio PCM / CBR: running `sample_count` from the muxer's `size / sample_size` formula); the override does NOT touch `avih.dwTotalFrames` (per-stream length and the file-global total are spec-independent fields) and does NOT alter any downstream `idx1` / `ix##` / `dmlh` derivation, so a caller that stamps a `dwLength` incompatible with their actual chunk count is creating an internally-inconsistent file on purpose; an explicit `0` stamps the de-facto "no length declared" value — the demuxer maps that back to `None`) |
| Per-stream `strh.dwFlags` (`AVISF_*` bits) (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_flags(stream_index) -> Option<u32>` raw accessor returning the 32-bit value from byte offset 8 of the 56-byte AVISTREAMHEADER + `stream_flags_typed(stream_index) -> Option<StrhFlags>` typed-decode accessor exposing the two documented `AVISF_*` bits as named `bool` fields (`disabled` for `AVISF_DISABLED` `0x0000_0001`: *"Indicates this stream should not be enabled by default."*; `video_palchanges` for `AVISF_VIDEO_PALCHANGES` `0x0001_0000`: *"Indicates this video stream contains palette changes. This flag warns the playback software that it will need to animate the palette."*; raw DWORD preserved in `StrhFlags::bits` so undocumented vendor / driver bits stay observable) + public `AVISF_DISABLED` / `AVISF_VIDEO_PALCHANGES` constants + `avi:strh.<n>.flags = "0xXXXXXXXX"` upper-case-hex metadata key; the `0` "no flags set" legacy writer default parses as `None` so an unspecified flag DWORD reads the same as an absent one, mirroring the round-229/222/217/210/203/182/176/153/119/115 "default == absent" convention; the demuxer does NOT mask undocumented bits — some legacy capture filters pack driver-private flags in the upper half-DWORD outside the spec's two `AVISF_*` constants, and those round-trip verbatim) | yes (`AviMuxOptions::with_stream_flags(stream_index, flags)` — last builder call per index wins; writes the supplied 32-bit value verbatim at byte offset 8 of the strh, replacing the pre-round-247 muxer default of `0` ("no flags set"); the muxer does NOT validate against the spec's two documented bits, does NOT touch `avih.dwFlags` (the file-global `AVIF_*` flags handled independently via `with_avih_flags` / `with_avih_flag_bit`), and does NOT cross-validate against other strh fields, so stamping `AVISF_VIDEO_PALCHANGES` on an audio stream is internally inconsistent on purpose; an explicit `0` stamps the legacy "no flags set" value — the demuxer maps that back to `None`) |
| Per-stream `(strh.dwScale, strh.dwRate)` timebase pair (AVI 1.0 §"AVISTREAMHEADER") | yes (`stream_timebase(stream_index) -> Option<(u32, u32)>` raw-DWORD accessor returning the paired 32-bit values from byte offsets 20 + 24 of the 56-byte AVISTREAMHEADER + `avi:strh.<n>.scale = "<N>"` and `avi:strh.<n>.rate = "<N>"` decimal metadata keys; per AVI 1.0 §"AVISTREAMHEADER" `dwScale` row line 241 + `dwRate` row line 242: *"Used with dwRate to specify the time scale that this stream will use. Dividing dwRate by dwScale gives the number of samples per second. For video streams, this is the frame rate. For audio streams, this rate corresponds to the time needed to play nBlockAlign bytes of audio, which for PCM audio is the just the sample rate."*; either DWORD being zero — the writer-skips-it / mathematically-undefined `rate/scale` ratio — parses as `None` so a degenerate / unspecified pair stays observable, mirroring the round-247/229/222/217/210/203/182/176/153/119/115 "default == absent" convention; the demuxer surfaces the raw u32 pair verbatim while the internal `StreamInfo::time_base` derivation still applies a `.max(1)` clamp to each DWORD for decode purposes, so the framework-level `Rational` time base and the raw-DWORD surface agree whenever both members are non-zero — the universal case in legitimate AVIs — and the `.max(1)` clamp covers truncated / zero-padded / hand-crafted edge cases) | yes (`AviMuxOptions::with_stream_timebase(stream_index, scale, rate)` — last builder call per index wins; writes the supplied pair verbatim at byte offsets 20 + 24 of the strh, replacing the packaging-derived default (video: per-stream `frame_rate.den / frame_rate.num`, audio: `1 / sample_rate`); the override does NOT alter the muxer's `(scale, rate)`-derived `dwLength` computation for audio streams (which still uses the packaging-derived `t.entry.{scale,rate}` to convert running samples into `dwLength` units), does NOT touch `avih.dwMicroSecPerFrame` (the file-global frame-rate hint, which the muxer derives independently from the first video stream's packaging pair), and does NOT cross-validate against the per-stream `dwLength` or `dwStart`, so stamping an audio sample-rate pair on a video stream is internally inconsistent on purpose; a `0` in either DWORD stamps the writer-skips-it sentinel — the demuxer maps that back to `None`) |
| `avih.dwPaddingGranularity` + `JUNK`-aligned packet emission (AVI 1.0 §"AVIMAINHEADER" / §"Other Data Chunks") | yes (`padding_granularity()` accessor + `avi:padding_granularity` metadata key — both omit the legacy 0 sentinel so absence is observable) | yes (`AviMuxOptions::with_padding_granularity(n)`; powers of two in `[2, 65536]`; inserts one `JUNK` chunk per misaligned packet so every packet header lands at a file-absolute offset divisible by `n`) |
| CBR-audio `ix##` block-alignment validator (OpenDML 2.0 §3.0 "AVI Standard Index Chunk") | yes (`cbr_audio_block_alignment_violations()` returns one `BlockAlignViolation { stream_index, entry_index, dw_size, block_align }` per std-index entry whose `dwSize` is not a multiple of the stream's `WAVEFORMATEX.nBlockAlign`; scoped to PCM / A-law / µ-law / IMA-ADPCM streams with `nBlockAlign > 1`; informational, never fails `open()`) | n/a |
| OpenDML 2.0 super-index `dwDuration` round-trip + cross-check (OpenDML 2.0 §"AVI Super Index Chunk" / §5.0 "Extended AVI Header") | yes (`super_index_segment_durations(stream)` lists the per-segment `_avisuperindex_entry.dwDuration` values; `super_index_duration_violations()` returns one `SuperIndexDurationViolation { stream_index, super_index_duration_total, dmlh_total_frames }` per video stream whose duration sum disagrees with `dmlh.dwTotalFrames`; informational, never fails `open()`; `parse_indx` keeps the legitimate primary-segment `qwOffset == 0` entry) | yes (`dwDuration` carries the **indexed** stream's per-segment frame count, not the all-stream packet total) |

OpenDML 2.0 muxing is opt-in via `muxer::open_with_kind` with an
`AviKind::OpenDml(RiffSegmentLimit::OneGiB)` (or `Bytes(n)` for
testing). The muxer rolls a new `RIFF AVIX` segment whenever the
running segment would exceed the byte ceiling; the primary segment
carries an `indx` super-index in the first stream's `strl` with one
entry per segment back-patched in `write_trailer`, and each segment
emits a per-stream `ix##` `AVISTDINDEX` chunk at the tail of its
`movi` LIST. Demuxer-side, `seek_to` falls back to the `ix##`
standard indexes when no AVI 1.0 `idx1` is present (typical for
OpenDML-only files written by recent ffmpeg / VirtualDub2 with
`--max_riff_size` set). The legacy `muxer::open` (which `Avi10`
defaults to) refuses to grow past ~2 GiB and returns
`Error::Unsupported`.

## Codec mapping

AVI identifies video streams by a 4-byte `biCompression` FourCC in the
`strf`/BITMAPINFOHEADER, and audio streams by a 16-bit `wFormatTag` in
the `strf`/WAVEFORMATEX. `oxideav-avi` translates these both ways into
the stable codec ids used everywhere else in oxideav.

FourCC matching is case-insensitive. Unknown tags round-trip as
`avi:<fourcc>` (video) or `avi:tag_xxxx` (audio) rather than failing —
so a caller can still walk an unrecognised stream, it just won't find a
decoder.

### Video FourCC to codec id

| Codec id       | FourCCs accepted (case-insensitive)                                                             |
|----------------|-------------------------------------------------------------------------------------------------|
| `mjpeg`        | `MJPG`, `AVRN`, `LJPG`, `JPGL`                                                                  |
| `ffv1`         | `FFV1`                                                                                          |
| `h263`         | `H263`, `U263`, `M263`, `ILVR`, `VX1K`, `VIV1`, `X263`, `T263`, `S263`, `L263`                  |
| `h264`         | `H264`, `AVC1`, `X264`, `VSSH`, `DAVC`, `PAVC`, `AVC2`, `AVC3`, AVC-Intra `AI13`/`AI15`/...     |
| `h265`         | `HEVC`, `H265`, `HVC1`, `HEV1`, `X265`, `DXHE`                                                  |
| `mpeg1video`   | `MPG1`, `MPEG`                                                                                  |
| `mpeg2video`   | `MPG2`, `MP2V`, `EM2V`, HDV flavours `HDV1`..`HDV9`                                             |
| `mpeg4video`   | `XVID`, `DIVX`, `DX50`, `DX40`, `MP4V`, `FMP4`, `DIV3`..`DIV6`, `3IV2`, `M4S2`, `MP4S`, `BLZ0`, `DXGM`, `RMP4`, `SMP4`, `UMP4`, `XVIX`, `WV1F`, `DIVF`, `MP43` |
| `vp6`          | `VP60`, `VP61`, `VP62`, `VP6F`, `VP6A`                                                          |
| `vp8` / `vp9`  | `VP80`/`VP8 `, `VP90`/`VP9 `                                                                    |
| `av1`          | `AV01`                                                                                          |
| `theora`       | `THEO`                                                                                          |
| `huffyuv` / `ffvhuff` | `HFYU`, `FFVH`                                                                           |
| `utvideo`      | `UTVS`, `ULRG`, `ULRA`, `ULY0`/`ULY2`/`ULY4`, `ULH0`/`ULH2`/`ULH4`                              |
| `magicyuv`     | 8-bit `M8RG`/`M8RA`/`M8Y4`/`M8Y2`/`M8Y0`/`M8YA`/`M8G0`, 10-bit `M0RG`/`M0RA`/`M0Y4`/`M0Y2`/`M0Y0`/`M0G0`, 12-bit `M2RG`/`M2RA`, 14-bit `M4RG`/`M4RA` |
| `prores`       | `APCH`, `APCN`, `APCS`, `APCO`, `AP4H`, `AP4X`                                                  |
| `dv`           | `DVSD`, `DV25`, `DV50`, `DVC `, `DVCP`, `DVHD`, `DVH1`                                          |
| `wmv1`/`wmv2`/`wmv3` | `WMV1`, `WMV2`, `WMV3`                                                                    |
| `vc1`          | `WVC1`, `WMVA`                                                                                  |
| `cinepak`      | `CVID`                                                                                          |
| `indeo3`/`indeo4`/`indeo5` | `IV31`/`IV32`, `IV41`/`IV42`, `IV50`                                                  |
| `svq1` / `svq3`| `SVQ1`, `SVQ3`                                                                                  |
| `flv1`         | `FLV1`                                                                                          |
| `rgb24`        | `BI_RGB` (all-zero FourCC), `DIB `, `RGB `, `RAW `                                              |
| `yuyv422` / `uyvy422` | `YUY2`/`YUYV`/`V422`/`YUNV`, `UYVY`/`Y422`/`UYNV`                                        |
| `nv12` / `nv21`| `NV12`, `NV21`                                                                                  |
| `yuv420p`      | `I420`, `IYUV`, `YV12`                                                                          |
| `yuv411p`      | `Y41P`                                                                                          |
| `yuv444p10le` / `v210` / `r210` | `V410`, `V210`, `R210`/`R10K`                                                  |

### Audio wFormatTag to codec id

| Codec id       | WAVEFORMATEX tag(s)                                            |
|----------------|----------------------------------------------------------------|
| `pcm_u8`       | `0x0001` with `wBitsPerSample = 8`                             |
| `pcm_s16le`    | `0x0001` with `wBitsPerSample = 16`                            |
| `pcm_s24le`    | `0x0001` with `wBitsPerSample = 24`                            |
| `pcm_s32le`    | `0x0001` with `wBitsPerSample = 32`                            |
| `pcm_f32le` / `pcm_f64le` | `0x0003` with 32 / 64 bits per sample               |
| `pcm_alaw`     | `0x0006`                                                       |
| `pcm_mulaw`    | `0x0007`                                                       |
| `adpcm_ms`     | `0x0002`                                                       |
| `adpcm_ima_wav`| `0x0011`                                                       |
| `adpcm_yamaha` | `0x0020`                                                       |
| `adpcm_ima_dk4`/`adpcm_ima_dk3` | `0x0061`, `0x0062`                            |
| `g722`         | `0x0028`                                                       |
| `g723_1`       | `0x0014`                                                       |
| `gsm_ms`       | `0x0031`, `0x0032`                                             |
| `mp2`          | `0x0050`                                                       |
| `mp3`          | `0x0055`                                                       |
| `aac`          | `0x00FF`, `0x706D`, `0x4143`, `0xA106`                         |
| `wmav1`/`wmav2`/`wmapro`/`wmalossless` | `0x0160`, `0x0161`, `0x0162`, `0x0163` |
| `ac3`          | `0x2000`, `0x6AC3`                                             |
| `eac3`         | `0x2001`, `0xEAC3`                                             |
| `dts`          | `0x2002`, `0x0008`                                             |
| `opus`         | `0x4F70`, `0x704F`, `0x7075`                                   |
| `vorbis`       | `0x674F`/`0x6750`/`0x6751`/`0x676F`/`0x6770`/`0x6771`          |
| `flac`         | `0xF1AC`                                                       |

Muxer-side packaging (`strf` + `strh`) is implemented for a subset of
the above: `mjpeg`, `ffv1`, `mpeg1video`, `mpeg2video`, `mpeg4video`,
`h263`, `h264`, `h265`, `vp8`, `vp9`, `av1`, `magicyuv`, `rgb24`, all
six raw PCM variants (`pcm_u8`/`s16le`/`s24le`/`s32le`/`f32le`/`f64le`),
`pcm_alaw`, `pcm_mulaw`, `mp2`, `mp3`, `aac`, `ac3`, `eac3`, `flac`.
Other codec ids return `Error::Unsupported` at `open()`.

For codec ids that share several FourCCs (e.g. `mpeg4video` →
`XVID`/`DIVX`/...; `magicyuv` → 17 native FourCCs), the muxer picks a
default FourCC unless the caller hints otherwise. For `magicyuv`, the
hint is the leading 4 bytes of `CodecParameters::extradata` if they
spell one of the 17 native FourCCs; otherwise `M8RG` is used.

## Quick use

### Demux

```rust
use oxideav_container::{ContainerRegistry, ReadSeek};

let mut containers = ContainerRegistry::new();
oxideav_avi::register(&mut containers);

let input: Box<dyn ReadSeek> =
    Box::new(std::io::Cursor::new(std::fs::read("capture.avi")?));
let mut dmx = containers.open_demuxer("avi", input)?;

for s in dmx.streams() {
    eprintln!("stream {}: {} ({:?})", s.index, s.params.codec_id, s.params.media_type);
}

loop {
    match dmx.next_packet() {
        Ok(pkt) => {
            // feed into the matching decoder
        }
        Err(oxideav_core::Error::Eof) => break,
        Err(e) => return Err(e.into()),
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`seek_to(stream, pts)` lands on the nearest keyframe at or before the
target, using the `idx1` index. Files without `idx1` (streamed AVI,
half-written recordings) return `Error::Unsupported` for seek; linear
decode still works.

### Mux

```rust
use oxideav_container::{ContainerRegistry, WriteSeek};
use oxideav_core::{CodecId, CodecParameters, MediaType, Packet,
                   Rational, SampleFormat, StreamInfo, TimeBase};

let mut containers = ContainerRegistry::new();
oxideav_avi::register(&mut containers);

// One video + one audio stream.
let mut v = CodecParameters::video(CodecId::new("mjpeg"));
v.width = Some(640);
v.height = Some(480);
v.frame_rate = Some(Rational::new(25, 1));

let mut a = CodecParameters::audio(CodecId::new("pcm_s16le"));
a.channels = Some(2);
a.sample_rate = Some(48_000);
a.sample_format = Some(SampleFormat::S16);

let streams = [
    StreamInfo { index: 0, time_base: TimeBase::new(1, 25),
                 duration: None, start_time: Some(0), params: v },
    StreamInfo { index: 1, time_base: TimeBase::new(1, 48_000),
                 duration: None, start_time: Some(0), params: a },
];

let out: Box<dyn WriteSeek> =
    Box::new(std::fs::File::create("out.avi")?);
let mut mux = containers.open_muxer("avi", out, &streams)?;
mux.write_header()?;
// mux.write_packet(&pkt)? for each encoded packet, interleaved
mux.write_trailer()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The muxer always emits a legacy `idx1` index at the end of the file so
the resulting AVI is seekable. The 2 GiB ceiling is enforced strictly —
if `write_packet` would push the file past ~2 GiB, it returns
`Error::Unsupported` so the caller can roll a new segment.

### Container id

- Container: `"avi"`, matches `.avi` by extension and the
  `RIFF....AVI ` magic.
- Registered via `oxideav_avi::register(&mut ContainerRegistry)`.

## License

MIT — see [LICENSE](LICENSE).
