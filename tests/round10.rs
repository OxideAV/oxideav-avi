//! Round-10 AVI feature tests.
//!
//! Covers:
//! - **C1** `xxtx` text/subtitle chunk recognition — mirror of round-8
//!   C3 (`xxpc`). The demuxer must skip `NNtx` chunks from the regular
//!   packet stream, count them per stream, surface the count via
//!   `text_chunk_count(stream)` plus the `avi:text_chunk.<n>` metadata
//!   key, and the runtime walk must pick up text chunks in idx1-less
//!   files just like the palette-change path.
//! - **C2** `VprpConfig::with_field_descs([..])` muxer override — the
//!   round-9 muxer hard-coded a PAL-flavoured `half_height + 23`
//!   second-line, which is wrong for NTSC (line 285) and any other
//!   broadcast standard with non-PAL first-line conventions. Round-10
//!   lets callers supply the eight DWORDs of each `VIDEO_FIELD_DESC`
//!   verbatim so a re-mux doesn't lie about the signal-domain offsets.
//! - **C3** `AvihFlags` typed accessor — `dwFlags` decodes into
//!   per-bit `bool`s (`AVIF_HASINDEX` / `AVIF_MUSTUSEINDEX` /
//!   `AVIF_ISINTERLEAVED` / `AVIF_TRUSTCKTYPE` / `AVIF_WASCAPTUREFILE`
//!   / `AVIF_COPYRIGHTED`) without forcing callers to re-parse the
//!   `avi:flags` hex string.

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

use oxideav_avi::demuxer::{
    open_avi as demuxer_open_avi, AvihFlags, AVIF_COPYRIGHTED, AVIF_HASINDEX, AVIF_ISINTERLEAVED,
    AVIF_MUSTUSEINDEX, AVIF_TRUSTCKTYPE, AVIF_WASCAPTUREFILE,
};
use oxideav_avi::muxer::{
    open_with_options, AviKind, AviMuxOptions, RiffSegmentLimit, VprpConfig, VprpFieldDescOverride,
};

// ---------------------------------------------------------------------------
// Test fixtures shared across round-10 cases.
// ---------------------------------------------------------------------------

fn registry_with_video_and_audio() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    // 8bpp indexed/raw; matches the BITMAPINFOHEADER below.
    reg.register(CodecInfo::new(CodecId::new("rgb8")).tag(CodecTag::fourcc(b"\x00\x00\x00\x00")));
    reg
}

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
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// C1 helper: hand-craft an AVI with `tx_count` text chunks + `data_count`
// data chunks, single video stream. Mirrors round-8's
// `craft_avi_with_palette_changes` shape so the same demuxer paths
// (idx1 scan, runtime walk) get exercised.
// ---------------------------------------------------------------------------

fn craft_avi_with_text_chunks(
    tx_count: usize,
    data_count: usize,
    avih_flags: u32,
    include_idx1: bool,
) -> Vec<u8> {
    use std::io::Cursor;

    let mut buf = Cursor::new(Vec::<u8>::new());
    let _w = &mut buf;

    // AVIMAINHEADER body (56 bytes per aviriff.h).
    let mut avih = [0u8; 56];
    avih[0..4].copy_from_slice(&40_000u32.to_le_bytes()); // 25 fps
    avih[12..16].copy_from_slice(&avih_flags.to_le_bytes());
    avih[16..20].copy_from_slice(&(data_count as u32).to_le_bytes()); // dwTotalFrames
    avih[24..28].copy_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih[32..36].copy_from_slice(&16u32.to_le_bytes()); // dwWidth
    avih[36..40].copy_from_slice(&16u32.to_le_bytes()); // dwHeight

    // strh — vids.
    let mut strh = [0u8; 56];
    strh[0..4].copy_from_slice(b"vids");
    strh[4..8].copy_from_slice(b"\0\0\0\0");
    strh[20..24].copy_from_slice(&1u32.to_le_bytes());
    strh[24..28].copy_from_slice(&25u32.to_le_bytes());
    strh[32..36].copy_from_slice(&(data_count as u32).to_le_bytes());

    // strf — minimal BITMAPINFOHEADER + 256-entry palette.
    let mut strf = Vec::with_capacity(40 + 256 * 4);
    strf.extend_from_slice(&40u32.to_le_bytes());
    strf.extend_from_slice(&16i32.to_le_bytes());
    strf.extend_from_slice(&16i32.to_le_bytes());
    strf.extend_from_slice(&1u16.to_le_bytes());
    strf.extend_from_slice(&8u16.to_le_bytes());
    strf.extend_from_slice(&0u32.to_le_bytes());
    strf.extend_from_slice(&(16u32 * 16).to_le_bytes());
    strf.extend_from_slice(&0i32.to_le_bytes());
    strf.extend_from_slice(&0i32.to_le_bytes());
    strf.extend_from_slice(&256u32.to_le_bytes());
    strf.extend_from_slice(&0u32.to_le_bytes());
    for i in 0..256u32 {
        strf.push(i as u8);
        strf.push((i >> 1) as u8);
        strf.push((i >> 2) as u8);
        strf.push(0);
    }

    let mut strl_with_form: Vec<u8> = Vec::new();
    strl_with_form.extend_from_slice(b"strl");
    strl_with_form.extend_from_slice(b"strh");
    strl_with_form.extend_from_slice(&(strh.len() as u32).to_le_bytes());
    strl_with_form.extend_from_slice(&strh);
    strl_with_form.extend_from_slice(b"strf");
    strl_with_form.extend_from_slice(&(strf.len() as u32).to_le_bytes());
    strl_with_form.extend_from_slice(&strf);

    let mut hdrl_with_form: Vec<u8> = Vec::new();
    hdrl_with_form.extend_from_slice(b"hdrl");
    hdrl_with_form.extend_from_slice(b"avih");
    hdrl_with_form.extend_from_slice(&(avih.len() as u32).to_le_bytes());
    hdrl_with_form.extend_from_slice(&avih);
    hdrl_with_form.extend_from_slice(b"LIST");
    hdrl_with_form.extend_from_slice(&(strl_with_form.len() as u32).to_le_bytes());
    hdrl_with_form.extend_from_slice(&strl_with_form);

    // movi: tx_count `00tx` chunks (8 bytes payload each) then
    // data_count `00db` chunks (32 bytes payload each).
    let mut movi_with_form: Vec<u8> = Vec::new();
    movi_with_form.extend_from_slice(b"movi");
    let mut idx_entries: Vec<([u8; 4], u32, u32, u32)> = Vec::new();
    let mut rel_off: u32 = 4;
    for _ in 0..tx_count {
        let payload = b"hello!\0\0";
        movi_with_form.extend_from_slice(b"00tx");
        movi_with_form.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        movi_with_form.extend_from_slice(payload);
        idx_entries.push((*b"00tx", 0, rel_off, payload.len() as u32));
        rel_off += 8 + payload.len() as u32;
    }
    for i in 0..data_count {
        let payload: Vec<u8> = (0..32u8).map(|b| b ^ (i as u8)).collect();
        movi_with_form.extend_from_slice(b"00db");
        movi_with_form.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        movi_with_form.extend_from_slice(&payload);
        idx_entries.push((*b"00db", 0x10, rel_off, payload.len() as u32));
        rel_off += 8 + payload.len() as u32;
    }

    let mut idx1 = Vec::with_capacity(idx_entries.len() * 16);
    for (ckid, flags, off, size) in &idx_entries {
        idx1.extend_from_slice(ckid);
        idx1.extend_from_slice(&flags.to_le_bytes());
        idx1.extend_from_slice(&off.to_le_bytes());
        idx1.extend_from_slice(&size.to_le_bytes());
    }

    let mut riff_body = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(hdrl_with_form.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&hdrl_with_form);
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(movi_with_form.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&movi_with_form);
    if include_idx1 {
        riff_body.extend_from_slice(b"idx1");
        riff_body.extend_from_slice(&(idx1.len() as u32).to_le_bytes());
        riff_body.extend_from_slice(&idx1);
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);
    out
}

// ---------------------------------------------------------------------------
// C1: xxtx recognition.
// ---------------------------------------------------------------------------

#[test]
fn xxtx_text_chunk_count_surfaces_via_accessor_and_metadata() {
    // Round-10 C1: 4 text chunks + 2 data chunks. The demuxer must
    // return 2 packets, count 4 text chunks via the typed accessor,
    // and emit `avi:text_chunk.0 = "4"` in metadata.
    let bytes = craft_avi_with_text_chunks(4, 2, AVIF_HASINDEX | AVIF_ISINTERLEAVED, true);
    let tmp = std::env::temp_dir().join("oxideav-avi-r10-xxtx-4tx-2db.avi");
    std::fs::write(&tmp, &bytes).unwrap();
    let reg = registry_with_video_and_audio();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.text_chunk_count(0), 4);
    let md = dmx.metadata();
    let text_md = md.iter().find(|(k, _)| k == "avi:text_chunk.0");
    assert_eq!(
        text_md.map(|(_, v)| v.as_str()),
        Some("4"),
        "avi:text_chunk.0 metadata key must be present with the count"
    );

    let mut pkts = 0usize;
    while let Ok(p) = dmx.next_packet() {
        assert_eq!(p.stream_index, 0);
        assert_eq!(p.data.len(), 32);
        pkts += 1;
    }
    assert_eq!(pkts, 2);
}

#[test]
fn xxtx_zero_count_emits_no_text_chunk_metadata() {
    let bytes = craft_avi_with_text_chunks(0, 3, AVIF_HASINDEX | AVIF_ISINTERLEAVED, true);
    let tmp = std::env::temp_dir().join("oxideav-avi-r10-xxtx-zero.avi");
    std::fs::write(&tmp, &bytes).unwrap();
    let reg = registry_with_video_and_audio();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.text_chunk_count(0), 0);
    let md = dmx.metadata();
    assert!(
        !md.iter().any(|(k, _)| k.starts_with("avi:text_chunk.")),
        "no avi:text_chunk.* keys allowed when zero xxtx chunks were seen"
    );
}

#[test]
fn xxtx_runtime_count_increments_when_idx1_absent() {
    // Round-10 C1: when idx1 is omitted the static scan can't see the
    // text chunks. The runtime path in `next_packet` must still bump
    // the counter as it walks past `xxtx`.
    let bytes = craft_avi_with_text_chunks(3, 2, AVIF_ISINTERLEAVED, false);
    let tmp = std::env::temp_dir().join("oxideav-avi-r10-xxtx-runtime.avi");
    std::fs::write(&tmp, &bytes).unwrap();
    let reg = registry_with_video_and_audio();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Without idx1 the static scan didn't run.
    assert_eq!(dmx.text_chunk_count(0), 0);

    let mut pkts = 0usize;
    while let Ok(p) = dmx.next_packet() {
        let _ = p.data.len();
        pkts += 1;
    }
    assert_eq!(pkts, 2, "runtime walk must still return all data chunks");
    assert_eq!(
        dmx.text_chunk_count(0),
        3,
        "runtime path must bump xxtx counter for every text chunk seen"
    );
}

#[test]
fn xxtx_unknown_stream_index_returns_zero() {
    let bytes = craft_avi_with_text_chunks(1, 1, AVIF_HASINDEX, true);
    let tmp = std::env::temp_dir().join("oxideav-avi-r10-xxtx-unknown.avi");
    std::fs::write(&tmp, &bytes).unwrap();
    let reg = registry_with_video_and_audio();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.text_chunk_count(0), 1);
    assert_eq!(dmx.text_chunk_count(1), 0);
    assert_eq!(dmx.text_chunk_count(99), 0);
}

// ---------------------------------------------------------------------------
// C2: VprpConfig::with_field_descs([..]) muxer override.
// ---------------------------------------------------------------------------

#[test]
fn vprp_with_field_descs_ntsc_first_line_offsets_round_trip() {
    // Round-10 C2: NTSC bottom field starts at line 285 (= 263 + 22),
    // not PAL's 335 (= 312 + 23). Supply a hand-rolled override and
    // verify the demuxer reads back the requested first-line values.
    let stream = magicyuv_stream(720, 480);
    let payload = synth_payload(73, 64);
    let reg = registry_with_magicyuv();

    let descs = vec![
        VprpFieldDescOverride {
            compressed_bm_height: 240,
            compressed_bm_width: 720,
            valid_bm_height: 240,
            valid_bm_width: 720,
            valid_bm_x_offset: 0,
            valid_bm_y_offset: 0,
            video_x_offset_in_t: 0,
            video_y_valid_start_line: 23,
        },
        VprpFieldDescOverride {
            compressed_bm_height: 240,
            compressed_bm_width: 720,
            valid_bm_height: 240,
            valid_bm_width: 720,
            valid_bm_x_offset: 0,
            valid_bm_y_offset: 0,
            video_x_offset_in_t: 0,
            video_y_valid_start_line: 285,
        },
    ];

    let tmp = std::env::temp_dir().join("oxideav-avi-r10-vprp-ntsc-override.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts =
            AviMuxOptions::new().with_vprp(0, VprpConfig::ntsc().with_field_descs(descs.clone()));
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let parsed = dmx.vprp_field_descs(0);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].video_y_valid_start_line, 23, "NTSC top-field");
    assert_eq!(
        parsed[1].video_y_valid_start_line, 285,
        "NTSC bottom-field starts at line 285, not PAL's 335"
    );
    assert_eq!(parsed[0].compressed_bm_height, 240);
    assert_eq!(parsed[1].compressed_bm_height, 240);
    assert_eq!(parsed[0].compressed_bm_width, 720);
    assert_eq!(parsed[1].compressed_bm_width, 720);
}

#[test]
fn vprp_with_field_descs_too_short_falls_back_to_synthesised_default() {
    // Round-10 C2: a Vec shorter than `nbFieldPerFrame.max(1)` is
    // ignored so a partial override doesn't silently truncate the
    // array. Supply 1 record for a 2-field config — the muxer must
    // emit the synthesised default (PAL-flavoured, half_height + 23).
    let stream = magicyuv_stream(720, 576);
    let payload = synth_payload(83, 64);
    let reg = registry_with_magicyuv();

    let descs = vec![VprpFieldDescOverride {
        compressed_bm_height: 99,
        compressed_bm_width: 99,
        valid_bm_height: 99,
        valid_bm_width: 99,
        valid_bm_x_offset: 0,
        valid_bm_y_offset: 0,
        video_x_offset_in_t: 0,
        video_y_valid_start_line: 999,
    }];

    let tmp = std::env::temp_dir().join("oxideav-avi-r10-vprp-short-override.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_vprp(0, VprpConfig::pal().with_field_descs(descs));
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let parsed = dmx.vprp_field_descs(0);
    assert_eq!(parsed.len(), 2, "PAL nbFieldPerFrame=2 → two records");
    assert_eq!(
        parsed[0].compressed_bm_height, 288,
        "synthesised default = half_height = 288 (not the override's 99)"
    );
    assert_eq!(parsed[0].video_y_valid_start_line, 23);
    assert_eq!(
        parsed[1].video_y_valid_start_line, 311,
        "synthesised PAL default = half_height + 23 = 311"
    );
}

#[test]
fn vprp_with_field_descs_progressive_uses_first_record_only() {
    // Round-10 C2: progressive (nb_field_per_frame=1) only consumes
    // the first record; trailing override records are ignored.
    let stream = magicyuv_stream(640, 480);
    let payload = synth_payload(91, 64);
    let reg = registry_with_magicyuv();

    let descs = vec![
        VprpFieldDescOverride {
            compressed_bm_height: 480,
            compressed_bm_width: 640,
            valid_bm_height: 480,
            valid_bm_width: 640,
            valid_bm_x_offset: 0,
            valid_bm_y_offset: 0,
            video_x_offset_in_t: 0,
            video_y_valid_start_line: 42,
        },
        // Sentinel: never written — progressive only emits one record.
        VprpFieldDescOverride {
            compressed_bm_height: 0xDEAD_BEEF,
            compressed_bm_width: 0xDEAD_BEEF,
            valid_bm_height: 0xDEAD_BEEF,
            valid_bm_width: 0xDEAD_BEEF,
            valid_bm_x_offset: 0,
            valid_bm_y_offset: 0,
            video_x_offset_in_t: 0,
            video_y_valid_start_line: 0xDEAD_BEEF,
        },
    ];

    let tmp = std::env::temp_dir().join("oxideav-avi-r10-vprp-progressive-override.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_vprp(
            0,
            VprpConfig::default()
                .with_nb_field_per_frame(1)
                .with_field_descs(descs),
        );
        let mut mux = open_with_options(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let parsed = dmx.vprp_field_descs(0);
    assert_eq!(parsed.len(), 1, "progressive → single record");
    assert_eq!(parsed[0].video_y_valid_start_line, 42);
    assert_eq!(parsed[0].compressed_bm_height, 480);
}

// ---------------------------------------------------------------------------
// C3: AvihFlags typed accessor.
// ---------------------------------------------------------------------------

#[test]
fn avih_flags_decodes_each_documented_bit() {
    let raw = AVIF_HASINDEX
        | AVIF_MUSTUSEINDEX
        | AVIF_ISINTERLEAVED
        | AVIF_TRUSTCKTYPE
        | AVIF_WASCAPTUREFILE
        | AVIF_COPYRIGHTED;
    let flags = AvihFlags::from_bits(raw);
    assert!(flags.has_index);
    assert!(flags.must_use_index);
    assert!(flags.is_interleaved);
    assert!(flags.trust_ck_type);
    assert!(flags.was_capture_file);
    assert!(flags.copyrighted);
    assert_eq!(flags.bits, raw);
}

#[test]
fn avih_flags_zero_yields_all_false() {
    let f = AvihFlags::from_bits(0);
    assert!(!f.has_index);
    assert!(!f.must_use_index);
    assert!(!f.is_interleaved);
    assert!(!f.trust_ck_type);
    assert!(!f.was_capture_file);
    assert!(!f.copyrighted);
    assert_eq!(f.bits, 0);
}

#[test]
fn avih_flags_preserves_undocumented_bits_in_raw() {
    // Vendor-extension / future-spec bits — exposed verbatim via `bits`
    // even though no boolean field decodes them.
    let raw = AVIF_HASINDEX | 0x8000_0000;
    let f = AvihFlags::from_bits(raw);
    assert!(f.has_index);
    assert_eq!(f.bits, raw);
    assert!(!f.copyrighted);
}

#[test]
fn avih_flags_accessor_round_trips_against_demuxer_avih() {
    // Round-10 C3: the typed accessor must reflect the actual avih
    // flags parsed from a real AVI file. Use a crafted file with a
    // non-trivial flags pattern (HASINDEX | ISINTERLEAVED | TRUSTCKTYPE
    // | WASCAPTUREFILE) and verify each bit decodes correctly.
    let raw = AVIF_HASINDEX | AVIF_ISINTERLEAVED | AVIF_TRUSTCKTYPE | AVIF_WASCAPTUREFILE;
    let bytes = craft_avi_with_text_chunks(0, 1, raw, true);
    let tmp = std::env::temp_dir().join("oxideav-avi-r10-avih-flags-roundtrip.avi");
    std::fs::write(&tmp, &bytes).unwrap();
    let reg = registry_with_video_and_audio();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let flags = dmx.avih_flags();
    assert!(flags.has_index);
    assert!(!flags.must_use_index);
    assert!(flags.is_interleaved);
    assert!(flags.trust_ck_type);
    assert!(flags.was_capture_file);
    assert!(!flags.copyrighted);
    assert_eq!(flags.bits, raw);

    // Cross-check against the existing hex-string metadata key.
    let md = dmx.metadata();
    let hex = md
        .iter()
        .find(|(k, _)| k == "avi:flags")
        .map(|(_, v)| v.clone());
    assert_eq!(hex.as_deref(), Some(format!("0x{raw:08X}").as_str()));
}

// ---------------------------------------------------------------------------
// Cross-feature: keep the `Read`/`Seek`/`Write` imports referenced so
// dead-code warnings don't fire when the only use is inside helper
// functions (cargo's per-import lint is module-scoped).
// ---------------------------------------------------------------------------

#[test]
fn round10_helpers_are_used() {
    let mut buf = std::io::Cursor::new(Vec::<u8>::new());
    buf.write_all(b"ok").unwrap();
    buf.seek(std::io::SeekFrom::Start(0)).unwrap();
    let mut s = String::new();
    let _ = buf.read_to_string(&mut s);
    assert_eq!(s, "ok");
}
