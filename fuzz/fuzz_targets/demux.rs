#![no_main]

//! Arbitrary hostile bytes through the full demux surface.
//!
//! Every walk decision in an AVI file is driven by attacker-
//! controlled 32-bit LE size fields: the top-level `RIFF AVI ` /
//! `RIFF AVIX` segments, nested `LIST` forms (`hdrl` / `strl` /
//! `movi` / `odml` / `INFO` / `rec `), the `avih` main header, the
//! per-stream `strh` + `strf` (BITMAPINFOHEADER / WAVEFORMATEX /
//! WAVEFORMATEXTENSIBLE) + `strd` / `strn` / `vprp` / `indx` chunks,
//! the legacy `idx1` table with its movi-relative vs file-absolute
//! offset-base probe and `rec ` LIST entries, and the OpenDML 2.0
//! two-tier index (`indx` AVISUPERINDEX -> per-segment `ix##`
//! AVISTDINDEX, plus the compact in-`strl` `AVI_INDEX_OF_CHUNKS`
//! layout and 12-byte `AVI_INDEX_2FIELD` entries).
//!
//! Contract under test: all three open front doors (`open_avi`,
//! `open_avi_lenient`, `open_avi_strict`) ALWAYS return a `Result` —
//! no panic, no abort, no debug-build integer overflow, no
//! out-of-bounds index, no allocation proportional to an attacker-
//! claimed `cb` / entry-count field. Anything that opens is then
//! drained (bounded), seeked (both the idx1 path and the `ix##`
//! std-index path), and run through the full accessor battery.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    oxideav_avi_fuzz::open_all_and_exercise(data);
});
