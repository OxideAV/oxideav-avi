//! Round-13 AVI feature tests.
//!
//! Covers:
//! - **C1** Typed `xxpc` palette-change round-trip —
//!   [`crate::demuxer::PaletteChange`] / [`crate::demuxer::PaletteEntry`]
//!   typed structs + [`AviDemuxer::palette_change_typed`] decode +
//!   [`AviMuxer::with_palette_change_typed`] encode. Pairs the typed
//!   muxer write with the typed demuxer read so callers don't have to
//!   hand-pack the AVI 1.0 `BITMAPINFO`-style palette delta.
//! - **C2** `avih.dwSuggestedBufferSize` populator — auto-computed
//!   max-chunk-body hint per AVI 1.0 §3.1, plus
//!   [`AviMuxOptions::with_suggested_buffer_size`] override and
//!   [`AviDemuxer::avih_suggested_buffer_size`] readback.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{
    open_avi as demuxer_open_avi, PaletteChange, PaletteEntry, AVIF_COPYRIGHTED, AVIF_HASINDEX,
    AVIF_ISINTERLEAVED, AVIF_MUSTUSEINDEX, AVIF_TRUSTCKTYPE, AVIF_WASCAPTUREFILE,
};
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, DEFAULT_AVIH_FLAGS};

// ---------------------------------------------------------------------------
// Test fixtures (self-contained per the workspace per-round-test convention).
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
// C1: typed PaletteChange round-trip.
// ---------------------------------------------------------------------------

#[test]
fn palette_change_typed_round_trips_byte_exactly() {
    // Round-13 C1: build a typed `PaletteChange`, write it via the
    // muxer's typed helper, and confirm both the byte body and the
    // typed decode round-trip identically.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(7, 64);
    let reg = registry_with_magicyuv();

    let change_a = PaletteChange {
        first_entry: 0,
        num_entries: 2,
        flags: 0,
        entries: vec![
            PaletteEntry {
                red: 0xFF,
                green: 0x00,
                blue: 0x00,
                flags: 0,
            },
            PaletteEntry {
                red: 0x00,
                green: 0xFF,
                blue: 0x00,
                flags: 0,
            },
        ],
    };
    let change_b = PaletteChange {
        first_entry: 16,
        num_entries: 1,
        flags: 0x0001,
        entries: vec![PaletteEntry {
            red: 0x12,
            green: 0x34,
            blue: 0x56,
            flags: 0x80,
        }],
    };

    let tmp = std::env::temp_dir().join("oxideav-avi-r13-pc-typed-roundtrip.avi");
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
        mux.with_palette_change_typed(0, &change_a).unwrap();
        mux.with_palette_change_typed(0, &change_b).unwrap();
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let typed = dmx.palette_change_typed(0);
    assert_eq!(typed.len(), 2);
    assert_eq!(typed[0], change_a);
    assert_eq!(typed[1], change_b);

    // Sanity: raw bodies still match the to_bytes() encoding.
    let raw = dmx.palette_change_data(0);
    assert_eq!(raw.len(), 2);
    assert_eq!(&raw[0][..], change_a.to_bytes().as_slice());
    assert_eq!(&raw[1][..], change_b.to_bytes().as_slice());
}

#[test]
fn palette_change_parse_rejects_short_or_misaligned() {
    // Round-13 C1: the typed parser returns None for bodies shorter
    // than the 4-byte fixed header, and for bodies with a trailing
    // PALETTEENTRY array length that isn't a multiple of 4.
    assert!(PaletteChange::parse(&[]).is_none());
    assert!(PaletteChange::parse(&[0, 0, 0]).is_none());
    // 4-byte header + 5-byte tail (not divisible by 4).
    assert!(PaletteChange::parse(&[0, 1, 0, 0, 1, 2, 3, 4, 5]).is_none());
    // 4-byte header alone (zero entries) is valid.
    let zero = PaletteChange::parse(&[3, 0, 0, 0]).unwrap();
    assert_eq!(zero.first_entry, 3);
    assert_eq!(zero.num_entries, 0);
    assert_eq!(zero.flags, 0);
    assert!(zero.entries.is_empty());
}

#[test]
fn palette_change_typed_empty_for_unknown_stream() {
    // Round-13 C1: out-of-range stream index returns an empty Vec.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(0, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r13-pc-typed-empty.avi");
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
    assert!(dmx.palette_change_typed(0).is_empty());
    assert!(dmx.palette_change_typed(99).is_empty());
}

#[test]
fn palette_change_to_bytes_matches_avi10_layout() {
    // Round-13 C1: spot-check the on-wire layout of `to_bytes`
    // against the AVI 1.0 / vfw.h spec
    // (BYTE first / BYTE num / WORD flags / PALETTEENTRY[]).
    let pc = PaletteChange {
        first_entry: 0x42,
        num_entries: 2,
        flags: 0xBEEF,
        entries: vec![
            PaletteEntry {
                red: 0x11,
                green: 0x22,
                blue: 0x33,
                flags: 0x44,
            },
            PaletteEntry {
                red: 0x55,
                green: 0x66,
                blue: 0x77,
                flags: 0x88,
            },
        ],
    };
    let bytes = pc.to_bytes();
    assert_eq!(
        bytes,
        vec![
            0x42, 0x02, 0xEF, 0xBE, // header (LE flags)
            0x11, 0x22, 0x33, 0x44, // entry 0
            0x55, 0x66, 0x77, 0x88, // entry 1
        ]
    );
    // Round-trip via parse() reproduces the typed struct.
    let parsed = PaletteChange::parse(&bytes).unwrap();
    assert_eq!(parsed, pc);
}

// ---------------------------------------------------------------------------
// C2: avih.dwSuggestedBufferSize populator.
// ---------------------------------------------------------------------------

#[test]
fn suggested_buffer_size_is_max_packet_body_rounded_up() {
    // Round-13 C2: the muxer must populate avih.dwSuggestedBufferSize
    // with the largest packet body it saw, rounded up to the next
    // 4-byte boundary. With a 4096-byte peak the value should be
    // 4096 verbatim (already 4-aligned).
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();
    let small = synth_payload(1, 1024);
    let large = synth_payload(2, 4096);

    let tmp = std::env::temp_dir().join("oxideav-avi-r13-suggested-bufsize-auto.avi");
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
        let mut pkt0 = Packet::new(0, stream.time_base, small.clone());
        pkt0.pts = Some(0);
        pkt0.flags.keyframe = true;
        mux.write_packet(&pkt0).unwrap();
        let mut pkt1 = Packet::new(0, stream.time_base, large.clone());
        pkt1.pts = Some(1);
        pkt1.flags.keyframe = true;
        mux.write_packet(&pkt1).unwrap();
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let bufsize = dmx.avih_suggested_buffer_size();
    assert!(
        bufsize >= 4096,
        "avih.dwSuggestedBufferSize must be at least the peak packet body (got {bufsize})"
    );
    // 4-byte alignment property: muxer rounds up to the next multiple
    // of 4 to leave headroom for an aligned read into a SIMD buffer.
    assert_eq!(bufsize & 3, 0, "computed value must be 4-byte aligned");
}

#[test]
fn suggested_buffer_size_rounds_unaligned_peak_up_to_4() {
    // Round-13 C2: a 1023-byte peak (not 4-aligned) should round up
    // to 1024.
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();
    let payload = synth_payload(3, 1023);

    let tmp = std::env::temp_dir().join("oxideav-avi-r13-suggested-bufsize-pad.avi");
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
    let bufsize = dmx.avih_suggested_buffer_size();
    assert_eq!(
        bufsize, 1024,
        "1023-byte peak must round up to 1024-byte 4-aligned hint"
    );
}

#[test]
fn suggested_buffer_size_honours_explicit_override() {
    // Round-13 C2: callers passing `with_suggested_buffer_size(n)`
    // get exactly `n` stamped — the auto-computed peak is ignored.
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();
    let payload = synth_payload(4, 512);
    let want: u32 = 65_536;

    let tmp = std::env::temp_dir().join("oxideav-avi-r13-suggested-bufsize-override.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_suggested_buffer_size(want);
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
    assert_eq!(
        dmx.avih_suggested_buffer_size(),
        want,
        "explicit override must land verbatim"
    );
}

#[test]
fn suggested_buffer_size_zero_when_no_packets_written() {
    // Round-13 C2: a muxer that wrote zero packets has no observed
    // peak; the auto-computed value should be 0 (the legacy round-12
    // behaviour). Verifies the populator never panics on the empty
    // case and remains backwards-compatible.
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r13-suggested-bufsize-empty.avi");
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
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.avih_suggested_buffer_size(), 0);
}

#[test]
fn suggested_buffer_size_metadata_key_matches_typed_accessor() {
    // Round-13 C2: the existing `avi:suggested_buffer_size` metadata
    // key must report the same value as the new typed accessor.
    let stream = magicyuv_stream(64, 64);
    let reg = registry_with_magicyuv();
    let payload = synth_payload(5, 2048);

    let tmp = std::env::temp_dir().join("oxideav-avi-r13-suggested-bufsize-md.avi");
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
    let typed = dmx.avih_suggested_buffer_size();
    assert!(typed >= 2048);
    let md = dmx.metadata();
    let md_str = md
        .iter()
        .find(|(k, _)| k == "avi:suggested_buffer_size")
        .map(|(_, v)| v.clone())
        .expect("avi:suggested_buffer_size key must be present when value is non-zero");
    assert_eq!(md_str, typed.to_string());
}

// ---------------------------------------------------------------------------
// C3: AVIF_* named per-bit muxer builders.
// ---------------------------------------------------------------------------

#[test]
fn avih_named_flag_builders_or_into_default() {
    // Round-13 C3: each named builder ORs its bit on top of the
    // running flags value (default = `DEFAULT_AVIH_FLAGS`). All six
    // named bits set produces a value with every documented AVIF_*
    // bit ORed into the baseline.
    let opts = AviMuxOptions::new()
        .with_has_index(true)
        .with_must_use_index(true)
        .with_is_interleaved(true)
        .with_trust_ck_type(true)
        .with_was_capture_file(true)
        .with_copyrighted(true);
    let want = DEFAULT_AVIH_FLAGS
        | AVIF_HASINDEX
        | AVIF_MUSTUSEINDEX
        | AVIF_ISINTERLEAVED
        | AVIF_TRUSTCKTYPE
        | AVIF_WASCAPTUREFILE
        | AVIF_COPYRIGHTED;
    assert_eq!(opts.avih_flags_override, Some(want));
}

#[test]
fn avih_named_flag_builders_can_mask_off_bits() {
    // Round-13 C3: passing `false` masks the bit out so callers can
    // strip a default-on bit (e.g. AVIF_TRUSTCKTYPE is in the
    // baseline) without having to recompute the raw u32.
    let opts = AviMuxOptions::new().with_trust_ck_type(false);
    let bits = opts.avih_flags_override.expect("override must be set");
    assert_eq!(
        bits & AVIF_TRUSTCKTYPE,
        0,
        "with_trust_ck_type(false) must mask AVIF_TRUSTCKTYPE off"
    );
    // AVIF_HASINDEX (also in DEFAULT_AVIH_FLAGS) must remain set.
    assert_ne!(bits & AVIF_HASINDEX, 0);
}

#[test]
fn avih_named_builders_round_trip_through_demuxer() {
    // Round-13 C3: a writer→reader cycle preserves the flag bits
    // selected via the named builders. Pairs the round-12 C2 typed
    // accessor with the round-13 fluent setter.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(13, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r13-named-flags-roundtrip.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_was_capture_file(true)
            .with_copyrighted(true)
            .with_must_use_index(true);
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
    let flags = dmx.avih_flags();
    assert!(flags.was_capture_file);
    assert!(flags.copyrighted);
    assert!(flags.must_use_index);
    // baseline `AVIF_HASINDEX | AVIF_TRUSTCKTYPE` survives.
    assert!(flags.has_index);
    assert!(flags.trust_ck_type);
}
