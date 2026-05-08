//! Round-6 OpenDML 2.0 + AVI 1.0 feature tests.
//!
//! Covers:
//! - **C1** 2-field idx1 entry-flag emission: muxer sets
//!   `AVIIF_FIRSTPART | AVIIF_LASTPART` (= 0x60) on every idx1 entry
//!   for streams registered via [`AviMuxOptions::with_field2_stream`].
//!   Demuxer surfaces the flags per-entry via
//!   `AviDemuxer::idx1_flags_for_packet` and emits an
//!   `avi:idx1.<n>.is_2field` hint based on the flag bits even when
//!   no `ix##` super-index is present (true AVI 1.0 mode).
//! - **C2** `LIST INFO` muxer-side emit: callers register
//!   `(FourCC, value)` pairs through [`AviMuxOptions::with_info`].
//!   Pairs land in a `LIST INFO` chunk inside `hdrl`, where the
//!   demuxer's `parse_hdrl` recurses into the nested `INFO` and
//!   surfaces each known sub-chunk as a metadata key (e.g.
//!   `INAM` -> `title`).
//! - **C3** OpenDML super-index capacity opt-in: callers raise
//!   [`AviMuxOptions::with_super_index_capacity`] past the default
//!   256 slots. The on-disk `indx` payload size + the per-stream
//!   `dwLength` bookkeeping rolls forward without drift; the
//!   demuxer still accepts the larger preamble unchanged.

use std::io::Read;

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{
    open_avi, AviKind, AviMuxOptions, RiffSegmentLimit, AVIIF_FIRSTPART, AVIIF_KEYFRAME,
    AVIIF_LASTPART, OPENDML_SUPER_INDEX_DEFAULT_CAPACITY,
};

fn registry_with_magicyuv() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv"))
        .tag(CodecTag::fourcc(b"M8RG"))
        .tag(CodecTag::fourcc(b"M8YA"));
    reg.register(info);
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

fn synthesize_payload(seed: u32, base_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(base_len);
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..base_len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// C1: 2-field idx1 entry-flag emission.
// ---------------------------------------------------------------------------

#[test]
fn idx1_entries_carry_firstpart_lastpart_for_2field_streams() {
    // Round-6 C1: every idx1 entry for a stream registered via
    // `with_field2_stream` must carry the
    // `AVIIF_FIRSTPART | AVIIF_LASTPART` (= 0x60) bits in addition
    // to AVIIF_KEYFRAME (0x10) per vfw.h. Verifies via
    // `AviDemuxer::idx1_flags_for_packet`.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synthesize_payload(i + 9000, 192)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-idx1-2field-flags.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_field2_stream(0);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            mux.set_field2_offset(96);
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let want = AVIIF_KEYFRAME | AVIIF_FIRSTPART | AVIIF_LASTPART;
    for i in 0..frames.len() {
        let got = dmx
            .idx1_flags_for_packet(0, i)
            .expect("every keyframe must produce an idx1 entry");
        assert_eq!(
            got, want,
            "idx1 entry {i} flags = 0x{got:02X}; expected 0x{want:02X} (keyframe + part-both)"
        );
    }
    // Out-of-range packet returns None.
    assert!(dmx.idx1_flags_for_packet(0, 999).is_none());
    assert!(dmx.idx1_flags_for_packet(7, 0).is_none());
}

#[test]
fn idx1_entries_lack_part_bits_for_progressive_streams() {
    // Without `with_field2_stream`, idx1 entries must NOT carry
    // AVIIF_FIRSTPART / AVIIF_LASTPART. Bit-level verification.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(12, 96);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-idx1-progressive-flags.avi");
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
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let got = dmx.idx1_flags_for_packet(0, 0).unwrap();
    assert_eq!(got & AVIIF_KEYFRAME, AVIIF_KEYFRAME);
    assert_eq!(
        got & (AVIIF_FIRSTPART | AVIIF_LASTPART),
        0,
        "progressive idx1 entries must not set the FIRSTPART/LASTPART bits"
    );
}

#[test]
fn avi10_idx1_2field_hint_surfaces_from_flag_bits_alone() {
    // A pure AVI 1.0 file (no `ix##` super-index) carrying 2-field
    // entries: the demuxer must still surface
    // `avi:idx1.<n>.is_2field` from the flag bits alone (round-6 C1).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 11000, 96)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-avi10-2field.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        // AviKind::Avi10 + with_field2_stream — the field2 hook
        // will not affect ix## (none in AVI 1.0) but the muxer
        // still stamps the idx1 flag bits.
        let opts = AviMuxOptions::new().with_field2_stream(0);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
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
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let md = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    // ix-layer hint must be ABSENT (AVI 1.0 has no ix##).
    assert!(
        get("avi:ix.0.is_2field").is_none(),
        "AVI 1.0 has no ix## chunks → no avi:ix.<n>.is_2field hint"
    );
    // idx1-layer hint must be PRESENT — surfaced from flag bits alone.
    assert_eq!(
        get("avi:idx1.0.is_2field").as_deref(),
        Some("true"),
        "idx1 hint must surface from FIRSTPART|LASTPART bits even without ix##"
    );
}

// ---------------------------------------------------------------------------
// C2: LIST INFO emit on muxer side.
// ---------------------------------------------------------------------------

#[test]
fn list_info_round_trips_known_keys() {
    // Round-6 C2: AviMuxOptions::with_info(...) emits a LIST INFO
    // chunk inside hdrl carrying the registered entries. The
    // demuxer maps known FourCCs (INAM/IART/...) to the standard
    // metadata key names ("title"/"artist"/...).
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(42, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-info-roundtrip.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_info(*b"INAM", "Test Title")
            .with_info(*b"IART", "Test Artist")
            .with_info(*b"IPRD", "Test Album")
            .with_info(*b"ICMT", "Test Comment")
            .with_info(*b"ICRD", "2026-05-08")
            .with_info(*b"ISFT", "oxideav-avi r6");
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

    assert_eq!(get("title").as_deref(), Some("Test Title"));
    assert_eq!(get("artist").as_deref(), Some("Test Artist"));
    assert_eq!(get("album").as_deref(), Some("Test Album"));
    assert_eq!(get("comment").as_deref(), Some("Test Comment"));
    assert_eq!(get("date").as_deref(), Some("2026-05-08"));
    assert_eq!(get("encoder").as_deref(), Some("oxideav-avi r6"));
}

#[test]
fn list_info_chunk_present_in_hdrl_on_disk() {
    // Round-6 C2: the LIST INFO chunk lives inside hdrl. Walk the
    // raw bytes to confirm the layout: RIFF, AVI, LIST hdrl, ...
    // somewhere inside hdrl we must find LIST INFO.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(7, 32);

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-info-onwire.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_info(*b"INAM", "On-Wire");
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

    // Find the position of the hdrl LIST. Layout is RIFF + size +
    // "AVI " + "LIST" + hdrl_size + "hdrl" + body. So the hdrl body
    // starts at offset 24.
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"AVI ");
    assert_eq!(&bytes[12..16], b"LIST");
    let hdrl_size = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
    assert_eq!(&bytes[20..24], b"hdrl");
    let hdrl_end = 24 + hdrl_size - 4;
    let hdrl_bytes = &bytes[24..hdrl_end];
    // Search for "LIST" + size + "INFO" inside hdrl body.
    let mut found = false;
    let mut i = 0;
    while i + 12 <= hdrl_bytes.len() {
        if &hdrl_bytes[i..i + 4] == b"LIST" && &hdrl_bytes[i + 8..i + 12] == b"INFO" {
            found = true;
            break;
        }
        i += 1;
    }
    assert!(found, "LIST INFO sub-chunk must appear inside hdrl body");
}

#[test]
fn list_info_empty_value_is_skipped() {
    // Round-6 C2: with_info(..., "") is a no-op so callers can
    // gate optional metadata without a separate code path.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(99, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-info-empty.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_info(*b"INAM", "")
            .with_info(*b"IART", "kept");
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
    let titles: Vec<&String> = md
        .iter()
        .filter(|(k, _)| k == "title")
        .map(|(_, v)| v)
        .collect();
    assert!(
        titles.is_empty(),
        "empty INAM value must not emit a 'title' metadata entry"
    );
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "artist")
            .map(|(_, v)| v.as_str()),
        Some("kept")
    );
}

#[test]
fn list_info_omitted_when_no_entries() {
    // No with_info() calls → no LIST INFO on the wire. A round-trip
    // must still succeed and the demuxer must not surface ANY of
    // the standard INFO-mapped metadata keys.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(13, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-info-none.avi");
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
    let mut bytes = Vec::new();
    std::fs::File::open(&tmp)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    // Confirm "INFO" form-type does NOT appear after a LIST header
    // anywhere in the file.
    let mut i = 0;
    while i + 12 <= bytes.len() {
        if &bytes[i..i + 4] == b"LIST" && &bytes[i + 8..i + 12] == b"INFO" {
            panic!("unexpected LIST INFO at offset {i}");
        }
        i += 1;
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let md = dmx.metadata();
    for k in ["title", "artist", "album", "comment", "date", "encoder"] {
        assert!(
            md.iter().all(|(kk, _)| kk != k),
            "no '{k}' metadata expected when LIST INFO is omitted"
        );
    }
}

// ---------------------------------------------------------------------------
// C3: OpenDML super-index capacity opt-in.
// ---------------------------------------------------------------------------

#[test]
fn super_index_capacity_default_is_256() {
    // Sanity check: the public default constant matches the round-3
    // capacity.
    assert_eq!(OPENDML_SUPER_INDEX_DEFAULT_CAPACITY, 256);
}

#[test]
fn raised_super_index_capacity_changes_indx_payload_size() {
    // Round-6 C3: callers can raise the super-index reserve past
    // the default 256. Verify by inspecting the on-disk indx
    // payload length: header (24 B) + cap*16 entries.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(25, 64);

    let raised_cap = 512usize;
    let tmp = std::env::temp_dir().join("oxideav-avi-r6-cap-raised.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_super_index_capacity(raised_cap);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
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

    // Find the indx chunk header in the file. Per layout the indx
    // sits inside the first stream's strl, so it appears once.
    let mut i = 0;
    let mut indx_payload_len: Option<u32> = None;
    while i + 8 <= bytes.len() {
        if &bytes[i..i + 4] == b"indx" {
            indx_payload_len = Some(u32::from_le_bytes([
                bytes[i + 4],
                bytes[i + 5],
                bytes[i + 6],
                bytes[i + 7],
            ]));
            break;
        }
        i += 1;
    }
    let got = indx_payload_len.expect("indx chunk must be present");
    let want = 24u32 + 16u32 * raised_cap as u32;
    assert_eq!(
        got, want,
        "indx payload at raised cap = 24 + 16*{raised_cap} = {want}; got {got}"
    );
}

#[test]
fn raised_super_index_capacity_round_trips_via_demuxer() {
    // Round-6 C3: the demuxer must still parse / play back a file
    // muxed with a raised super-index capacity. The reserved tail
    // entries are zero-filled so demuxers that walk the indx will
    // skip them naturally.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 13000, 128)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-cap-roundtrip.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_super_index_capacity(1024);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
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
    let mut got = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        got.push(p.data);
    }
    assert_eq!(got, frames, "all frames must round-trip at raised capacity");
}

#[test]
fn super_index_capacity_below_min_keeps_default() {
    // Round-6 C3: sub-floor values fall back to the default 256.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(17, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r6-cap-floor.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        // 1 is well below the OPENDML_SUPER_INDEX_MIN_CAPACITY
        // floor → ignored, default 256 stays in effect.
        let opts = AviMuxOptions::new().with_super_index_capacity(1);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
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
    // Find the indx chunk and assert the default payload size.
    let mut i = 0;
    let mut indx_payload_len: Option<u32> = None;
    while i + 8 <= bytes.len() {
        if &bytes[i..i + 4] == b"indx" {
            indx_payload_len = Some(u32::from_le_bytes([
                bytes[i + 4],
                bytes[i + 5],
                bytes[i + 6],
                bytes[i + 7],
            ]));
            break;
        }
        i += 1;
    }
    let got = indx_payload_len.expect("indx chunk must be present");
    let want = 24u32 + 16u32 * OPENDML_SUPER_INDEX_DEFAULT_CAPACITY as u32;
    assert_eq!(got, want, "sub-floor capacity must fall back to default");
}
