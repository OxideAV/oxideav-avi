//! Round-11 AVI feature tests.
//!
//! Covers:
//! - **C1** Top-level `LIST INFO` muxer write path —
//!   [`AviMuxOptions::with_top_level_info`] places the metadata
//!   list as a sibling of `LIST hdrl` (between hdrl and movi)
//!   instead of nested inside hdrl. Both placements are AVI-1.0
//!   spec-compliant; the demuxer's existing `b"INFO" if is_primary`
//!   walker arm keeps the round-trip byte-equal on metadata payload.
//! - **C2** `AviDemuxer::seek_to_keyframe_strict_via_std_index` —
//!   forces the OpenDML 2.0 `ix##` standard-index seek path
//!   regardless of whether `idx1` is also present, and returns the
//!   same [`KeyframeSeekResult`] shape as the round-9 strict
//!   variant. Useful for OpenDML-only files (no `idx1`) and as a
//!   muxer-fidelity sanity check on dual-indexed files.
//! - **C3** `AviMuxer::write_text_chunk` / `write_palette_change`
//!   side-band emitters — the muxer can now write `NNtx` /
//!   `NNpc` chunks into the current `movi` LIST, lay down idx1
//!   entries for each, and (in OpenDML mode) include them in the
//!   per-stream `ix##` standard-index. The demuxer's round-8 C3 +
//!   round-10 C1 read paths (`palette_change_count` /
//!   `text_chunk_count`) close the round-trip.

use std::io::{Read, Seek, Write};

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
// Keep `Muxer` referenced — its trait methods (write_header /
// write_packet / write_trailer) are call-sites elsewhere in this
// file but rustc's `unused_imports` can still gate on the symbol
// alone in some compiler versions.
#[allow(dead_code)]
fn _muxer_trait_in_scope<M: Muxer>() {}

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

// ---------------------------------------------------------------------------
// Test fixtures shared across round-11 cases.
// ---------------------------------------------------------------------------

fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg
}

fn magicyuv_stream(width: u32, height: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn synth_payload(seed: u32, base_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(base_len);
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..base_len {
        s = s.wrapping_mul(0x100_0193).wrapping_add(0x811C_9DC5);
        out.push((s >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// C1: top-level LIST INFO write path.
// ---------------------------------------------------------------------------

/// Find the byte offset of `needle` in `haystack`, or `None` if absent.
fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn top_level_info_lands_between_hdrl_and_movi() {
    // Round-11 C1: `with_top_level_info(true)` emits the LIST INFO
    // chunk as a sibling of LIST hdrl. Walk the raw bytes and
    // confirm that:
    //   - the LIST INFO appears OUTSIDE hdrl's body (its position is
    //     past the hdrl close), and
    //   - it appears BEFORE the LIST movi.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(7, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-info-toplevel.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_info(*b"INAM", "Top-Level Title")
            .with_info(*b"IART", "Top-Level Artist")
            .with_top_level_info(true);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&tmp)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();

    // RIFF preamble: "RIFF" + size(4) + "AVI " = 12 B. Then "LIST" +
    // hdrl_size + "hdrl" — so hdrl starts at file offset 12 and its
    // body runs from 24 to 24 + hdrl_size - 4.
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"AVI ");
    assert_eq!(&bytes[12..16], b"LIST");
    let hdrl_size = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
    assert_eq!(&bytes[20..24], b"hdrl");
    let hdrl_end = 24 + hdrl_size - 4;
    let hdrl_body = &bytes[24..hdrl_end];

    // The round-6 default (nested LIST INFO inside hdrl) would put a
    // "LIST" + size + "INFO" sub-chunk inside hdrl's body. Top-level
    // placement must NOT have INFO inside hdrl.
    let info_marker: Vec<u8> = {
        let mut v = b"LIST".to_vec();
        // skip 4 size bytes via window-search on full LIST + INFO seq:
        // we look for "LIST????INFO" using two separate searches.
        v.extend_from_slice(b"INFO");
        v
    };
    // Walk hdrl_body looking for "LIST" followed (4 bytes later) by
    // "INFO". This catches both placements in a single scan.
    let mut nested_found = false;
    let mut i = 0;
    while i + 12 <= hdrl_body.len() {
        if &hdrl_body[i..i + 4] == b"LIST" && &hdrl_body[i + 8..i + 12] == b"INFO" {
            nested_found = true;
            break;
        }
        i += 1;
    }
    assert!(
        !nested_found,
        "with_top_level_info(true) must NOT nest LIST INFO inside hdrl"
    );

    // Outside hdrl: search the file from hdrl_end forward for LIST INFO
    // and LIST movi. INFO must appear before movi.
    let after_hdrl = &bytes[hdrl_end..];
    let info_pos = {
        let mut found = None;
        let mut j = 0;
        while j + 12 <= after_hdrl.len() {
            if &after_hdrl[j..j + 4] == b"LIST" && &after_hdrl[j + 8..j + 12] == b"INFO" {
                found = Some(j);
                break;
            }
            j += 1;
        }
        found.expect("top-level LIST INFO must appear after hdrl")
    };
    let movi_pos = find_subseq(after_hdrl, b"movi").expect("LIST movi must follow");
    assert!(
        info_pos < movi_pos,
        "top-level LIST INFO ({info_pos}) must precede LIST movi ({movi_pos})"
    );

    // Sanity check that `info_marker` is unused: silences dead-code
    // warning on the helper buffer.
    drop(info_marker);
}

#[test]
fn top_level_info_round_trips_metadata() {
    // Round-11 C1: the demuxer must surface the same metadata for
    // top-level INFO as it does for nested-in-hdrl INFO. Round-6 C2
    // already covers nested; this test pins the top-level path.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(13, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-info-toplevel-rt.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_info(*b"INAM", "RT Title")
            .with_info(*b"IART", "RT Artist")
            .with_info(*b"ICMT", "RT Comment")
            .with_top_level_info(true);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let md = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(get("title").as_deref(), Some("RT Title"));
    assert_eq!(get("artist").as_deref(), Some("RT Artist"));
    assert_eq!(get("comment").as_deref(), Some("RT Comment"));
}

#[test]
fn top_level_info_default_off_keeps_nested_layout() {
    // Round-11 C1: `with_top_level_info` defaults to false so the
    // round-6 nested-in-hdrl layout stays the default. Confirm by
    // searching for LIST INFO inside hdrl when the new builder is
    // not called.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(99, 32);

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-info-default-nested.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_info(*b"INAM", "Nested Default");
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&tmp)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let hdrl_size = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
    let hdrl_end = 24 + hdrl_size - 4;
    let hdrl_body = &bytes[24..hdrl_end];

    let mut nested_found = false;
    let mut i = 0;
    while i + 12 <= hdrl_body.len() {
        if &hdrl_body[i..i + 4] == b"LIST" && &hdrl_body[i + 8..i + 12] == b"INFO" {
            nested_found = true;
            break;
        }
        i += 1;
    }
    assert!(
        nested_found,
        "default (with_top_level_info NOT called) keeps INFO nested in hdrl"
    );
}

// ---------------------------------------------------------------------------
// C2: seek_to_keyframe_strict_via_std_index.
// ---------------------------------------------------------------------------

#[test]
fn strict_via_std_index_lands_zero_gap_on_keyframe_with_idx1_present() {
    // Round-11 C2: `seek_to_keyframe_strict_via_std_index` always
    // walks the OpenDML std-index, even when idx1 is present. Build
    // a TRUE multi-segment OpenDML AVI (bytes-bounded segments
    // ensure the demuxer's `want_ix_scan` gate fires so std_indexes
    // gets populated) where every frame is a keyframe — the
    // std-index lands pts=5 the same way idx1 would.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..10).map(|i| synth_payload(i + 100, 512)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-stdix-strict-kf.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::Bytes(2 * 1024)),
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    let res = dmx
        .seek_to_keyframe_strict_via_std_index(0, 5)
        .expect("std-index strict seek must succeed");
    assert_eq!(res.target_pts, 5);
    assert_eq!(res.landed_pts, 5);
    assert_eq!(res.gop_distance, 0);
}

#[test]
fn strict_via_std_index_works_on_opendml_only_file_idx1_stripped() {
    // Round-11 C2: the strict-via-std-index variant must work
    // when idx1 is missing entirely — the canonical OpenDML-only
    // case. Produce an OpenDML AVI, strip idx1 (rename FourCC to
    // JUNK so the walker skips it), then strict-seek on the
    // std-index.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..16).map(|i| synth_payload(i + 200, 512)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-stdix-strict-opendml-only.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::Bytes(8 * 1024)),
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Strip idx1 so the demuxer must use ix## std-indexes only.
    let mut bytes = std::fs::read(&tmp).unwrap();
    let mut found = None;
    for (i, w) in bytes.windows(4).enumerate() {
        if w == b"idx1" {
            found = Some(i);
            break;
        }
    }
    let pos = found.expect("muxer always emits idx1 for primary segment");
    bytes[pos..pos + 4].copy_from_slice(b"JUNK");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    let res = dmx
        .seek_to_keyframe_strict_via_std_index(0, 8)
        .expect("OpenDML-only strict-via-std-index must succeed");
    assert_eq!(res.target_pts, 8);
    assert_eq!(res.landed_pts, 8);
    assert_eq!(res.gop_distance, 0);
}

#[test]
fn strict_via_std_index_errors_when_no_std_index() {
    // Round-11 C2: the strict-via-std-index variant errors when no
    // ix## chunks are present (the AVI 1.0 envelope has no
    // std-indexes at all). Use AviKind::Avi10 to produce such a
    // file.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(7, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-stdix-strict-no-stdix.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    let err = dmx
        .seek_to_keyframe_strict_via_std_index(0, 0)
        .expect_err("AVI 1.0 has no ix## std-indexes → strict variant must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("ix## standard indexes") || msg.contains("std"),
        "error must reference std-index requirement; got `{msg}`"
    );
}

#[test]
fn strict_via_std_index_reports_non_zero_gap_inside_gop() {
    // Round-11 C2: strict-via-std-index reports the GOP distance
    // when the requested PTS is mid-GOP. Build a multi-segment
    // OpenDML AVI (so the demuxer's want_ix_scan gate fires) then
    // patch every ix## entry's high `dwSize` keyframe bit so only
    // global frames 0 and 5 stay flagged as keyframes (high bit
    // CLEAR = keyframe per OpenDML 2.0 §3.0).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..10).map(|i| synth_payload(i + 300, 512)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-stdix-strict-gop.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::Bytes(2 * 1024)),
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Walk every `ix00` chunk in the file (one per segment) and patch
    // each entry's high `dwSize` bit. AVISTDINDEX header is 24 B per
    // OpenDML 2.0 §3.0; entries are 8 B each (wLongsPerEntry=2). We
    // track a global frame-counter across segments so the
    // keyframe-only-at-{0,5} pattern survives the per-segment ix##
    // splitting.
    let mut bytes = std::fs::read(&tmp).unwrap();
    let mut ix_positions: Vec<usize> = Vec::new();
    for (i, w) in bytes.windows(4).enumerate() {
        if w == b"ix00" {
            ix_positions.push(i);
        }
    }
    assert!(
        !ix_positions.is_empty(),
        "multi-segment OpenDML must emit at least one ix00"
    );
    let mut global_frame = 0usize;
    for ix_off in ix_positions {
        let n_entries = u32::from_le_bytes([
            bytes[ix_off + 12],
            bytes[ix_off + 13],
            bytes[ix_off + 14],
            bytes[ix_off + 15],
        ]) as usize;
        let entries_off = ix_off + 32;
        for i in 0..n_entries {
            let entry_off = entries_off + i * 8;
            let dw_size_off = entry_off + 4;
            let mut dw_size = u32::from_le_bytes([
                bytes[dw_size_off],
                bytes[dw_size_off + 1],
                bytes[dw_size_off + 2],
                bytes[dw_size_off + 3],
            ]);
            if global_frame == 0 || global_frame == 5 {
                dw_size &= 0x7FFF_FFFF;
            } else {
                dw_size |= 0x8000_0000;
            }
            bytes[dw_size_off..dw_size_off + 4].copy_from_slice(&dw_size.to_le_bytes());
            global_frame += 1;
        }
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    let res = dmx
        .seek_to_keyframe_strict_via_std_index(0, 8)
        .expect("strict-via-std-index seek must succeed");
    assert_eq!(res.target_pts, 8);
    assert_eq!(res.landed_pts, 5);
    assert_eq!(res.gop_distance, 3);
}

#[test]
fn strict_via_std_index_rejects_oob_stream_index() {
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..6).map(|i| synth_payload(i + 11, 512)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-stdix-strict-oob.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::Bytes(2 * 1024)),
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert!(dmx.seek_to_keyframe_strict_via_std_index(7, 0).is_err());
}

// ---------------------------------------------------------------------------
// C3: xxtx / xxpc muxer write helpers.
// ---------------------------------------------------------------------------

#[test]
fn write_text_chunk_round_trips_via_text_chunk_count_accessor() {
    // Round-11 C3: write 3 text chunks alongside a couple of regular
    // video packets, then re-open the file and confirm the demuxer's
    // `text_chunk_count(stream)` accessor returns 3 — closing the
    // muxer side of round-10 C1.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(7, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-xxtx-emit.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for i in 0..3 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
            mux.write_text_chunk(0, b"caption!\0").unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.text_chunk_count(0),
        3,
        "demuxer must report 3 text chunks via the round-10 C1 accessor"
    );
    let md = dmx.metadata().to_vec();
    let v = md
        .iter()
        .find(|(k, _)| k == "avi:text_chunk.0")
        .map(|(_, v)| v.clone());
    assert_eq!(v.as_deref(), Some("3"));
}

#[test]
fn write_palette_change_round_trips_via_palette_change_count_accessor() {
    // Round-11 C3: same flow for `xxpc`. Two palette-change chunks
    // alongside a couple of video packets must surface as
    // `palette_change_count(0) == 2`.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(13, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-xxpc-emit.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        // Minimal AVI 1.0 palette-change body: bFirstEntry=0,
        // bNumEntries=2, wFlags=0, two 4-byte palette quads.
        let pal = [0u8, 2, 0, 0, 0xFF, 0, 0, 0, 0, 0xFF, 0, 0];
        for i in 0..2 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
            mux.write_palette_change(0, &pal).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.palette_change_count(0),
        2,
        "demuxer must report 2 palette-change chunks via the round-8 C3 accessor"
    );
}

#[test]
fn write_text_chunk_does_not_inflate_strh_dw_length() {
    // Round-11 C3: side-band chunks must NOT bump the parent
    // stream's `strh.dwLength` (which the demuxer surfaces via
    // `streams()[s].duration`). 4 video packets + 6 text chunks
    // → strh.dwLength = 4 frames, not 10.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(99, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-xxtx-no-inflate.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for i in 0..4 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
            mux.write_text_chunk(0, b"sub\0").unwrap();
            mux.write_text_chunk(0, b"sub2\0").unwrap();
        }
        // Trailing extra text chunk after the last packet.
        mux.write_text_chunk(0, b"final\0").unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let dur = dmx.streams()[0].duration;
    assert_eq!(
        dur,
        Some(4),
        "strh.dwLength must equal the regular-packet count (4), not include side-band chunks"
    );
    assert_eq!(dmx.text_chunk_count(0), 9, "9 total text chunks expected");
}

#[test]
fn write_sideband_chunk_errors_before_header_or_after_trailer() {
    // Round-11 C3: side-band emitters must error if invoked outside
    // the [write_header, write_trailer] window.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(1, 32);

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-xxtx-state.avi");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(
        ws,
        std::slice::from_ref(&stream),
        AviKind::Avi10,
        AviMuxOptions::new(),
    )
    .unwrap();
    // Before write_header: must error.
    assert!(mux.write_text_chunk(0, b"x").is_err());
    assert!(mux.write_palette_change(0, b"x").is_err());

    mux.write_header().unwrap();
    let mut pkt = Packet::new(0, stream.time_base, payload.clone());
    pkt.pts = Some(0);
    pkt.flags.keyframe = true;
    mux.write_packet(&pkt).unwrap();
    mux.write_trailer().unwrap();
    // After write_trailer: must error.
    assert!(mux.write_text_chunk(0, b"x").is_err());
    assert!(mux.write_palette_change(0, b"x").is_err());
}

#[test]
fn write_text_chunk_in_opendml_mode_lands_in_ix_index() {
    // Round-11 C3: in OpenDML mode the std-index for the parent
    // stream must include the text-chunk entries (as delta-flagged
    // entries — high `dwSize` bit set). The demuxer's
    // `text_chunk_count` covers idx1; this test asserts the ix##
    // path keeps parity by counting both regular packets and text
    // chunks in nEntriesInUse.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(42, 256);

    let tmp = std::env::temp_dir().join("oxideav-avi-r11-xxtx-opendml.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        for i in 0..3 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
            mux.write_text_chunk(0, b"sub\0").unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    // Find ix00 chunk.
    let mut ix00_off = None;
    for (i, w) in bytes.windows(4).enumerate() {
        if w == b"ix00" {
            ix00_off = Some(i);
            break;
        }
    }
    let ix00 = ix00_off.expect("OpenDML mode must emit ix00 at segment tail");
    let n_entries = u32::from_le_bytes([
        bytes[ix00 + 12],
        bytes[ix00 + 13],
        bytes[ix00 + 14],
        bytes[ix00 + 15],
    ]);
    // 3 packets + 3 text chunks = 6 entries.
    assert_eq!(
        n_entries, 6,
        "ix00 must carry both regular and side-band entries"
    );
}

// Silence unused_imports warnings for items dragged along for the
// fixture helpers but not exercised by every test.
#[allow(dead_code)]
fn _keep_seek_imports_alive(_w: &mut dyn Write, _s: &mut dyn Seek) {}
