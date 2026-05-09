//! Round-15 AVI feature tests.
//!
//! Covers:
//! - **C2** Audio-only `avih.dwMaxBytesPerSec` fallback — when no
//!   video stream is present (no per-frame timing) the populator
//!   sums every audio track's WAVEFORMATEX `nAvgBytesPerSec` per
//!   AVI 1.0 §3.1, instead of leaving the field at 0 the way
//!   round-14 did.
//! - **C3** `AviDemuxer::text_chunk_typed_iter` — lazy iterator
//!   over `xxtx` text/subtitle chunks, mirror of round-14's
//!   `palette_change_typed_iter`. Also covers the typed eager
//!   accessor `text_chunk_typed` and the muxer-side
//!   `with_text_chunk_typed` round-trip.
//! - **C1** `avi:over_budget` metadata key — surfaced when the
//!   stamped `avih.dwMaxBytesPerSec` is smaller than
//!   `sum(audio.avg_bytes_per_sec) + computed_video_bytes_per_sec`.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Muxer, Packet, Rational,
    ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{open_avi as demuxer_open_avi, TextChunk};
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};

// ---------------------------------------------------------------------------
// Test fixtures.
// ---------------------------------------------------------------------------

fn registry_with_magicyuv_and_pcm() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    reg
}

fn magicyuv_stream(width: u32, height: u32, fps: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.width = Some(width);
    params.height = Some(height);
    params.frame_rate = Some(Rational::new(fps as i64, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, fps as i64),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn pcm_s16le_stream(channels: u16, sample_rate: u32) -> StreamInfo {
    let mut params =
        CodecParameters::audio(CodecId::new("pcm_s16le")).with_tag(CodecTag::wave_format(0x0001));
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sample_rate as i64),
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
// C2: audio-only `dwMaxBytesPerSec` fallback to sum(wave_format.avg_bytes_per_sec).
// ---------------------------------------------------------------------------

#[test]
fn audio_only_max_bytes_falls_back_to_pcm_avg_bytes_per_sec() {
    // Round-15 C2: a single PCM s16le stereo @ 48 kHz audio stream
    // → block_align = 2 ch × 2 B = 4, nAvgBytesPerSec = 48_000 × 4
    // = 192_000. With no video stream the per-frame-timing path
    // returns 0, so the round-15 fallback sums every audio track's
    // WAVEFORMATEX `nAvgBytesPerSec` → 192_000.
    let stream = pcm_s16le_stream(2, 48_000);
    let payload = synth_payload(101, 4096);

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-audio-only-pcm.avi");
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
            pkt.pts = Some(i);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    let got = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    assert_eq!(
        got, 192_000,
        "audio-only mux must surface sum(wave_format.avg_bytes_per_sec)"
    );

    // Round-trip via the demuxer's metadata key.
    let reg = registry_with_magicyuv_and_pcm();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let md_str = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "avi:max_bytes_per_sec")
        .map(|(_, v)| v.clone())
        .expect("avi:max_bytes_per_sec must be present when value is non-zero");
    assert_eq!(md_str, "192000");
}

#[test]
fn audio_only_max_bytes_sums_two_tracks() {
    // Round-15 C2: with two audio tracks the fallback sums both
    // WAVEFORMATEX `nAvgBytesPerSec` values. PCM s16le mono @ 8 kHz
    // = 16_000 plus PCM s16le stereo @ 48 kHz = 192_000 → 208_000.
    let mut a = pcm_s16le_stream(1, 8_000);
    a.index = 0;
    let mut b = pcm_s16le_stream(2, 48_000);
    b.index = 1;
    let streams = [a.clone(), b.clone()];
    let payload_a = synth_payload(102, 256);
    let payload_b = synth_payload(103, 4096);

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-audio-only-two-tracks.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::new()).unwrap();
        mux.write_header().unwrap();
        for i in 0..3 {
            let mut p = Packet::new(0, a.time_base, payload_a.clone());
            p.pts = Some(i);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
            let mut p = Packet::new(1, b.time_base, payload_b.clone());
            p.pts = Some(i);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    let got = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    assert_eq!(
        got, 208_000,
        "must equal 8 kHz mono + 48 kHz stereo PCM rates"
    );
}

#[test]
fn audio_only_max_bytes_override_still_wins() {
    // Round-15 C2: the explicit override path (round-14 builder
    // helper) keeps precedence over the new fallback — caller's
    // value lands verbatim.
    let stream = pcm_s16le_stream(2, 48_000);
    let payload = synth_payload(104, 4096);
    let want: u32 = 1_234_567;

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-audio-only-override.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_max_bytes_per_sec(want);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..2 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    let got = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    assert_eq!(got, want, "override must beat fallback");
}

#[test]
fn audio_video_mux_uses_video_path_not_audio_fallback() {
    // Round-15 C2: with a video stream present the populator
    // continues to use the per-frame-timing path (round-14 default),
    // not the audio fallback. Sanity check that adding the fallback
    // didn't change the existing audio+video behaviour.
    let video = magicyuv_stream(64, 64, 25);
    let mut audio = pcm_s16le_stream(2, 48_000);
    audio.index = 1;
    let streams = [video.clone(), audio.clone()];
    let video_payload = synth_payload(105, 1024);
    let audio_payload = synth_payload(106, 4096);

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-audio-video-mux.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::new()).unwrap();
        mux.write_header().unwrap();
        for i in 0..25 {
            let mut p = Packet::new(0, video.time_base, video_payload.clone());
            p.pts = Some(i as i64);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
            let mut p = Packet::new(1, audio.time_base, audio_payload.clone());
            p.pts = Some(i as i64);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    let got = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    // 25 video frames × 1024 B + 25 audio chunks × 4096 B = 128_000 B
    // over 1 s → 128_000 B/s. Allow 1% tolerance.
    let expected: u32 = (25 * 1024 + 25 * 4096) as u32; // 128_000
    let lo = expected - expected / 100;
    let hi = expected + expected / 100;
    assert!(
        got >= lo && got <= hi,
        "audio+video mux must use video timing path, not audio fallback (got {got})"
    );
}

// ---------------------------------------------------------------------------
// C3: TextChunk parse + to_bytes + iterators.
// ---------------------------------------------------------------------------

#[test]
fn text_chunk_parse_to_bytes_round_trip_utf8() {
    // Round-15 C3: a UTF-8 codepage (65001) round-trips byte-exact
    // through TextChunk::parse → to_bytes.
    let original = TextChunk {
        codepage: 65001,
        language: 0x0409, // en-US LANGID
        dialect: 0x0001,
        body: "Héllo, мир!".to_string(),
    };
    let bytes = original.to_bytes();
    // 6-byte header + UTF-8 body.
    assert!(bytes.len() >= 6 + "Héllo, мир!".len());
    let parsed = TextChunk::parse(&bytes).expect("must parse");
    assert_eq!(parsed, original);
}

#[test]
fn text_chunk_parse_to_bytes_round_trip_codepage_zero_is_utf8() {
    // Round-15 C3: codepage 0 ("system default") is treated as
    // UTF-8 by parse/to_bytes (the modern recommendation; legacy
    // ANSI bytes still pass through Latin-1).
    let original = TextChunk {
        codepage: 0,
        language: 0,
        dialect: 0,
        body: "ascii only".to_string(),
    };
    let bytes = original.to_bytes();
    let parsed = TextChunk::parse(&bytes).expect("must parse");
    assert_eq!(parsed, original);
}

#[test]
fn text_chunk_parse_to_bytes_round_trip_latin1_passthrough() {
    // Round-15 C3: a non-UTF-8 codepage uses byte pass-through
    // (each byte → one Latin-1 char) so a parse → to_bytes cycle
    // on the same buffer is byte-exact.
    let mut raw = Vec::new();
    raw.extend_from_slice(&1252u16.to_le_bytes()); // codepage = Windows-1252
    raw.extend_from_slice(&0u16.to_le_bytes());
    raw.extend_from_slice(&0u16.to_le_bytes());
    // Bytes 0xC0..0xC4 are À, Á, Â, Ã, Ä in Latin-1.
    raw.extend_from_slice(&[0xC0, 0xC1, 0xC2, 0xC3, 0xC4]);

    let parsed = TextChunk::parse(&raw).expect("must parse");
    assert_eq!(parsed.codepage, 1252);
    let re_emitted = parsed.to_bytes();
    assert_eq!(re_emitted, raw, "Latin-1 pass-through must round-trip");
}

#[test]
fn text_chunk_parse_too_short_returns_none() {
    // Round-15 C3: a body shorter than the 6-byte VfW header is
    // not a valid TextChunk.
    assert!(TextChunk::parse(&[]).is_none());
    assert!(TextChunk::parse(&[0u8; 5]).is_none());
    // Exactly 6 bytes (no body) is OK — the body is optional.
    assert!(TextChunk::parse(&[0u8; 6]).is_some());
}

#[test]
fn text_chunk_typed_iter_yields_same_sequence_as_eager() {
    // Round-15 C3: lazy iterator returns the same TextChunk
    // sequence as the eager Vec accessor.
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(120, 64);
    let reg = registry_with_magicyuv_and_pcm();

    let chunk_a = TextChunk {
        codepage: 65001,
        language: 0x0409,
        dialect: 0x0001,
        body: "first cue".into(),
    };
    let chunk_b = TextChunk {
        codepage: 65001,
        language: 0x040C,
        dialect: 0x0000,
        body: "deuxième repère".into(),
    };

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-text-iter.avi");
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
        mux.with_text_chunk_typed(0, &chunk_a).unwrap();
        mux.with_text_chunk_typed(0, &chunk_b).unwrap();
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let eager = dmx.text_chunk_typed(0);
    assert_eq!(eager.len(), 2);

    let mut iter = dmx.text_chunk_typed_iter(0);
    assert_eq!(iter.size_hint(), (2, Some(2)));
    let first = iter.next().expect("first item must be present");
    let second = iter.next().expect("second item must be present");
    assert!(
        iter.next().is_none(),
        "iterator must terminate after two items"
    );

    assert_eq!(first.unwrap(), eager[0]);
    assert_eq!(second.unwrap(), eager[1]);
    assert_eq!(eager[0], chunk_a);
    assert_eq!(eager[1], chunk_b);
}

#[test]
fn text_chunk_typed_iter_empty_for_unknown_stream() {
    // Round-15 C3: out-of-range stream index returns an iterator
    // that's immediately empty (mirrors palette accessor).
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(121, 32);
    let reg = registry_with_magicyuv_and_pcm();

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-text-iter-unknown.avi");
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
    assert!(dmx.text_chunk_typed_iter(0).next().is_none());
    assert!(dmx.text_chunk_typed_iter(99).next().is_none());
}

#[test]
fn text_chunk_typed_iter_size_hint_decrements_as_consumed() {
    // Round-15 C3: ExactSizeIterator contract — size_hint shrinks
    // by one per next(). Mirrors palette iterator's contract.
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(122, 32);
    let reg = registry_with_magicyuv_and_pcm();

    let chunks: Vec<TextChunk> = (0..5)
        .map(|i| TextChunk {
            codepage: 65001,
            language: 0,
            dialect: 0,
            body: format!("cue {i}"),
        })
        .collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-text-iter-size-hint.avi");
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
        for tc in &chunks {
            mux.with_text_chunk_typed(0, tc).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let mut iter = dmx.text_chunk_typed_iter(0);
    assert_eq!(iter.len(), 5);
    let _ = iter.next().unwrap().unwrap();
    assert_eq!(iter.len(), 4);
    let _ = iter.next().unwrap().unwrap();
    assert_eq!(iter.len(), 3);
    let drained: Vec<TextChunk> = iter.map(|r| r.unwrap()).collect();
    assert_eq!(drained.len(), 3);
}

#[test]
fn text_chunk_typed_iter_collect_matches_eager_vec() {
    // Round-15 C3: a `.collect::<Result<Vec<_>, _>>()` over the
    // lazy iterator must reproduce the eager Vec exactly when
    // every body is well-formed.
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(123, 32);
    let reg = registry_with_magicyuv_and_pcm();

    let chunks: Vec<TextChunk> = (0..6)
        .map(|i| TextChunk {
            codepage: 65001,
            language: 0,
            dialect: 0,
            body: format!("entry-{i}"),
        })
        .collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-text-iter-collect.avi");
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
        for tc in &chunks {
            mux.with_text_chunk_typed(0, tc).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let eager = dmx.text_chunk_typed(0);
    let lazy: oxideav_core::Result<Vec<TextChunk>> = dmx.text_chunk_typed_iter(0).collect();
    let lazy = lazy.expect("all bodies are well-formed");
    assert_eq!(eager, lazy);
    assert_eq!(eager, chunks);
}

// ---------------------------------------------------------------------------
// C1: avi:over_budget metadata key.
// ---------------------------------------------------------------------------

#[test]
fn over_budget_metadata_surfaces_when_stamp_understates_demand() {
    // Round-15 C1: a deliberately-small `with_max_bytes_per_sec`
    // override stamps a value below the real per-stream sum →
    // demuxer surfaces `avi:over_budget` with both the expected
    // and stamped values. Use a video stream so the computed
    // bitrate term is non-zero.
    let stream = magicyuv_stream(320, 240, 25);
    let payload = synth_payload(140, 8192);
    let reg = registry_with_magicyuv_and_pcm();

    // 25 fps × 8 KiB/frame = 200 KiB/s; stamp 10 KiB/s.
    let tmp = std::env::temp_dir().join("oxideav-avi-r15-over-budget.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_max_bytes_per_sec(10_000);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..25 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let val = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "avi:over_budget")
        .map(|(_, v)| v.clone())
        .expect("avi:over_budget must surface when stamp under-allocates");
    assert!(
        val.contains("expected_max=") && val.contains("stamped=10000"),
        "metadata value must name both numbers (got {val:?})"
    );
}

#[test]
fn over_budget_metadata_absent_when_stamp_meets_or_exceeds_demand() {
    // Round-15 C1: the conformant default (auto-populator stamps
    // exactly the per-stream sum) never trips the warning.
    let stream = magicyuv_stream(320, 240, 25);
    let payload = synth_payload(141, 8192);
    let reg = registry_with_magicyuv_and_pcm();

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-on-budget.avi");
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
        for i in 0..25 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert!(
        dmx.metadata().iter().all(|(k, _)| k != "avi:over_budget"),
        "auto-populated dwMaxBytesPerSec must never trip the over-budget warning"
    );
}

#[test]
fn over_budget_metadata_skipped_when_stamp_is_zero() {
    // Round-15 C1: a writer that left dwMaxBytesPerSec = 0 has no
    // budget to compare against; the demuxer must skip the
    // warning rather than always firing.
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(142, 1024);
    let reg = registry_with_magicyuv_and_pcm();

    let tmp = std::env::temp_dir().join("oxideav-avi-r15-zero-stamp.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_max_bytes_per_sec(0);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..5 {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert!(
        dmx.metadata().iter().all(|(k, _)| k != "avi:over_budget"),
        "stamped=0 means rate-unknown, no warning"
    );
}
