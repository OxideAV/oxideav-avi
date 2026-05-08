//! Round-7 OpenDML 2.0 + AVI 1.0 feature tests.
//!
//! Covers:
//! - **C1** mid-`movi` `ix##` index emit + parse.
//!   `AviMuxOptions::with_mid_movi_index(stream, n)` flushes a
//!   standard-index chunk (e.g. `02ix` for stream 2) every `n`
//!   packets while the `movi` LIST is open. Per OpenDML 2.0
//!   §"Index Locations in RIFF File", inline `ix##` chunks are
//!   spec-blessed for streams that benefit from sub-segment
//!   random-access (timecode, sparse subtitles, sometimes audio).
//!   The demuxer's `scan_ix_in_movi` already walks `movi` segments
//!   for `ix##` chunks; we verify it picks up the inline ones the
//!   round-7 muxer emits.
//! - **C2** Multi-value INFO parsing — `parse_info_list` now surfaces
//!   unknown FourCCs under `avi:info.<fourcc>` instead of dropping
//!   them, so callers wanting full INFO fidelity can read every
//!   sub-chunk regardless of whether it's in the well-known map.

use std::io::Read;

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

/// Synthesise a CodecRegistry with `magicyuv` (video) + `pcm_s16le`
/// (audio) entries so the AVI demuxer resolves both sides of the
/// stream tags. No real codec crates are pulled — round-7 has no new
/// cross-crate dev-deps.
fn registry_with_video_and_audio() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    reg
}

fn magicyuv_stream(index: u32, width: u32, height: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn pcm_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::audio(CodecId::new("pcm_s16le")).with_tag(CodecTag::wave_format(0x0001));
    params.media_type = MediaType::Audio;
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(oxideav_core::SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
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
// C1: mid-`movi` `ix##` flush.
// ---------------------------------------------------------------------------

#[test]
fn mid_movi_ix_chunk_appears_inside_movi_at_cadence() {
    // Round-7 C1: with `with_mid_movi_index(stream, n)` set, the muxer
    // emits an inline `ix##` chunk every `n` packets while writing
    // into `movi`. With cadence = 2 and 5 packets we expect 2 inline
    // flushes (after packets 2 and 4) plus a residual segment-tail
    // flush carrying packet 5's entry.
    let stream = magicyuv_stream(0, 64, 64);
    let frames: Vec<Vec<u8>> = (0..5).map(|i| synth_payload(i + 7100, 96)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r7-mid-movi-ix.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_mid_movi_index(0, 2);
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

    // Walk the file looking for `ix00` chunks. Count where they land
    // — multiple inline chunks must be present (cadence=2 over 5
    // packets => at least 2 mid-`movi` flushes, then the segment-tail
    // residual flush).
    let mut bytes = Vec::new();
    std::fs::File::open(&tmp)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let mut ix00_count = 0usize;
    let mut i = 0;
    while i + 8 <= bytes.len() {
        if &bytes[i..i + 4] == b"ix00" {
            ix00_count += 1;
        }
        i += 1;
    }
    assert!(
        ix00_count >= 3,
        "expected at least 3 ix00 chunks (2 inline + 1 tail), got {ix00_count}"
    );
}

#[test]
fn mid_movi_ix_chunks_round_trip_via_demuxer() {
    // Round-7 C1: every packet round-trips correctly even with the
    // mid-`movi` `ix##` flushes interspersed in the chunk stream.
    // The demuxer's `next_packet` walker tolerates `ix##` chunks
    // inline (it skips them as non-data chunks) and the inline
    // standard-index chunks are picked up by `scan_ix_in_movi`
    // alongside the segment-tail ones.
    let stream = magicyuv_stream(0, 64, 64);
    let frames: Vec<Vec<u8>> = (0..7).map(|i| synth_payload(i + 5800, 128)).collect();
    let reg = registry_with_video_and_audio();

    let tmp = std::env::temp_dir().join("oxideav-avi-r7-mid-movi-ix-roundtrip.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_mid_movi_index(0, 3);
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
    assert_eq!(
        got, frames,
        "all frames must round-trip even with mid-`movi` `ix##` flushes"
    );
}

#[test]
fn mid_movi_ix_for_secondary_stream_emits_correct_fourcc() {
    // Round-7 C1: the inline `ix##` FourCC matches the registered
    // stream index. Set up a 2-stream file (video + audio), register
    // mid-`movi` index for the audio stream (index=1) only, and
    // assert `ix01` chunks appear inside `movi` while no inline
    // `ix00` chunks do (stream 0 keeps the default segment-tail
    // behaviour).
    let v = magicyuv_stream(0, 32, 32);
    let a = pcm_stream(1);
    let video_frames: Vec<Vec<u8>> = (0..4).map(|i| synth_payload(i + 1000, 64)).collect();
    let audio_frames: Vec<Vec<u8>> = (0..6).map(|i| synth_payload(i + 2000, 480)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r7-mid-movi-secondary.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_mid_movi_index(1, 2);
        let mut mux = open_avi(
            ws,
            &[v.clone(), a.clone()],
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        // Interleave: each video frame followed by one audio frame.
        let mut ai = 0usize;
        for (vi, frame) in video_frames.iter().enumerate() {
            let mut pkt = Packet::new(0, v.time_base, frame.clone());
            pkt.pts = Some(vi as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
            if ai < audio_frames.len() {
                let mut apkt = Packet::new(1, a.time_base, audio_frames[ai].clone());
                apkt.pts = Some(ai as i64);
                apkt.flags.keyframe = true;
                mux.write_packet(&apkt).unwrap();
                ai += 1;
            }
        }
        // Drain any remaining audio.
        while ai < audio_frames.len() {
            let mut apkt = Packet::new(1, a.time_base, audio_frames[ai].clone());
            apkt.pts = Some(ai as i64);
            apkt.flags.keyframe = true;
            mux.write_packet(&apkt).unwrap();
            ai += 1;
        }
        mux.write_trailer().unwrap();
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&tmp)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();

    // Locate the `LIST movi` body so we only count `ix##` chunks
    // inside it (the strl-level `indx` super-index also has
    // FourCC-shaped data we don't want to count). Scan top-level
    // for "LIST" + size + "movi".
    let mut movi_body: Option<(usize, usize)> = None;
    let mut i = 0;
    while i + 12 <= bytes.len() {
        if &bytes[i..i + 4] == b"LIST" && &bytes[i + 8..i + 12] == b"movi" {
            let sz = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]])
                as usize;
            let body_start = i + 12;
            let body_end = (i + 8 + sz).min(bytes.len());
            movi_body = Some((body_start, body_end));
            break;
        }
        i += 1;
    }
    let (mb, me) = movi_body.expect("missing LIST movi");
    let mut ix00_in_movi = 0usize;
    let mut ix01_in_movi = 0usize;
    let mut j = mb;
    while j + 4 <= me {
        if &bytes[j..j + 4] == b"ix00" {
            ix00_in_movi += 1;
        } else if &bytes[j..j + 4] == b"ix01" {
            ix01_in_movi += 1;
        }
        j += 1;
    }
    // Stream 0 has no mid-`movi` schedule → at most one tail flush.
    assert!(
        ix00_in_movi <= 1,
        "stream 0 had no mid-movi schedule but found {ix00_in_movi} ix00 chunks in movi"
    );
    // Stream 1 (audio) cadence=2 over 6 packets ⇒ 3 inline flushes,
    // plus possibly the residual segment-tail flush. Allow either 3
    // or 4 chunks (the residual tail chunk is empty if the cadence
    // divides evenly so the tail flush is a no-op).
    assert!(
        (3..=4).contains(&ix01_in_movi),
        "stream 1 cadence=2 over 6 packets must produce 3 or 4 ix01 chunks (got {ix01_in_movi})"
    );
}

#[test]
fn mid_movi_ix_zero_cadence_disables_inline_flush() {
    // Round-7 C1: `with_mid_movi_index(stream, 0)` must NOT register
    // a mid-`movi` flush — it's the public way to disable a
    // previously-registered cadence. Verify by writing 4 packets and
    // confirming only one `ix00` chunk lands (the segment-tail
    // flush).
    let stream = magicyuv_stream(0, 32, 32);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth_payload(i + 8800, 64)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r7-mid-movi-zero.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        // Register the cadence then "clear" it by re-registering with
        // 0 — the second call must drop the entry per builder semantics.
        let opts = AviMuxOptions::new()
            .with_mid_movi_index(0, 2)
            .with_mid_movi_index(0, 0);
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
    let mut bytes = Vec::new();
    std::fs::File::open(&tmp)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let mut ix00 = 0usize;
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ix00" {
            ix00 += 1;
        }
        i += 1;
    }
    assert_eq!(
        ix00, 1,
        "zero-cadence opt-out must emit exactly one tail-flushed ix00 (got {ix00})"
    );
}

#[test]
fn mid_movi_ix_ignored_for_avi10_envelope() {
    // Round-7 C1: AVI 1.0 has no `ix##` chunks at all. Registering a
    // mid-`movi` schedule while opening with `AviKind::Avi10` must be
    // a silent no-op.
    let stream = magicyuv_stream(0, 32, 32);
    let payload = synth_payload(42, 64);

    let tmp = std::env::temp_dir().join("oxideav-avi-r7-mid-movi-avi10.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_mid_movi_index(0, 1);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
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
    // Pure AVI 1.0 → no `ix##` chunks anywhere.
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if (&bytes[i..i + 4] == b"ix00") || (&bytes[i..i + 4] == b"ix01") {
            panic!("AVI 1.0 must not emit ix## chunks (found at offset {i})");
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// C2: Multi-value INFO parsing — surface unknown FourCCs.
// ---------------------------------------------------------------------------

#[test]
fn unknown_info_fourcc_surfaces_under_avi_info_namespace() {
    // Round-7 C2: `parse_info_list` no longer drops unknown INFO
    // FourCCs — they surface under `avi:info.<fourcc>` so callers
    // wanting full INFO fidelity (e.g. video editors round-tripping
    // capture-card metadata) can read every entry.
    //
    // `IPRT` ("printing destination" — not in our well-known map),
    // `ISRC` ("source"), `IDST` ("destination") are all real INFO
    // FourCCs from the AVI 1.0 registry that we don't translate to
    // a standard key. Verify they all surface verbatim.
    let stream = magicyuv_stream(0, 32, 32);
    let payload = synth_payload(13, 32);
    let reg = registry_with_video_and_audio();

    let tmp = std::env::temp_dir().join("oxideav-avi-r7-info-unknown.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_info(*b"INAM", "Known title") // mapped → "title"
            .with_info(*b"IPRT", "Printer X") // unknown → "avi:info.IPRT"
            .with_info(*b"ISRC", "Camera 5") // unknown → "avi:info.ISRC"
            .with_info(*b"IDST", "Archive A"); // unknown → "avi:info.IDST"
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let md = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    // Known FourCC still maps to the canonical key.
    assert_eq!(get("title").as_deref(), Some("Known title"));
    // Unknown FourCCs surface under `avi:info.<fourcc>` verbatim.
    assert_eq!(get("avi:info.IPRT").as_deref(), Some("Printer X"));
    assert_eq!(get("avi:info.ISRC").as_deref(), Some("Camera 5"));
    assert_eq!(get("avi:info.IDST").as_deref(), Some("Archive A"));
}

#[test]
fn duplicate_info_fourccs_round_trip_as_multi_values() {
    // Round-7 C2: `LIST INFO` is a flat list (not a map), so multiple
    // sub-chunks with the same FourCC are spec-legal. The parser
    // emits one metadata entry per occurrence, preserving order.
    let stream = magicyuv_stream(0, 32, 32);
    let payload = synth_payload(99, 16);
    let reg = registry_with_video_and_audio();

    let tmp = std::env::temp_dir().join("oxideav-avi-r7-info-dup.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new()
            .with_info(*b"IART", "Artist 1")
            .with_info(*b"IART", "Artist 2");
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let md = dmx.metadata();
    let artists: Vec<&str> = md
        .iter()
        .filter(|(k, _)| k == "artist")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(
        artists,
        vec!["Artist 1", "Artist 2"],
        "duplicate IART must surface as two ordered 'artist' entries"
    );
}
