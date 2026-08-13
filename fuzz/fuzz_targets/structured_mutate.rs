#![no_main]

//! Structure-aware hostile mutation — the coverage-guided upgrade of
//! the in-tree round-394 deterministic harness.
//!
//! The first recipe bytes build a writer-shaped fixture through our
//! own muxer (AVI 1.0 with idx1 + side-bands, multi-segment OpenDML
//! with `indx` super-indexes + per-segment `ix##`, or the compact
//! in-`strl` standard index — whichever the recipe picks, with
//! rec-clusters / mid-`movi` flushes / vprp / INFO on top). The
//! remaining bytes drive a bounded mutation program over the
//! fixture:
//!
//!   * byte flips anywhere (headers, size fields, index entries);
//!   * truncation at an arbitrary point (capture crash dumps);
//!   * targeted 8/16-byte overwrites right after each `idx1` /
//!     `indx` / `ix##` FourCC — the exact fields the index walkers
//!     arithmetic on (entry counts, strides, `qwOffset` targets,
//!     `qwBaseOffset`, duration ticks, all-ones sizes).
//!
//! Because the mutation starts from a structurally deep file, the
//! fuzzer reaches parser states (super-index entry walks, AVIX
//! continuation chains, palette side-band scans, the idx1 offset-
//! base probe) that raw random bytes almost never assemble.
//!
//! Contract: no panic, no abort, no debug-build overflow, no
//! attacker-proportional allocation — whether a mutant opens or
//! errors is the file's business. All three open front doors plus
//! the full accessor battery + bounded drain + both seek paths run
//! on every mutant.

use libfuzzer_sys::fuzz_target;

use oxideav_avi_fuzz::{open_all_and_exercise, MuxPlan, Recipe};

fuzz_target!(|data: &[u8]| {
    let mut r = Recipe::new(data);
    let plan = MuxPlan::decode(&mut r);
    let base = plan.run();
    if base.is_empty() {
        return;
    }

    // Mutation program from the remaining recipe bytes.
    let mut m = base.clone();
    let mut ops = Recipe::new(r.rest());
    let n_ops = 1 + (ops.u8() % 8) as usize;
    let mut truncated = false;
    for _ in 0..n_ops {
        match ops.u8() % 4 {
            // Byte flip.
            0 | 1 => {
                let pos = ops.u32() as usize % m.len();
                let val = ops.u8();
                m[pos] ^= val.max(1);
            }
            // Truncate (final op: everything after is meaningless).
            2 => {
                let cut = ops.u32() as usize % (m.len() + 1);
                m.truncate(cut);
                truncated = true;
            }
            // Targeted index-structure corruption: overwrite the
            // bytes right after the k-th idx1/indx/ix## FourCC.
            _ => {
                let mut hits: Vec<usize> = Vec::new();
                for k in 0..m.len().saturating_sub(4) {
                    let t = &m[k..k + 4];
                    if t == b"idx1" || t == b"indx" || (t[0] == b'i' && t[1] == b'x') {
                        hits.push(k);
                    }
                }
                if hits.is_empty() {
                    continue;
                }
                let at = hits[ops.u8() as usize % hits.len()];
                let skip = 4 + 4 * (ops.u8() as usize % 3); // header, size, or body
                let start = (at + skip).min(m.len());
                let len = 4 + (ops.u8() as usize % 13);
                let end = (start + len).min(m.len());
                for b in &mut m[start..end] {
                    *b = ops.u8();
                }
            }
        }
        if truncated || m.is_empty() {
            break;
        }
    }

    open_all_and_exercise(&m);
});
