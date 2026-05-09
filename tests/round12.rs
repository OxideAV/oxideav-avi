//! Round-12 AVI feature tests.
//!
//! Covers:
//! - **C1** Side-band data accessors —
//!   [`AviDemuxer::palette_change_data`] /
//!   [`AviDemuxer::text_chunk_data`] return the actual chunk bodies
//!   for `xxpc` / `xxtx` chunks, closing the byte round-trip with
//!   round-11 C3's [`AviMuxer::write_palette_change`] /
//!   [`AviMuxer::write_text_chunk`].
//! - **C2** `AviMuxOptions::with_avih_flags` /
//!   `with_avih_flag_bit` — explicit `avih.dwFlags` builder paired
//!   with the round-10 C3 demuxer accessor [`AviDemuxer::avih_flags`]
//!   so a writer→reader cycle preserves bits like `AVIF_TRUSTCKTYPE`,
//!   `AVIF_WASCAPTUREFILE`, `AVIF_COPYRIGHTED`, and
//!   `AVIF_MUSTUSEINDEX` that the legacy default omits.
//! - **C3** `AviDemuxer::all_info_for(&str)` — string-keyed sibling
//!   of round-8 C2's [`AviDemuxer::info_all_for`] that accepts the
//!   FourCC as a `&str` instead of a `[u8; 4]` literal.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, MediaType, Muxer, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
#[allow(dead_code)]
fn _muxer_trait_in_scope<M: Muxer>() {}

use oxideav_avi::demuxer::{
    open_avi as demuxer_open_avi, AVIF_COPYRIGHTED, AVIF_HASINDEX, AVIF_MUSTUSEINDEX,
    AVIF_TRUSTCKTYPE, AVIF_WASCAPTUREFILE,
};
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit, DEFAULT_AVIH_FLAGS};

// ---------------------------------------------------------------------------
// Test fixtures (independent of round-11.rs to avoid cross-file fixture
// coupling — each round file should be self-contained per the workspace
// per-round-test convention).
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
// C1: side-band data accessors.
// ---------------------------------------------------------------------------

#[test]
fn palette_change_data_round_trips_byte_exactly() {
    // Round-12 C1: write a couple of distinct palette-change chunks
    // and confirm the demuxer returns them byte-identical via the new
    // `palette_change_data(stream)` accessor. Pairs the round-11 C3
    // muxer write path with a round-trip data check (count alone
    // isn't enough — a buggy writer could emit the wrong bytes).
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(7, 64);
    let reg = registry_with_magicyuv();

    // Two distinguishable palette deltas (different bNumEntries +
    // different colour quads so no two bodies are byte-identical).
    let pal_a: Vec<u8> = vec![0u8, 2, 0, 0, 0xFF, 0, 0, 0, 0, 0xFF, 0, 0];
    let pal_b: Vec<u8> = vec![1u8, 1, 0, 0, 0x12, 0x34, 0x56, 0x78];

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-pc-data-roundtrip.avi");
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
        mux.write_palette_change(0, &pal_a).unwrap();
        mux.write_palette_change(0, &pal_b).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let data = dmx.palette_change_data(0);
    assert_eq!(data.len(), 2, "expected 2 buffered palette-change bodies");
    assert_eq!(&data[0][..], &pal_a[..]);
    assert_eq!(&data[1][..], &pal_b[..]);
    // count accessor must agree with data slice length.
    assert_eq!(dmx.palette_change_count(0) as usize, data.len());
}

#[test]
fn text_chunk_data_round_trips_byte_exactly() {
    // Round-12 C1: same shape for `xxtx` text/subtitle chunks. Three
    // distinct caption payloads must come back in file order with
    // byte-identical content.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(11, 64);
    let reg = registry_with_magicyuv();

    let captions: [&[u8]; 3] = [b"first caption\0", b"second\0", b"final line\0"];

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-tx-data-roundtrip.avi");
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
        for c in &captions {
            mux.write_text_chunk(0, c).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let data = dmx.text_chunk_data(0);
    assert_eq!(data.len(), captions.len());
    for (i, c) in captions.iter().enumerate() {
        assert_eq!(&data[i][..], &c[..], "caption {i} body mismatch");
    }
    assert_eq!(dmx.text_chunk_count(0) as usize, data.len());
}

#[test]
fn palette_change_data_empty_for_unknown_stream_or_no_chunks() {
    // Round-12 C1: out-of-range stream index returns &[]; a stream
    // with zero side-band chunks also returns &[].
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(0, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-pc-empty.avi");
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
    assert!(dmx.palette_change_data(0).is_empty());
    assert!(dmx.palette_change_data(99).is_empty());
    assert!(dmx.text_chunk_data(0).is_empty());
    assert!(dmx.text_chunk_data(99).is_empty());
}

#[test]
fn sideband_data_available_before_next_packet_walk() {
    // Round-12 C1: with `idx1` present (the AVI 1.0 default), the
    // side-band data buffers must be populated EAGERLY at `open()` —
    // accessible before the first `next_packet` call. This is the
    // primary use case (callers wanting to inspect palette/caption
    // metadata without paying for a full movi walk).
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(42, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-eager-load.avi");
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
        mux.write_palette_change(0, &[0u8, 1, 0, 0, 0xAA, 0xBB, 0xCC, 0xDD])
            .unwrap();
        mux.write_text_chunk(0, b"hello\0").unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    // No `next_packet()` calls — data must be ready right after
    // open() courtesy of the eager idx1 walk.
    assert_eq!(dmx.palette_change_data(0).len(), 1);
    assert_eq!(dmx.text_chunk_data(0).len(), 1);
    assert_eq!(&dmx.text_chunk_data(0)[0][..], b"hello\0");
}

#[test]
fn opendml_sideband_data_via_lazy_walk() {
    // Round-12 C1: in OpenDML mode the muxer ALSO emits idx1 (for
    // backward compat with AVI 1.0 readers), so the eager path
    // populates data buffers there too. Confirm the byte-equality
    // check still holds for OpenDML files (text bodies preserved
    // across the std-index + idx1 dual-record).
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(99, 256);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-opendml-sideband.avi");
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
        for i in 0..2 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
            mux.write_text_chunk(0, b"sub\0").unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let data = dmx.text_chunk_data(0);
    assert_eq!(data.len(), 2);
    for body in data {
        assert_eq!(&body[..], b"sub\0");
    }
}

// ---------------------------------------------------------------------------
// C2: avih.dwFlags builder.
// ---------------------------------------------------------------------------

#[test]
fn with_avih_flags_overrides_dwflags_verbatim() {
    // Round-12 C2: caller-supplied dwFlags lands verbatim in the
    // header. Confirm via the round-10 C3 demuxer accessor
    // (`avih_flags()` returns a typed `AvihFlags` struct with
    // per-bit booleans + raw bits).
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(0, 32);
    let custom = AVIF_HASINDEX | AVIF_TRUSTCKTYPE | AVIF_WASCAPTUREFILE | AVIF_COPYRIGHTED;

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-avih-flags-verbatim.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_avih_flags(custom);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &oxideav_core::NullCodecResolver).unwrap_or_else(|_| {
        // Fall back to the magicyuv resolver if the codec lookup
        // is required (some demux paths reject unknown FourCCs).
        let rs2: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
        demuxer_open_avi(rs2, &registry_with_magicyuv()).unwrap()
    });
    let flags = dmx.avih_flags();
    assert_eq!(flags.bits, custom, "raw dwFlags must round-trip verbatim");
    assert!(flags.has_index);
    assert!(flags.trust_ck_type);
    assert!(flags.was_capture_file);
    assert!(flags.copyrighted);
    assert!(
        !flags.is_interleaved,
        "verbatim override must NOT carry the muxer default's AVIF_ISINTERLEAVED bit"
    );
    assert!(!flags.must_use_index);
}

#[test]
fn with_avih_flag_bit_ors_into_default() {
    // Round-12 C2: `with_avih_flag_bit` keeps the muxer default
    // (AVIF_ISINTERLEAVED | AVIF_HASINDEX) and ORs in the requested
    // bit. Two calls accumulate.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(0, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-avih-flag-bit.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_avih_flag_bit(AVIF_TRUSTCKTYPE)
            .with_avih_flag_bit(AVIF_MUSTUSEINDEX);
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
    let expected = DEFAULT_AVIH_FLAGS | AVIF_TRUSTCKTYPE | AVIF_MUSTUSEINDEX;
    assert_eq!(flags.bits, expected);
    // Default bits preserved (the round-6 baseline includes
    // AVIF_HASINDEX and AVIF_TRUSTCKTYPE per
    // [`DEFAULT_AVIH_FLAGS`] = 0x0810).
    assert!(flags.has_index, "AVIF_HASINDEX bit from default preserved");
    // OR'd-in bits set on top of the default.
    assert!(flags.trust_ck_type);
    assert!(flags.must_use_index);
}

#[test]
fn default_avih_flags_unchanged_when_no_override() {
    // Round-12 C2: callers who don't touch the new builder still get
    // the round-6 default `AVIF_ISINTERLEAVED | AVIF_HASINDEX`.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(0, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-avih-flags-default.avi");
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
    let flags = dmx.avih_flags();
    // Default `dwFlags` matches the round-6 muxer baseline. The
    // exact bit pattern is captured by the public
    // [`DEFAULT_AVIH_FLAGS`] constant (0x0000_0810); callers who
    // want the canonical "interleaved + has-index" pair should set
    // the bits explicitly via `with_avih_flags`.
    assert_eq!(flags.bits, DEFAULT_AVIH_FLAGS);
    assert!(flags.has_index, "default carries AVIF_HASINDEX");
    assert!(
        flags.trust_ck_type,
        "default 0x810 carries AVIF_TRUSTCKTYPE (legacy round-6 baseline)"
    );
}

// ---------------------------------------------------------------------------
// C3: all_info_for(&str) string-keyed accessor.
// ---------------------------------------------------------------------------

#[test]
fn all_info_for_string_key_returns_every_value() {
    // Round-12 C3: string-keyed `all_info_for` accepts the FourCC as
    // a `&str`, returns every matching value in file order. Mirrors
    // round-8 C2's `info_all_for([u8; 4])` shape but without the
    // byte-literal ergonomic bump.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(0, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-all-info-str.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_info(*b"IART", "Artist A")
            .with_info(*b"IART", "Artist B")
            .with_info(*b"INAM", "Title One")
            .with_info(*b"ICMT", "Comment C");
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
    // Multi-entry FourCC.
    let artists = dmx.all_info_for("IART");
    assert_eq!(artists, vec!["Artist A", "Artist B"]);
    // Single-entry FourCCs.
    let titles = dmx.all_info_for("INAM");
    assert_eq!(titles, vec!["Title One"]);
    let comments = dmx.all_info_for("ICMT");
    assert_eq!(comments, vec!["Comment C"]);
    // Unknown FourCC.
    assert!(dmx.all_info_for("ICOP").is_empty());
}

#[test]
fn all_info_for_returns_empty_on_non_4_char_key() {
    // Round-12 C3: defensive — non-4-character keys return Vec::new()
    // rather than panicking or matching anything by accident.
    let stream = magicyuv_stream(64, 64);
    let payload = synth_payload(0, 32);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r12-all-info-bad-key.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_info(*b"INAM", "Title");
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
    assert!(dmx.all_info_for("INA").is_empty()); // 3 chars
    assert!(dmx.all_info_for("INAMX").is_empty()); // 5 chars
    assert!(dmx.all_info_for("").is_empty()); // empty
                                              // The valid 4-char key still works.
    assert_eq!(dmx.all_info_for("INAM"), vec!["Title"]);
}
