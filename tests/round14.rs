//! Round-14 AVI feature tests.
//!
//! Covers:
//! - **C1** `avih.dwMaxBytesPerSec` populator — auto-computed from
//!   `sum(per_track_total_bytes) / file_duration_seconds` per AVI 1.0
//!   §3.1, plus [`AviMuxOptions::with_max_bytes_per_sec`] override.
//! - **C2** `strh.dwSampleSize` VBR/CBR validator at `open_avi` —
//!   VBR codecs (MP3 / AAC / MPEG) require `dwSampleSize == 0`; CBR
//!   codecs (PCM / G.711 / IMA-ADPCM) require `dwSampleSize > 0`.
//!   Caller can opt out via [`open_avi_lenient`].
//! - **C3** `AviDemuxer::palette_change_typed_iter` — lazy iterator
//!   over `xxpc` palette-change chunks, decoding the typed shape on
//!   demand instead of materialising the full Vec.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, Muxer, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{
    open_avi as demuxer_open_avi, open_avi_lenient, PaletteChange, PaletteEntry,
};
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};

// ---------------------------------------------------------------------------
// Test fixtures (self-contained per the workspace per-round-test convention).
// ---------------------------------------------------------------------------

fn registry_with_magicyuv_and_pcm() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    reg.register(CodecInfo::new(CodecId::new("mp3")).tag(CodecTag::wave_format(0x0055)));
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
// C1: avih.dwMaxBytesPerSec populator.
// ---------------------------------------------------------------------------

#[test]
fn max_bytes_per_sec_computed_from_total_bytes_over_duration() {
    // Round-14 C1: a 10-second mux at 25 fps with a 250-frame total
    // and a per-frame body of 20_972 bytes lands at
    // 250 * 20_972 = 5_243_000 bytes ≈ 5 MB. file_duration = 10 s.
    // Expected dwMaxBytesPerSec ≈ 524_300 (≈ 5 MB / 10 s).
    //
    // The important property is that the populator scales with
    // `total_bytes / duration_seconds` and stamps the value into the
    // avih body; the spec only requires "approximate maximum data
    // rate", so we assert against a tight tolerance band rather than
    // an exact equality on rounding.
    let stream = magicyuv_stream(320, 240, 25);
    let reg = registry_with_magicyuv_and_pcm();
    let payload = synth_payload(11, 20_972);
    let total_frames = 250u32;

    let tmp = std::env::temp_dir().join("oxideav-avi-r14-max-bytes-auto.avi");
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
        for i in 0..total_frames {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    // avih.dwMaxBytesPerSec sits at body offset 4 → file offset 36
    // (RIFF preamble 12 + LIST hdrl preamble 12 + "avih" + size 8 + 4).
    let max_bytes = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    let expected = (total_frames as u64 * payload.len() as u64) / 10; // 10 s
    let expected_u32 = expected as u32;
    // Allow 1% tolerance for integer division rounding.
    let lo = expected_u32 - expected_u32 / 100;
    let hi = expected_u32 + expected_u32 / 100;
    assert!(
        max_bytes >= lo && max_bytes <= hi,
        "dwMaxBytesPerSec {max_bytes} should be near {expected_u32} (10s × ~5 MB / s)"
    );
    // Sanity: also compare against the demuxer's metadata key.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let md = dmx.metadata();
    let md_str = md
        .iter()
        .find(|(k, _)| k == "avi:max_bytes_per_sec")
        .map(|(_, v)| v.clone())
        .expect("avi:max_bytes_per_sec metadata key must be present when value is non-zero");
    assert_eq!(md_str, max_bytes.to_string());
}

#[test]
fn max_bytes_per_sec_respects_explicit_override() {
    // Round-14 C1: caller-supplied override stamps the exact value
    // verbatim, bypassing the per-track sum.
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(12, 1024);
    let want: u32 = 4_000_000;

    let tmp = std::env::temp_dir().join("oxideav-avi-r14-max-bytes-override.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_max_bytes_per_sec(want);
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

    let bytes = std::fs::read(&tmp).unwrap();
    let got = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    assert_eq!(got, want, "explicit override must land verbatim");
}

#[test]
fn max_bytes_per_sec_zero_when_no_packets_written() {
    // Round-14 C1: an empty file (no packets, total_frames == 0) has
    // no usable duration → the auto-computed value is 0 (matches the
    // pre-round-14 hard-coded baseline; defensive — never panic).
    let stream = magicyuv_stream(64, 64, 25);

    let tmp = std::env::temp_dir().join("oxideav-avi-r14-max-bytes-empty.avi");
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

    let bytes = std::fs::read(&tmp).unwrap();
    let got = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    assert_eq!(got, 0, "empty mux must surface zero data rate");
}

#[test]
fn max_bytes_per_sec_audio_only_falls_back_to_wave_format_sum() {
    // Round-15 C2: closes round-14's "audio-only file surfaces 0"
    // gap. With no video stream the per-frame timing path returns 0,
    // so the populator now falls back to summing each audio track's
    // WAVEFORMATEX `nAvgBytesPerSec` (per AVI 1.0 §3.1, the right
    // pacing budget for an audio-only file). PCM s16le stereo at
    // 48 kHz → block_align = 2 ch × 2 B = 4, nAvgBytesPerSec =
    // 48_000 × 4 = 192_000.
    let mut params =
        CodecParameters::audio(CodecId::new("pcm_s16le")).with_tag(CodecTag::wave_format(0x0001));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };
    let payload = synth_payload(13, 4096);

    let tmp = std::env::temp_dir().join("oxideav-avi-r14-max-bytes-audio-only.avi");
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
}

// ---------------------------------------------------------------------------
// C2: strh.dwSampleSize VBR/CBR validator.
// ---------------------------------------------------------------------------

/// Build a minimal AVI byte buffer with a single audio strl whose
/// `wFormatTag` and `dwSampleSize` are set verbatim — bypasses the
/// muxer (which would normalise them) so the validator can be exercised
/// directly with arbitrary (potentially malformed) values.
fn build_one_stream_audio_avi(format_tag: u16, sample_size: u32) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();

    // ---- avih body (56 bytes) ----
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMicroSecPerFrame
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwTotalFrames
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwWidth
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwHeight
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]

    // ---- strh body (56 bytes) ----
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"auds"); // fccType
    strh.extend_from_slice(b"\0\0\0\0"); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&48_000u32.to_le_bytes()); // dwRate
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwLength
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwQuality
    strh.extend_from_slice(&sample_size.to_le_bytes()); // dwSampleSize
    strh.extend_from_slice(&[0u8; 8]); // rcFrame

    // ---- strf (WAVEFORMATEX, 18 bytes minimum) ----
    let mut strf = Vec::with_capacity(18);
    strf.extend_from_slice(&format_tag.to_le_bytes()); // wFormatTag
    strf.extend_from_slice(&2u16.to_le_bytes()); // nChannels
    strf.extend_from_slice(&48_000u32.to_le_bytes()); // nSamplesPerSec
    strf.extend_from_slice(&192_000u32.to_le_bytes()); // nAvgBytesPerSec
    strf.extend_from_slice(&4u16.to_le_bytes()); // nBlockAlign
    strf.extend_from_slice(&16u16.to_le_bytes()); // wBitsPerSample
    strf.extend_from_slice(&0u16.to_le_bytes()); // cbSize

    // ---- compose strl LIST ----
    fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
    }

    let mut strl_body: Vec<u8> = Vec::new();
    strl_body.extend_from_slice(b"strl");
    chunk(&mut strl_body, b"strh", &strh);
    chunk(&mut strl_body, b"strf", &strf);

    // ---- compose hdrl LIST ----
    let mut hdrl_body: Vec<u8> = Vec::new();
    hdrl_body.extend_from_slice(b"hdrl");
    chunk(&mut hdrl_body, b"avih", &avih);
    hdrl_body.extend_from_slice(b"LIST");
    hdrl_body.extend_from_slice(&(strl_body.len() as u32).to_le_bytes());
    hdrl_body.extend_from_slice(&strl_body);

    // ---- empty movi LIST ----
    let mut movi_body: Vec<u8> = Vec::new();
    movi_body.extend_from_slice(b"movi");

    // ---- compose top-level RIFF AVI ----
    let mut riff_body: Vec<u8> = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(hdrl_body.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&hdrl_body);
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(movi_body.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&movi_body);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);
    out
}

#[test]
fn vbr_audio_with_nonzero_sample_size_fails_validator() {
    // Round-14 C2: MP3 (wFormatTag 0x0055) with dwSampleSize != 0
    // breaks the AVI 1.0 invariant — the muxer would write nonsense
    // dwLength values for it.
    let bytes = build_one_stream_audio_avi(0x0055, 1152);
    let reg = registry_with_magicyuv_and_pcm();
    let cur = std::io::Cursor::new(bytes);
    let rs: Box<dyn ReadSeek> = Box::new(cur);
    match demuxer_open_avi(rs, &reg) {
        Err(Error::InvalidData(msg)) => {
            assert!(
                msg.contains("VBR") && msg.contains("0x0055"),
                "error must name the offending format tag and the VBR rule (got {msg:?})"
            );
        }
        Err(other) => panic!("expected InvalidData, got {other:?}"),
        Ok(_) => panic!("expected validator to reject VBR with nonzero dwSampleSize"),
    }
}

#[test]
fn cbr_audio_with_zero_sample_size_fails_validator() {
    // Round-14 C2: PCM (wFormatTag 0x0001) with dwSampleSize == 0
    // breaks the AVI 1.0 invariant — the muxer would derive sample
    // counts as `size / 0` later. Surface as InvalidData.
    let bytes = build_one_stream_audio_avi(0x0001, 0);
    let reg = registry_with_magicyuv_and_pcm();
    let cur = std::io::Cursor::new(bytes);
    let rs: Box<dyn ReadSeek> = Box::new(cur);
    match demuxer_open_avi(rs, &reg) {
        Err(Error::InvalidData(msg)) => {
            assert!(
                msg.contains("CBR") && msg.contains("0x0001"),
                "error must name the offending format tag and the CBR rule (got {msg:?})"
            );
        }
        Err(other) => panic!("expected InvalidData, got {other:?}"),
        Ok(_) => panic!("expected validator to reject CBR with zero dwSampleSize"),
    }
}

#[test]
fn vbr_audio_with_zero_sample_size_passes_validator() {
    // Round-14 C2: the spec-compliant VBR shape (MP3 / dwSampleSize 0)
    // must pass without error.
    let bytes = build_one_stream_audio_avi(0x0055, 0);
    let reg = registry_with_magicyuv_and_pcm();
    let cur = std::io::Cursor::new(bytes);
    let rs: Box<dyn ReadSeek> = Box::new(cur);
    let dmx = demuxer_open_avi(rs, &reg).expect("VBR with zero sample_size is spec-compliant");
    assert_eq!(dmx.streams().len(), 1);
}

#[test]
fn cbr_audio_with_nonzero_sample_size_passes_validator() {
    // Round-14 C2: spec-compliant CBR (PCM / dwSampleSize > 0) must
    // pass.
    let bytes = build_one_stream_audio_avi(0x0001, 4);
    let reg = registry_with_magicyuv_and_pcm();
    let cur = std::io::Cursor::new(bytes);
    let rs: Box<dyn ReadSeek> = Box::new(cur);
    let dmx = demuxer_open_avi(rs, &reg).expect("CBR with sample_size > 0 is spec-compliant");
    assert_eq!(dmx.streams().len(), 1);
}

#[test]
fn lenient_open_skips_validator_for_malformed_vbr_audio() {
    // Round-14 C2: callers re-muxing or inspecting a malformed
    // legacy file can opt out via `open_avi_lenient` — the validator
    // is bypassed and the file opens successfully.
    let bytes = build_one_stream_audio_avi(0x0055, 1152);
    let reg = registry_with_magicyuv_and_pcm();
    let cur = std::io::Cursor::new(bytes);
    let rs: Box<dyn ReadSeek> = Box::new(cur);
    let dmx =
        open_avi_lenient(rs, &reg).expect("lenient open must accept malformed audio sample_size");
    assert_eq!(dmx.streams().len(), 1);
}

#[test]
fn unconstrained_format_tag_passes_validator() {
    // Round-14 C2: format tags not in the spec's VBR/CBR tables
    // (e.g. WMA, AC-3, custom) pass through with no constraint.
    // wFormatTag 0x0161 (WMA) with dwSampleSize 0 must NOT be
    // rejected as a CBR violation.
    let bytes = build_one_stream_audio_avi(0x0161, 0);
    let reg = registry_with_magicyuv_and_pcm();
    let cur = std::io::Cursor::new(bytes);
    let rs: Box<dyn ReadSeek> = Box::new(cur);
    let _dmx = demuxer_open_avi(rs, &reg).expect("unconstrained format tag must pass validator");
}

// ---------------------------------------------------------------------------
// C3: palette_change_typed_iter lazy iterator.
// ---------------------------------------------------------------------------

#[test]
fn palette_change_typed_iter_yields_same_sequence_as_eager() {
    // Round-14 C3: the lazy iterator returns the same PaletteChange
    // sequence as the eager Vec accessor (just one at a time).
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(20, 64);
    let reg = registry_with_magicyuv_and_pcm();

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

    let tmp = std::env::temp_dir().join("oxideav-avi-r14-pc-iter.avi");
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

    // Eager Vec form for ground truth.
    let eager = dmx.palette_change_typed(0);
    assert_eq!(eager.len(), 2);

    // Lazy iter form.
    let mut iter = dmx.palette_change_typed_iter(0);
    // ExactSizeIterator: size_hint matches the count up front.
    assert_eq!(iter.size_hint(), (2, Some(2)));
    let first = iter.next().expect("first item must be present");
    let second = iter.next().expect("second item must be present");
    assert!(
        iter.next().is_none(),
        "iterator must terminate after two items"
    );

    assert_eq!(first.unwrap(), eager[0]);
    assert_eq!(second.unwrap(), eager[1]);
}

#[test]
fn palette_change_typed_iter_empty_for_unknown_stream() {
    // Round-14 C3: out-of-range stream index returns an iterator
    // that's immediately empty (mirrors the eager accessor's
    // empty-Vec semantics for unknown streams).
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(21, 32);
    let reg = registry_with_magicyuv_and_pcm();

    let tmp = std::env::temp_dir().join("oxideav-avi-r14-pc-iter-unknown.avi");
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
    assert!(dmx.palette_change_typed_iter(0).next().is_none());
    assert!(dmx.palette_change_typed_iter(99).next().is_none());
}

#[test]
fn palette_change_typed_iter_size_hint_decrements_as_consumed() {
    // Round-14 C3: ExactSizeIterator contract — size_hint shrinks by
    // one with each next(). Useful for callers that pre-allocate a
    // sink Vec without first counting.
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(22, 32);
    let reg = registry_with_magicyuv_and_pcm();

    let pcs: Vec<PaletteChange> = (0..5)
        .map(|i| PaletteChange {
            first_entry: i as u8,
            num_entries: 1,
            flags: 0,
            entries: vec![PaletteEntry {
                red: i as u8,
                green: 0x77,
                blue: 0x99,
                flags: 0,
            }],
        })
        .collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r14-pc-iter-size-hint.avi");
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
        for pc in &pcs {
            mux.with_palette_change_typed(0, pc).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let mut iter = dmx.palette_change_typed_iter(0);
    assert_eq!(iter.len(), 5);
    let _ = iter.next().unwrap().unwrap();
    assert_eq!(iter.len(), 4);
    let _ = iter.next().unwrap().unwrap();
    assert_eq!(iter.len(), 3);
    // Drain the rest.
    let drained: Vec<PaletteChange> = iter.map(|r| r.unwrap()).collect();
    assert_eq!(drained.len(), 3);
}

#[test]
fn palette_change_typed_iter_collect_matches_eager_vec() {
    // Round-14 C3: a `.collect::<Result<Vec<_>, _>>()` over the lazy
    // iterator must reproduce the eager Vec exactly when every body
    // is well-formed.
    let stream = magicyuv_stream(64, 64, 25);
    let payload = synth_payload(23, 32);
    let reg = registry_with_magicyuv_and_pcm();

    let pcs: Vec<PaletteChange> = (0..8)
        .map(|i| PaletteChange {
            first_entry: (i * 16) as u8,
            num_entries: 2,
            flags: 0,
            entries: vec![
                PaletteEntry {
                    red: i as u8,
                    green: 0x33,
                    blue: 0x44,
                    flags: 0,
                },
                PaletteEntry {
                    red: 0x55,
                    green: 0x66,
                    blue: i as u8,
                    flags: 0,
                },
            ],
        })
        .collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r14-pc-iter-collect.avi");
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
        for pc in &pcs {
            mux.with_palette_change_typed(0, pc).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let eager = dmx.palette_change_typed(0);
    let lazy: Result<Vec<PaletteChange>, _> = dmx.palette_change_typed_iter(0).collect();
    let lazy = lazy.expect("all bodies are well-formed");
    assert_eq!(eager, lazy);
}
