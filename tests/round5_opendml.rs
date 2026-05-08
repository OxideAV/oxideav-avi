//! Round-5 OpenDML 2.0 + AVI 1.0 feature tests.
//!
//! Covers:
//! - **C1** 2-field-aware accessor: `AviDemuxer::field2_offset_for_packet`
//!   surfaces the per-packet `dwOffsetField2` directly, parallel to
//!   the comma-joined `avi:ix.<n>.field2_offsets` metadata key.
//! - **C2** idx1 + 2-field correlation: when a 2-field `ix##` is
//!   present alongside an `idx1` table for the same stream, the
//!   demuxer surfaces an `avi:idx1.<n>.is_2field` hint so consumers
//!   walking idx1 know the entries describe interlaced frames.
//! - **C3** VBR audio framing: `Packet.duration` drives
//!   `strh.dwLength` for non-PCM audio (sample_size == 0) so VBR
//!   streams round-trip a real frame count rather than the round-3
//!   "1 per packet" placeholder.
//! - **C4** xxix cycling validation: muxer surfaces the truncation
//!   count via `AviMuxer::truncated_super_index_segments()` and the
//!   demuxer surfaces `avi:indx.<n>.overflow_entries` when an `indx`
//!   declares more entries than the conventional 256-slot reserve.

use std::io::Read;

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

/// Synthetic registry entry for the FOURCC ↔ codec_id mapping the
/// tests below need. Avoids a producer-crate dev-dep — real
/// MagicYUV decode coverage lives in `crates/oxideav-tests`.
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
// C1: per-packet field-2 accessor on AviDemuxer.
// ---------------------------------------------------------------------------

#[test]
fn field2_offset_for_packet_returns_per_entry_value() {
    // Mux a 2-field interlaced stream with monotonically increasing
    // payload-relative field-2 offsets per packet, then read each
    // back through `AviDemuxer::field2_offset_for_packet`. Round-5
    // C1: this used to require parsing the comma-joined
    // `avi:ix.<n>.field2_offsets` metadata value.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..5).map(|i| synthesize_payload(i + 6000, 256)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-field2-accessor.avi");
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
            // Vary the per-frame split: 64, 96, 128, 160, 192.
            let split = 64u32 + 32u32 * i as u32;
            mux.set_field2_offset(split);
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Per-packet accessor returns Some(qwBaseOffset-relative offset)
    // for every frame; the values must be monotonically increasing
    // because dwOffset for each successive packet grows and the
    // muxer's payload-relative split also grows.
    let mut prev = 0u32;
    for i in 0..frames.len() {
        let got = dmx
            .field2_offset_for_packet(0, i)
            .expect("every 2-field packet must report a field2 offset");
        assert!(
            got > prev,
            "field-2 offsets must be strictly increasing in file order; got {prev} then {got} at packet {i}"
        );
        prev = got;
    }
    // Out-of-range packet returns None.
    assert!(dmx.field2_offset_for_packet(0, 999).is_none());
    // Unknown stream returns None.
    assert!(dmx.field2_offset_for_packet(7, 0).is_none());
}

#[test]
fn field2_offset_for_packet_none_on_non_field2_stream() {
    // Without `with_field2_stream(0)`, the std-index keeps the
    // default 8-byte entries — `field2_offset_for_packet` must
    // return `None` for every packet.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(1, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-field2-default.avi");
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
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert!(dmx.field2_offset_for_packet(0, 0).is_none());
}

// ---------------------------------------------------------------------------
// C2: idx1 + 2-field correlation surfaces a per-stream hint.
// ---------------------------------------------------------------------------

#[test]
fn idx1_2field_hint_emitted_when_ix_subtype_set() {
    // OpenDML mode produces both idx1 (legacy single-segment fallback)
    // and ix## std-indexes. When the ix## carries
    // `bIndexSubType == AVI_INDEX_2FIELD`, the demuxer surfaces an
    // `avi:idx1.<n>.is_2field = true` hint at the idx1 layer too
    // (round-5 C2) so consumers seeking via idx1 still know the
    // entries describe interlaced frames.
    let stream = magicyuv_stream(64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 7000, 128)).collect();
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-idx1-2field.avi");
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
            mux.set_field2_offset(64);
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

    // The ix-layer hint already exists (round 4); the new layer is
    // the idx1 hint.
    assert_eq!(get("avi:ix.0.is_2field").as_deref(), Some("true"));
    assert_eq!(
        get("avi:idx1.0.is_2field").as_deref(),
        Some("true"),
        "idx1 hint must surface alongside ix## hint when both index forms exist"
    );
}

#[test]
fn idx1_2field_hint_absent_for_progressive_streams() {
    // Default (non-2-field) OpenDML mode: idx1 is still emitted but
    // no `avi:idx1.<n>.is_2field` hint should appear.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(2, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-idx1-progressive.avi");
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
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let any_hint = dmx
        .metadata()
        .iter()
        .any(|(k, _)| k.starts_with("avi:idx1.") && k.ends_with(".is_2field"));
    assert!(
        !any_hint,
        "no idx1 2-field hint expected for progressive streams"
    );
}

// ---------------------------------------------------------------------------
// C3: VBR audio uses Packet.duration to drive strh.dwLength.
// ---------------------------------------------------------------------------

fn vbr_audio_stream() -> StreamInfo {
    // Compressed audio (mp3 wFormatTag 0x0055) → packaging::build_strf
    // sets sample_size = 0, which is the "VBR" indicator the muxer
    // honors in `sample_count_of_packet`.
    let mut params =
        CodecParameters::audio(CodecId::new("mp3")).with_tag(CodecTag::wave_format(0x0055));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

#[test]
fn vbr_audio_duration_drives_strh_dwlength() {
    // With per-packet `duration` set, the muxer accumulates the
    // duration values into strh.dwLength rather than the round-3
    // "1 per packet" fallback. Round-5 C3.
    let stream = vbr_audio_stream();
    // 3 packets at 1152 samples (MP3 nominal frame size).
    let durations: [i64; 3] = [1152, 1152, 1152];
    let payloads: Vec<Vec<u8>> = (0..3).map(|i| synthesize_payload(i + 8000, 480)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-vbr-dur.avi");
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
        for (i, payload) in payloads.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64 * durations[i]);
            pkt.duration = Some(durations[i]);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&tmp)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();

    // strh body lives at strl_off + 20 (strl LIST preamble: LIST + size
    // + "strl" = 12 bytes, then "strh" + size = 8 → body starts at +20).
    // strl_off = 88 (RIFF preamble 12 + LIST hdrl preamble 12 + avih
    // chunk 64). strh.dwLength sits at body[32..36].
    let strh_body_off = 88usize + 20;
    let dw_length_off = strh_body_off + 32;
    let dw_length = u32::from_le_bytes([
        bytes[dw_length_off],
        bytes[dw_length_off + 1],
        bytes[dw_length_off + 2],
        bytes[dw_length_off + 3],
    ]);
    let expected = durations.iter().sum::<i64>() as u32;
    assert_eq!(
        dw_length, expected,
        "strh.dwLength must be the sum of Packet.duration values for VBR audio"
    );
}

#[test]
fn vbr_audio_without_duration_falls_back_to_packet_count() {
    // Without `Packet.duration`, the muxer keeps the round-3
    // behaviour: one tick per packet so dwLength == packet count.
    // Round-5 C3 must not regress this fallback.
    let stream = vbr_audio_stream();
    let payloads: Vec<Vec<u8>> = (0..4).map(|i| synthesize_payload(i + 9000, 480)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-vbr-nodur.avi");
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
        for (i, payload) in payloads.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            // duration left as None.
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&tmp)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let dw_length_off = 88usize + 20 + 32;
    let dw_length = u32::from_le_bytes([
        bytes[dw_length_off],
        bytes[dw_length_off + 1],
        bytes[dw_length_off + 2],
        bytes[dw_length_off + 3],
    ]);
    assert_eq!(
        dw_length,
        payloads.len() as u32,
        "strh.dwLength must fall back to packet count when no Packet.duration is set"
    );
}

// ---------------------------------------------------------------------------
// C4: xxix cycling validation surfaces truncation count.
// ---------------------------------------------------------------------------

#[test]
fn truncated_super_index_segments_zero_for_avi10() {
    // AVI 1.0 has no super-index, so the truncation accessor must
    // return 0 regardless of segment count.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(1, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-trunc-avi10.avi");
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
    assert_eq!(mux.truncated_super_index_segments(), 0);
}

#[test]
fn truncated_super_index_segments_zero_when_under_capacity() {
    // OpenDML with a small handful of segments: well under the 256-slot
    // capacity, so the accessor returns 0.
    let stream = magicyuv_stream(64, 64);
    // 4 KiB payload plus 4 KiB segment cap → most packets force a
    // new segment, but well under 256 segments total.
    let frames: Vec<Vec<u8>> = (0..6)
        .map(|i| synthesize_payload(i + 14000, 4096))
        .collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-trunc-under.avi");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(
        ws,
        std::slice::from_ref(&stream),
        AviKind::OpenDml(RiffSegmentLimit::Bytes(4096)),
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
    assert_eq!(mux.truncated_super_index_segments(), 0);
}

#[test]
fn demuxer_overflow_metadata_absent_for_normal_files() {
    // Files within the 256-slot reserve must not surface
    // `avi:indx.<n>.overflow_entries`.
    let stream = magicyuv_stream(64, 64);
    let payload = synthesize_payload(3, 64);
    let reg = registry_with_magicyuv();

    let tmp = std::env::temp_dir().join("oxideav-avi-r5-no-overflow.avi");
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
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let any = dmx
        .metadata()
        .iter()
        .any(|(k, _)| k.starts_with("avi:indx.") && k.ends_with(".overflow_entries"));
    assert!(!any, "no overflow metadata expected for typical files");
}
