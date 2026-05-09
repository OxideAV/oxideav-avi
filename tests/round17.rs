//! Round-17 AVI feature tests.
//!
//! Covers:
//! - **C3** Typed `Idx1Flags` decode of one idx1 entry's `dwFlags`
//!   DWORD with public `AVIIF_LIST` / `AVIIF_KEYFRAME` /
//!   `AVIIF_FIRSTPART` / `AVIIF_LASTPART` / `AVIIF_NO_TIME` /
//!   `AVIIF_COMPRESSOR` constants and a typed
//!   `idx1_typed_flags_for_packet` accessor on `AviDemuxer`.
//! - **C4** idx1 ↔ ix## cross-validator: when a file carries both
//!   indexes and they disagree on a packet's (offset, size), the
//!   demuxer surfaces `avi:idx1.<n>.divergent_offsets` metadata.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{
    open_avi as demuxer_open_avi, Idx1Flags, AVIIF_COMPRESSOR, AVIIF_FIRSTPART, AVIIF_KEYFRAME,
    AVIIF_LASTPART, AVIIF_LIST, AVIIF_NO_TIME,
};
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

// ---------------------------------------------------------------------------
// Test fixtures.
// ---------------------------------------------------------------------------

fn registry_with_video() -> CodecRegistry {
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

fn synth_payload(seed: u32, len: usize) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// C3: AVIIF_* flag accessors.
// ---------------------------------------------------------------------------

#[test]
fn aviif_constants_match_vfw_h_values() {
    // Round-17 C3: the public AVIIF_* constants must equal the
    // values from Microsoft `vfw.h` so callers comparing against
    // raw idx1 bytes (e.g. fuzzers, byte-level conformance suites)
    // can use the crate's constants instead of magic literals.
    assert_eq!(AVIIF_LIST, 0x0000_0001);
    assert_eq!(AVIIF_KEYFRAME, 0x0000_0010);
    assert_eq!(AVIIF_FIRSTPART, 0x0000_0020);
    assert_eq!(AVIIF_LASTPART, 0x0000_0040);
    assert_eq!(AVIIF_NO_TIME, 0x0000_0100);
    assert_eq!(AVIIF_COMPRESSOR, 0x0FFF_0000);
}

#[test]
fn idx1_flags_from_bits_decodes_each_field() {
    // Round-17 C3: round-trip the typed decoder for every documented
    // bit set in isolation, then a few combinations + the raw bits
    // passthrough.
    let f = Idx1Flags::from_bits(0);
    assert!(!f.is_list && !f.is_keyframe && !f.is_first_part && !f.is_last_part && !f.is_no_time);
    assert_eq!(f.bits, 0);
    assert_eq!(f.compressor_bits(), 0);

    let f = Idx1Flags::from_bits(AVIIF_LIST);
    assert!(f.is_list);
    assert!(!f.is_keyframe);

    let f = Idx1Flags::from_bits(AVIIF_KEYFRAME);
    assert!(f.is_keyframe);
    assert!(!f.is_list);

    let f = Idx1Flags::from_bits(AVIIF_FIRSTPART);
    assert!(f.is_first_part);

    let f = Idx1Flags::from_bits(AVIIF_LASTPART);
    assert!(f.is_last_part);

    let f = Idx1Flags::from_bits(AVIIF_NO_TIME);
    assert!(f.is_no_time);

    // Combination — keyframe + 2-field stamp (FIRSTPART | LASTPART)
    // — like the muxer emits for 2-field interlaced streams.
    let combo = AVIIF_KEYFRAME | AVIIF_FIRSTPART | AVIIF_LASTPART;
    let f = Idx1Flags::from_bits(combo);
    assert!(f.is_keyframe && f.is_first_part && f.is_last_part);
    assert!(!f.is_list && !f.is_no_time);
    assert_eq!(f.bits, combo);

    // Compressor-private bits in the upper half are passed through
    // unchanged via `compressor_bits()`.
    let priv_bits = 0x0042_0000u32;
    let f = Idx1Flags::from_bits(priv_bits | AVIIF_KEYFRAME);
    assert_eq!(f.compressor_bits(), priv_bits);
    assert!(f.is_keyframe);
    assert_eq!(f.bits, priv_bits | AVIIF_KEYFRAME);

    // Vendor-extension bits OUTSIDE every documented mask are still
    // preserved verbatim in `bits`.
    let weird = 0x0000_0200u32; // not in any documented AVIIF_*
    let f = Idx1Flags::from_bits(weird);
    assert!(!f.is_list && !f.is_keyframe && !f.is_first_part && !f.is_last_part && !f.is_no_time);
    assert_eq!(f.bits, weird);
}

#[test]
fn idx1_typed_flags_for_packet_round_trips_keyframe() {
    // Round-17 C3: build a tiny AVI 1.0 file with 4 keyframes,
    // demux it, and check the typed accessor surfaces
    // `is_keyframe = true` on every packet.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth_payload(i + 17000, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r17-typed-flags.avi");
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
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    for seq in 0..4 {
        let f = dmx
            .idx1_typed_flags_for_packet(0, seq)
            .unwrap_or_else(|| panic!("missing typed flags for seq {seq}"));
        assert!(f.is_keyframe, "seq {seq} must be a keyframe");
        // Untyped accessor stays in lockstep.
        let raw = dmx
            .idx1_flags_for_packet(0, seq)
            .expect("untyped flags present");
        assert_eq!(f.bits, raw);
    }

    // Out-of-range seq returns None.
    assert!(dmx.idx1_typed_flags_for_packet(0, 999).is_none());
    // Unknown stream returns None.
    assert!(dmx.idx1_typed_flags_for_packet(99, 0).is_none());
}

// ---------------------------------------------------------------------------
// C4: idx1 ↔ ix## cross-validator.
// ---------------------------------------------------------------------------

/// Find every top-level chunk header `tag` in `bytes` and return
/// the byte offset of its 4-byte FourCC. Used to locate idx1 + ix##
/// chunk bodies for in-place mutation.
fn find_chunk_offsets(bytes: &[u8], tag: &[u8; 4]) -> Vec<usize> {
    let mut out = Vec::new();
    for i in 0..bytes.len().saturating_sub(4) {
        if &bytes[i..i + 4] == tag {
            out.push(i);
        }
    }
    out
}

/// Build a multi-segment OpenDML file the demuxer's `want_ix_scan`
/// will trigger for. Uses an 8 KiB `RiffSegmentLimit::Bytes` so the
/// muxer rolls into multiple `RIFF AVIX` segments (the demuxer's
/// `movi_segments.len() > 1` branch fires, parsing every
/// per-segment `ix##` into `std_indexes`) while the primary
/// segment still carries enough packets for the cross-validator's
/// per-packet comparison to be meaningful.
fn write_multi_segment_opendml_file(
    path: &std::path::Path,
    stream: &StreamInfo,
    frames: &[Vec<u8>],
) {
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let opts = AviMuxOptions::new().synthesise_idx1_from_ix(true);
    let mut mux = open_avi(
        ws,
        std::slice::from_ref(stream),
        AviKind::OpenDml(RiffSegmentLimit::Bytes(8 * 1024)),
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

#[test]
fn cross_validator_silent_when_idx1_and_ix_agree() {
    // Round-17 C4: a freshly-muxed multi-segment OpenDML file with
    // synthesise_idx1_from_ix on must produce a byte-identical idx1
    // and ix## packet view (for the primary segment) — the
    // cross-validator stays silent (no `divergent_offsets` key).
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..16).map(|i| synth_payload(i + 17100, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r17-cross-agree.avi");
    write_multi_segment_opendml_file(&tmp, &stream, &frames);

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    for (k, _) in dmx.metadata() {
        assert!(
            !k.contains("divergent_offsets"),
            "no divergent_offsets metadata expected for in-sync idx1+ix## file (saw key {k})"
        );
    }
}

#[test]
fn cross_validator_surfaces_mismatch_when_idx1_offset_corrupted() {
    // Round-17 C4: build a normal multi-segment OpenDML file with
    // both idx1 + ix## (the cross-validator only fires when
    // std_indexes is non-empty, which `want_ix_scan` requires
    // multi-segment to trigger), then corrupt the FIRST idx1
    // entry's offset DWORD in-place. Re-opening must surface
    // `avi:idx1.0.divergent_offsets`.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..16).map(|i| synth_payload(i + 17200, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r17-cross-mismatch.avi");
    write_multi_segment_opendml_file(&tmp, &stream, &frames);

    // Mutate the first idx1 entry's `dwOffset` DWORD: the entry
    // layout is ckid(4) + flags(4) + offset(4) + size(4) = 16 B,
    // and the chunk body starts 8 B after the `idx1` FourCC. Pick
    // a clearly-wrong offset that will read a different chunk.
    let mut bytes = std::fs::read(&tmp).unwrap();
    let idx1_offsets = find_chunk_offsets(&bytes, b"idx1");
    assert_eq!(idx1_offsets.len(), 1, "expected exactly one idx1");
    let body_start = idx1_offsets[0] + 8;
    // Entry 0's offset DWORD lives at body_start + 8 .. body_start + 12.
    let off_dword_pos = body_start + 8;
    // Write an obviously-wrong offset (0xDEAD_BEEF) — anything
    // that doesn't equal the original raw offset will trigger the
    // mismatch.
    bytes[off_dword_pos] = 0xEF;
    bytes[off_dword_pos + 1] = 0xBE;
    bytes[off_dword_pos + 2] = 0xAD;
    bytes[off_dword_pos + 3] = 0xDE;
    let mutated = std::env::temp_dir().join("oxideav-avi-r17-cross-mismatch-mut.avi");
    std::fs::write(&mutated, &bytes).unwrap();

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&mutated).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let div_keys: Vec<_> = dmx
        .metadata()
        .iter()
        .filter(|(k, _)| k.contains("divergent_offsets"))
        .collect();
    assert_eq!(
        div_keys.len(),
        1,
        "expected one divergent_offsets metadata key, got {div_keys:?}"
    );
    let (k, v) = div_keys[0];
    assert_eq!(k, "avi:idx1.0.divergent_offsets");
    assert!(v.starts_with("seq=0 "), "value should report seq=0: {v}");
    assert!(v.contains("idx1=offset_"), "value should mention idx1: {v}");
    assert!(v.contains("ix##=offset_"), "value should mention ix##: {v}");
}

#[test]
fn cross_validator_surfaces_length_mismatch_when_idx1_truncated() {
    // Round-17 C4: a length mismatch (idx1 has fewer primary-segment
    // entries than ix##) is itself a divergence — surface it at
    // index `common` (the first beyond-shared-prefix slot). We use
    // a multi-segment OpenDML mux + drop one entry near the end of
    // the primary-segment idx1 entries.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..16).map(|i| synth_payload(i + 17300, 256)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r17-cross-len.avi");
    write_multi_segment_opendml_file(&tmp, &stream, &frames);

    let mut bytes = std::fs::read(&tmp).unwrap();
    let idx1_offsets = find_chunk_offsets(&bytes, b"idx1");
    assert_eq!(idx1_offsets.len(), 1);
    let idx1_off = idx1_offsets[0];
    let body_size = u32::from_le_bytes([
        bytes[idx1_off + 4],
        bytes[idx1_off + 5],
        bytes[idx1_off + 6],
        bytes[idx1_off + 7],
    ]) as usize;
    let body_start = idx1_off + 8;
    let n_entries = body_size / 16;
    assert!(
        n_entries >= 2,
        "idx1 must have multiple entries (got {n_entries})"
    );
    // Mangle the LAST idx1 entry's ckid to "????" so
    // parse_stream_index returns None and build_idx_table drops it.
    // After this, idx1 has (n_entries - 1) entries while the
    // primary-segment ix## still reports n_entries.
    let last_ckid = body_start + (n_entries - 1) * 16;
    bytes[last_ckid] = b'?';
    bytes[last_ckid + 1] = b'?';
    bytes[last_ckid + 2] = b'?';
    bytes[last_ckid + 3] = b'?';
    let mutated = std::env::temp_dir().join("oxideav-avi-r17-cross-len-mut.avi");
    std::fs::write(&mutated, &bytes).unwrap();

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&mutated).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let div_keys: Vec<_> = dmx
        .metadata()
        .iter()
        .filter(|(k, _)| k.contains("divergent_offsets"))
        .collect();
    assert_eq!(
        div_keys.len(),
        1,
        "expected one divergent_offsets key, got {div_keys:?}"
    );
    let (k, v) = div_keys[0];
    assert_eq!(k, "avi:idx1.0.divergent_offsets");
    // Reported sequence is the new idx1 length (the first
    // beyond-shared-prefix slot).
    let expected_seq = n_entries - 1;
    assert!(
        v.starts_with(&format!("seq={expected_seq} ")),
        "value should report seq={expected_seq} (length-mismatch slot): {v}"
    );
}

#[test]
fn cross_validator_silent_when_only_idx1_present() {
    // Round-17 C4: AVI 1.0 mode emits only idx1 (no ix##), so the
    // cross-validator has nothing to compare against and stays
    // silent — even with a freshly-muxed file.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synth_payload(i + 17400, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r17-cross-avi10.avi");
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
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let reg = registry_with_video();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    for (k, _) in dmx.metadata() {
        assert!(
            !k.contains("divergent_offsets"),
            "AVI 1.0 mode has no ix## to cross-check; saw key {k}"
        );
    }
}
