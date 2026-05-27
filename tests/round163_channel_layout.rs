//! Round 163 — typed `WAVEFORMATEXTENSIBLE.dwChannelMask` surface.
//!
//! Source: `docs/container/riff/waveformatextensible/README.md`
//! (Microsoft Learn mirror, 2026-05-18) — "Channel-mask channel
//! ordering" and "Standard layouts" tables.
//!
//! Exercises the new typed accessors added in round 163:
//!
//! - [`AviDemuxer::stream_channel_mask_typed`] returns the wrapping
//!   [`ChannelMask`] preserving the raw `u32` for inspection.
//! - [`AviDemuxer::stream_channel_layout`] recognises the docs README
//!   named layouts (Mono / Stereo / 2.1 / Quad / 5.1 (back) / 5.1
//!   (side) / 7.1).
//! - `ChannelMask::iter_speakers` walks the `SPEAKER_*` positions in
//!   the docs PCM byte-stream channel order (lowest set bit first).
//! - Two new metadata keys land alongside `avi:auds.<n>.channel_mask`:
//!   `avi:auds.<n>.channel_speakers` (comma-joined abbreviations) and
//!   `avi:auds.<n>.channel_layout` (named-layout label; only present
//!   when the mask matches one of the named layouts).
//! - A non-extensible (legacy `WAVEFORMATEX`) audio stream yields
//!   `None` for both new typed accessors — the typed surface is gated
//!   on the same `wFormatTag = 0xFFFE` precondition as
//!   `stream_channel_mask`.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, Demuxer, MediaType, Muxer, Packet, ReadSeek,
    SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};
use oxideav_avi::stream_format::{
    ChannelLayout, Speaker, KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_BACK_LEFT, SPEAKER_BACK_RIGHT,
    SPEAKER_FRONT_CENTER, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, SPEAKER_LOW_FREQUENCY,
    SPEAKER_SIDE_LEFT, SPEAKER_SIDE_RIGHT,
};

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

fn write_extensible_pcm(path: &std::path::Path, channels: u16, channel_mask: u32, valid_bps: u16) {
    let stream = surround_pcm_stream("pcm_s32le", channels);
    let payload = synth_pcm_payload(16, channels, 4);
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let opts = AviMuxOptions::new().with_extensible_audio(
        0,
        channel_mask,
        valid_bps,
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

// ---------------------------------------------------------------------------
// 5.1 (Microsoft "back") — FL | FR | FC | LFE | BL | BR = 0x0000_003F.
// ---------------------------------------------------------------------------

#[test]
fn channel_layout_typed_recognises_5_1_back() {
    let mask = SPEAKER_FRONT_LEFT
        | SPEAKER_FRONT_RIGHT
        | SPEAKER_FRONT_CENTER
        | SPEAKER_LOW_FREQUENCY
        | SPEAKER_BACK_LEFT
        | SPEAKER_BACK_RIGHT;
    assert_eq!(mask, 0x0000_003F, "docs README cross-check");

    let tmp = std::env::temp_dir().join("oxideav-avi-r163-5_1-back.avi");
    write_extensible_pcm(&tmp, 6, mask, 24);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Raw and typed views agree.
    assert_eq!(dmx.stream_channel_mask(0), Some(mask));
    let cm = dmx
        .stream_channel_mask_typed(0)
        .expect("typed ChannelMask for extensible stream");
    assert_eq!(cm.raw(), mask);
    assert_eq!(cm.len(), 6, "5.1 = 6 documented bits");
    assert!(!cm.is_empty());
    assert_eq!(cm.reserved_bits(), 0);

    // PCM byte-stream channel order = lowest set bit first per docs.
    let speakers: Vec<Speaker> = cm.iter_speakers().collect();
    assert_eq!(
        speakers,
        vec![
            Speaker::FrontLeft,
            Speaker::FrontRight,
            Speaker::FrontCenter,
            Speaker::LowFrequency,
            Speaker::BackLeft,
            Speaker::BackRight,
        ],
    );

    // Named-layout recognition.
    assert_eq!(
        dmx.stream_channel_layout(0),
        Some(ChannelLayout::FivePointOneBack)
    );
    assert_eq!(cm.layout(), Some(ChannelLayout::FivePointOneBack));
}

// ---------------------------------------------------------------------------
// 5.1 (DVD-style "side") — FL | FR | FC | LFE | SL | SR = 0x0000_060F.
// ---------------------------------------------------------------------------

#[test]
fn channel_layout_typed_recognises_5_1_side() {
    let mask = SPEAKER_FRONT_LEFT
        | SPEAKER_FRONT_RIGHT
        | SPEAKER_FRONT_CENTER
        | SPEAKER_LOW_FREQUENCY
        | SPEAKER_SIDE_LEFT
        | SPEAKER_SIDE_RIGHT;
    assert_eq!(mask, 0x0000_060F, "docs README DVD-style 5.1 mask");

    let tmp = std::env::temp_dir().join("oxideav-avi-r163-5_1-side.avi");
    write_extensible_pcm(&tmp, 6, mask, 32);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_channel_layout(0),
        Some(ChannelLayout::FivePointOneSide),
    );

    let cm = dmx.stream_channel_mask_typed(0).unwrap();
    let abbrevs: Vec<&'static str> = cm.iter_speakers().map(Speaker::abbrev).collect();
    // Bit order: FC (0x4) < LFE (0x8) < FL (0x1) actually no — lowest
    // bit first: FL (0x1), FR (0x2), FC (0x4), LFE (0x8), SL (0x200),
    // SR (0x400). Verified against docs README table.
    assert_eq!(abbrevs, vec!["FL", "FR", "FC", "LFE", "SL", "SR"]);
}

// ---------------------------------------------------------------------------
// Stereo — FL | FR = 0x0000_0003.
// ---------------------------------------------------------------------------

#[test]
fn channel_layout_typed_recognises_stereo_and_emits_metadata() {
    let mask = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
    assert_eq!(mask, 0x0000_0003);

    let tmp = std::env::temp_dir().join("oxideav-avi-r163-stereo.avi");
    write_extensible_pcm(&tmp, 2, mask, 32);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_channel_layout(0), Some(ChannelLayout::Stereo));

    // Round 163 metadata keys.
    let md = dmx.metadata();
    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());

    // channel_mask still present (round-75) — sanity check.
    assert_eq!(lookup("avi:auds.0.channel_mask"), Some("0x00000003"));
    // New: comma-joined speaker abbreviations in PCM byte-stream order.
    assert_eq!(lookup("avi:auds.0.channel_speakers"), Some("FL,FR"));
    // New: named-layout label.
    assert_eq!(lookup("avi:auds.0.channel_layout"), Some("stereo"));
}

// ---------------------------------------------------------------------------
// 7.1 — FL | FR | FC | LFE | BL | BR | SL | SR = 0x0000_063F.
// ---------------------------------------------------------------------------

#[test]
fn channel_layout_typed_recognises_7_1() {
    let mask = SPEAKER_FRONT_LEFT
        | SPEAKER_FRONT_RIGHT
        | SPEAKER_FRONT_CENTER
        | SPEAKER_LOW_FREQUENCY
        | SPEAKER_BACK_LEFT
        | SPEAKER_BACK_RIGHT
        | SPEAKER_SIDE_LEFT
        | SPEAKER_SIDE_RIGHT;
    assert_eq!(mask, 0x0000_063F);

    let tmp = std::env::temp_dir().join("oxideav-avi-r163-7_1.avi");
    write_extensible_pcm(&tmp, 8, mask, 32);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_channel_layout(0),
        Some(ChannelLayout::SevenPointOne),
    );
    let cm = dmx.stream_channel_mask_typed(0).unwrap();
    assert_eq!(cm.len(), 8);
    // 7.1 channel byte order per docs README: FL, FR, FC, LFE, BL, BR,
    // SL, SR (bit-order = lowest first).
    let speakers: Vec<&'static str> = cm.iter_speakers().map(Speaker::abbrev).collect();
    assert_eq!(
        speakers,
        vec!["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]
    );
}

// ---------------------------------------------------------------------------
// Non-standard mask — typed view still works, layout() returns None.
// ---------------------------------------------------------------------------

#[test]
fn unrecognised_mask_yields_no_layout_but_decodes_bits() {
    // FL only — not one of the docs README's standard layouts (mono is
    // FC, not FL). The raw decode still surfaces FrontLeft; the
    // metadata channel_layout key is omitted; channel_speakers is
    // present (the mask is non-empty).
    let mask = SPEAKER_FRONT_LEFT;
    let tmp = std::env::temp_dir().join("oxideav-avi-r163-fl-only.avi");
    write_extensible_pcm(&tmp, 1, mask, 32);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_channel_layout(0), None);
    let cm = dmx.stream_channel_mask_typed(0).unwrap();
    assert_eq!(
        cm.iter_speakers().collect::<Vec<_>>(),
        vec![Speaker::FrontLeft]
    );
    assert_eq!(cm.layout(), None);

    let md = dmx.metadata();
    let has_key = |k: &str| md.iter().any(|(key, _)| key == k);
    assert!(
        has_key("avi:auds.0.channel_speakers"),
        "channel_speakers metadata key present for any non-empty mask"
    );
    assert!(
        !has_key("avi:auds.0.channel_layout"),
        "channel_layout key omitted when the mask doesn't match a named layout"
    );
}

// ---------------------------------------------------------------------------
// Legacy WAVEFORMATEX stream (no 0xFFFE) — typed accessors return None.
// ---------------------------------------------------------------------------

#[test]
fn legacy_waveformatex_audio_has_no_typed_channel_view() {
    // A plain `pcm_s16le` stereo stream uses the legacy 18-byte
    // WAVEFORMATEX with `wFormatTag = 0x0001` — no `dwChannelMask`
    // exists. The typed accessors must mirror `stream_channel_mask`'s
    // None gating.
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.media_type = MediaType::Audio;
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };

    let payload = synth_pcm_payload(16, 2, 2);
    let tmp = std::env::temp_dir().join("oxideav-avi-r163-legacy-wfx.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new();
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

    assert_eq!(dmx.stream_channel_mask(0), None);
    assert!(dmx.stream_channel_mask_typed(0).is_none());
    assert!(dmx.stream_channel_layout(0).is_none());

    // And no Round-163 metadata keys for a legacy stream.
    let md = dmx.metadata();
    let has_key = |k: &str| md.iter().any(|(key, _)| key == k);
    assert!(!has_key("avi:auds.0.channel_speakers"));
    assert!(!has_key("avi:auds.0.channel_layout"));

    // Sanity: codec resolved as legacy PCM.
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "pcm_s16le");
}
