//! Round-75 WAVEFORMATEXTENSIBLE (`wFormatTag = 0xFFFE`) AVI tests.
//!
//! Per docs/container/riff/waveformatextensible/ (Microsoft Learn mirror,
//! 2026-05-18). Exercises:
//!
//! - **Mux → demux round-trip** of `KSDATAFORMAT_SUBTYPE_PCM` 5.1 PCM
//!   (24-in-32 container): channel mask, valid bits per sample, and
//!   SubFormat GUID survive identically.
//! - **Per-stream metadata keys**: `avi:auds.<n>.channel_mask`,
//!   `avi:auds.<n>.valid_bits_per_sample`, `avi:auds.<n>.subformat`,
//!   `avi:auds.<n>.subformat_wformat_tag`.
//! - **Codec-id resolution from SubFormat GUID**: PCM-family GUIDs map
//!   to `pcm_*` codec ids depth-aware (mirrors the legacy
//!   `WAVEFORMATEX` path).
//! - **`IEEE_FLOAT` SubFormat**: 32-bit float resolves to `pcm_f32le`.
//! - **Builder dedup**: repeated `with_extensible_audio(0, …)` keeps
//!   only the last entry per stream index.
//! - **Mux refusal**: `params.tag = WaveFormat(0xFFFE)` without
//!   `with_extensible_audio` errors at `open_avi` time rather than
//!   silently emitting a legacy 18-byte WAVEFORMATEX.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer, Packet, Rational,
    ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};
use oxideav_avi::stream_format::{
    KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE,
};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// Build a `pcm_s24le` audio stream with 6 channels @ 48 kHz (5.1 layout
/// shape). The PCM container is 32 bits per the WAVEFORMATEX layer (the
/// muxer's `pcm_bits_per_sample` lookup for `pcm_s24le` returns 24, but
/// we override the container size by emitting `pcm_s32le` here and
/// letting the extensible extension carry `valid_bps = 24`).
fn surround_pcm_stream(codec_id: &str, channels: u16) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new(codec_id));
    params.media_type = MediaType::Audio;
    params.channels = Some(channels);
    params.sample_rate = Some(48_000);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn synth_pcm_payload(sample_count: usize, channels: u16, bytes_per_sample: usize) -> Vec<u8> {
    let n = sample_count * channels as usize * bytes_per_sample;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push((i as u8).wrapping_mul(31));
    }
    out
}

// ---------------------------------------------------------------------------
// Round-trip: 5.1 PCM (24-in-32) via WAVEFORMATEXTENSIBLE.
// ---------------------------------------------------------------------------

#[test]
fn wfex_pcm_5_1_24_in_32_roundtrip() {
    // 5.1 Microsoft channel layout per docs README:
    //   FL | FR | FC | LFE | BL | BR  = 0x0000003F
    const CHANNEL_MASK_5_1: u32 = 0x0000_003F;

    // `pcm_s32le` codec id gives a 32-bit container; `valid_bps = 24`
    // captures the 24-bit precision in the extension union.
    let channels = 6u16;
    let stream = surround_pcm_stream("pcm_s32le", channels);
    let payload = synth_pcm_payload(64, channels, 4);

    let tmp = std::env::temp_dir().join("oxideav-avi-r75-wfex-pcm5_1.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_extensible_audio(
            0,
            CHANNEL_MASK_5_1,
            24,
            KSDATAFORMAT_SUBTYPE_PCM,
        );
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload.clone());
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Typed accessors recover every extensible field exactly.
    assert_eq!(
        dmx.stream_channel_mask(0),
        Some(CHANNEL_MASK_5_1),
        "channel mask must survive mux→demux"
    );
    assert_eq!(
        dmx.stream_valid_bits_per_sample(0),
        Some(24),
        "valid_bps=24 must survive mux→demux"
    );
    let recovered = dmx.stream_subformat(0).expect("SubFormat GUID present");
    assert_eq!(recovered, KSDATAFORMAT_SUBTYPE_PCM);

    // params.tag stamps wFormatTag = 0xFFFE for round-trip fidelity.
    let p = &dmx.streams()[0].params;
    assert_eq!(p.tag, Some(CodecTag::wave_format(WAVE_FORMAT_EXTENSIBLE)));

    // PCM SubFormat + 24 valid bits ⇒ `pcm_s24le` resolution (depth-aware,
    // matching the legacy `audio_codec_id_fallback`).
    assert_eq!(p.codec_id.as_str(), "pcm_s24le");
    assert_eq!(p.channels, Some(channels));
    assert_eq!(p.sample_rate, Some(48_000));
}

// ---------------------------------------------------------------------------
// Metadata keys cover all four extensible-only facts.
// ---------------------------------------------------------------------------

#[test]
fn wfex_metadata_keys_present() {
    let stream = surround_pcm_stream("pcm_s32le", 2);
    let payload = synth_pcm_payload(8, 2, 4);
    let tmp = std::env::temp_dir().join("oxideav-avi-r75-wfex-meta.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_extensible_audio(
            0,
            0x0000_0003, /* FL | FR */
            32,          /* valid == container */
            KSDATAFORMAT_SUBTYPE_PCM,
        );
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let md = dmx.metadata();
    let has = |k: &str, want: &str| md.iter().any(|(key, val)| key == k && val == want);

    assert!(
        has("avi:auds.0.channel_mask", "0x00000003"),
        "channel_mask key missing: {:?}",
        md.iter()
            .filter(|(k, _)| k.starts_with("avi:auds.0"))
            .collect::<Vec<_>>()
    );
    assert!(has("avi:auds.0.valid_bits_per_sample", "32"));
    assert!(has(
        "avi:auds.0.subformat",
        "00000001-0000-0010-8000-00aa00389b71",
    ));
    assert!(has("avi:auds.0.subformat_wformat_tag", "0x0001"));
}

// ---------------------------------------------------------------------------
// IEEE_FLOAT SubFormat → `pcm_f32le` codec id resolution.
// ---------------------------------------------------------------------------

#[test]
fn wfex_ieee_float_subformat_resolves_to_pcm_f32le() {
    let stream = surround_pcm_stream("pcm_f32le", 2);
    let payload = synth_pcm_payload(4, 2, 4);
    let tmp = std::env::temp_dir().join("oxideav-avi-r75-wfex-float.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_extensible_audio(
            0,
            0x0000_0003,
            32,
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, payload);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    let p = &dmx.streams()[0].params;
    assert_eq!(p.codec_id.as_str(), "pcm_f32le");
    assert_eq!(
        dmx.stream_subformat(0),
        Some(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)
    );
}

// ---------------------------------------------------------------------------
// Builder dedup.
// ---------------------------------------------------------------------------

#[test]
fn with_extensible_audio_dedups_per_stream_index() {
    let opts = AviMuxOptions::new()
        .with_extensible_audio(0, 0x0003, 16, KSDATAFORMAT_SUBTYPE_PCM)
        .with_extensible_audio(0, 0x003F, 24, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)
        .with_extensible_audio(2, 0x0003, 16, KSDATAFORMAT_SUBTYPE_PCM);
    assert_eq!(opts.extensible_audio_streams.len(), 2);
    // The later registration for stream 0 wins.
    let (idx0, mask0, valid0, guid0) = opts.extensible_audio_streams[0];
    assert_eq!(idx0, 0);
    assert_eq!(mask0, 0x003F);
    assert_eq!(valid0, 24);
    assert_eq!(guid0, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
}

// ---------------------------------------------------------------------------
// Non-audio streams: silent no-op (the helper only fires on `auds`).
// ---------------------------------------------------------------------------

#[test]
fn wfex_silently_ignores_non_audio_streams() {
    // Pretend the user accidentally registered stream 0 for extensible
    // audio but actually fed in a video stream. The muxer should still
    // open and the helper just doesn't fire — i.e. the video strf is
    // still a BMIH, not a WAVEFORMATEXTENSIBLE.
    let mut p = CodecParameters::video(CodecId::new("rgb24"));
    p.media_type = MediaType::Video;
    p.width = Some(32);
    p.height = Some(32);
    p.frame_rate = Some(Rational::new(25, 1));
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params: p,
    };
    let payload = vec![0u8; 32 * 32 * 3];
    let tmp = std::env::temp_dir().join("oxideav-avi-r75-wfex-mis-stream.avi");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let opts = AviMuxOptions::new().with_extensible_audio(0, 0x0003, 16, KSDATAFORMAT_SUBTYPE_PCM);
    let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
    mux.write_header().unwrap();
    let mut pkt = Packet::new(0, stream.time_base, payload);
    pkt.pts = Some(0);
    pkt.flags.keyframe = true;
    mux.write_packet(&pkt).unwrap();
    mux.write_trailer().unwrap();

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    // Video stream — no audio strf info.
    assert!(dmx.stream_audio_strf(0).is_none());
}

// ---------------------------------------------------------------------------
// Mux refusal: tag=0xFFFE without `with_extensible_audio` errors.
// ---------------------------------------------------------------------------

#[test]
fn mux_rejects_extensible_tag_without_helper() {
    // `params.tag = WaveFormat(0xFFFE)` but no `with_extensible_audio`
    // registration: the muxer can't synthesise the SubFormat GUID, so it
    // refuses at `open_avi` time rather than silently emitting a broken
    // 18-byte WAVEFORMATEX.
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"))
        .with_tag(CodecTag::wave_format(WAVE_FORMAT_EXTENSIBLE));
    p.channels = Some(2);
    p.sample_rate = Some(48_000);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: p,
    };
    let tmp = std::env::temp_dir().join("oxideav-avi-r75-wfex-reject.avi");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let r = open_avi(
        ws,
        std::slice::from_ref(&stream),
        AviKind::Avi10,
        AviMuxOptions::new(),
    );
    assert!(r.is_err(), "expected error on bare 0xFFFE without helper");
}
