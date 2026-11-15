#![no_main]

//! Structure-aware fuzz of the **write side**: interpret the fuzz
//! bytes as a bounded, valid-by-construction mux recipe (streams x
//! packets x `AviKind` x rec-cluster / index-layout / vprp / INFO /
//! side-band options) and assert the round-trip identity contract
//! through our own demuxer.
//!
//! Unlike the `demux` target (whose contract is "malformed input
//! must error, never panic"), every recipe decoded here is valid by
//! construction, so the contract is strict:
//!
//!   * `open_avi` (mux) + `write_header` + every `write_packet` +
//!     `write_trailer` must succeed;
//!   * the produced bytes must open through `open_avi` AND through
//!     `open_avi_strict` — the strict idx1 <-> `ix##` cross-validator
//!     must accept our own writer's output, whatever combination of
//!     rec-clusters / mid-`movi` flushes / synthesised idx1 /
//!     in-`strl` compact index the recipe picked;
//!   * the demuxer must report the same stream count and, per
//!     stream, the same packet count and byte-identical payloads in
//!     order — where "packet" follows the container's own rule: a
//!     zero-length data chunk is a frame-time slot with no frame
//!     (`dwSampleSize` 0: one chunk per frame-time; only an
//!     `AVIIF_NO_TIME` chunk does not affect timing — AVI 1.0
//!     §AVISTREAMHEADER / §AVIOLDINDEX), so an empty video packet
//!     handed to the muxer is NOT returned by the demuxer, but the
//!     stream clock counts it: every returned video packet must carry
//!     the frame-slot number it was written at as its `pts`;
//!   * every video packet's demuxed `flags.keyframe` must equal the
//!     flag the muxer was handed (AVI 1.0 Appendix C
//!     `AVIIF_KEYFRAME` on the idx1 side, the OpenDML `ix##`
//!     `dwSize` delta high bit on the AVIX side);
//!   * when stream 0 is video and its first packet is a non-empty
//!     keyframe, `seek_to(0, 0)` must land and the next packet read
//!     must exist; on every other plan the seek may refuse but must
//!     not panic.
//!
//! The arithmetic under fire: the RIFF AVIX segment-roll projection
//! (tiny `RiffSegmentLimit::Bytes` ceilings force a roll every few
//! packets), `LIST rec ` cluster open/close + idx1 `AVIIF_LIST`
//! entries, the in-`strl` compact standard index and its overflow
//! migration back to the two-tier layout, mid-`movi` periodic `ix##`
//! flushes, super-index capacity overflow signalling, idx1
//! synthesis from per-packet `ix##` records, JUNK alignment padding,
//! and the palette-change / text side-band records interleaved with
//! data chunks.

use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

use oxideav_avi_fuzz::{MuxPlan, Recipe, registry};
use oxideav_core::{Demuxer as _, Error, ReadSeek};

fuzz_target!(|data: &[u8]| {
    let mut r = Recipe::new(data);
    let plan = MuxPlan::decode(&mut r);
    let file = plan.run();

    let reg = registry();

    // -- Strict open must accept our own writer's output. ------------
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file.clone()));
    oxideav_avi::demuxer::open_avi_strict(rs, &reg)
        .expect("muxer output must pass strict idx1<->ix## cross-validation");

    // -- Identity drain through the default front door. ---------------
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file.clone()));
    let mut dmx = oxideav_avi::demuxer::open_avi(rs, &reg).expect("muxer output must open");
    assert_eq!(
        dmx.streams().len(),
        plan.streams.len(),
        "stream count must survive the round trip"
    );

    let n = plan.streams.len();
    // What the demuxer owes us back: every non-empty packet, with the
    // frame-slot number it was written at (video) — a zero-length
    // chunk takes its slot on the stream clock but is not a packet.
    let want: Vec<Vec<(&[u8], bool, i64)>> = plan
        .sent
        .iter()
        .map(|list| {
            list.iter()
                .enumerate()
                .filter(|(_, (d, _))| !d.is_empty())
                .map(|(slot, (d, k))| (d.as_slice(), *k, slot as i64))
                .collect()
        })
        .collect();
    let mut got: Vec<Vec<(Vec<u8>, bool, Option<i64>)>> = vec![Vec::new(); n];
    let cap = plan.total_packets() + 64;
    for _ in 0..cap {
        match dmx.next_packet() {
            Ok(p) => {
                let s = p.stream_index as usize;
                assert!(s < n, "demuxed packet stream index in range");
                got[s].push((p.data, p.flags.keyframe, p.pts));
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error on muxer output: {e}"),
        }
    }

    for (s, got_s) in got.iter().enumerate() {
        assert_eq!(
            got_s.len(),
            want[s].len(),
            "stream {s}: non-empty packet count must survive the round trip"
        );
        for (i, ((gd, gk, gpts), (sd, sk, slot))) in got_s.iter().zip(want[s].iter()).enumerate() {
            assert_eq!(gd, sd, "stream {s} packet {i}: payload bytes must match");
            if plan.is_video[s] {
                assert_eq!(
                    gk, sk,
                    "stream {s} packet {i}: video keyframe flag must survive"
                );
                assert_eq!(
                    *gpts,
                    Some(*slot),
                    "stream {s} packet {i}: pts must be the frame slot it was written at"
                );
                assert_eq!(
                    dmx.packet_is_keyframe(s as u32, i),
                    Some(*sk),
                    "stream {s} packet {i}: index keyframe view must follow returned packets"
                );
            }
        }
    }

    // -- Seek contract on the leading video stream. -------------------
    let lead = plan.is_video[0].then(|| plan.sent[0].first()).flatten();
    if lead.is_some_and(|(d, kf)| !d.is_empty() && *kf) {
        dmx.seek_to(0, 0)
            .expect("seek to pts 0 must land on a file whose first video packet is a keyframe");
        let p = dmx
            .next_packet()
            .expect("a packet must follow a landed seek");
        let _ = p;
    } else {
        // Empty lead slot / no video: landing is the file's business,
        // not panicking is ours.
        let _ = dmx.seek_to(0, 0);
        let _ = dmx.next_packet();
    }
});
