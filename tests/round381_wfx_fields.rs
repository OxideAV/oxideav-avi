//! Round-381 milestone 3: raw `WAVEFORMATEX` scalar fields surfaced for
//! ANY audio stream — `nAvgBytesPerSec` and `wBitsPerSample`.
//!
//! Per RFC 2361 §"WAVEFORMATEX"
//! (`docs/container/riff/rfc2361-wav.txt`):
//!   - `nAvgBytesPerSec` — the required average data-transfer rate, in
//!     bytes per second, for the format tag; for CBR PCM it equals
//!     `nSamplesPerSec × nBlockAlign`.
//!   - `wBitsPerSample` — bits per sample for `WAVE_FORMAT_PCM`; for a
//!     compressed format *"should be set to ... the value most convenient
//!     ... or to zero if not applicable."*
//!
//! The two accessors fold the `0` "unspecified / not applicable" stamp to
//! `None`, and the `avi:auds.<n>.*` metadata keys are emitted only for the
//! non-zero value. Distinct from the typed `CodecParameters::bit_rate`
//! (= `nAvgBytesPerSec × 8`), which the demuxer derives separately.
//!
//! Clean-room source:
//!   - `docs/container/riff/rfc2361-wav.txt` §WAVEFORMATEX
//!   - `docs/container/riff/avi-riff-file-reference.md`

use oxideav_avi::demuxer::open_avi;
use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Packet,
    ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{open_with_options, AviKind, AviMuxOptions};

// --- raw-AVI builder for the explicit-field parse cases ---------------

fn chunk(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len() + 1);
    out.extend_from_slice(fourcc);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn list(form: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(form);
    for c in children {
        body.extend_from_slice(c);
    }
    chunk(b"LIST", &body)
}

/// A 16-byte WAVEFORMATEX (no cbSize) with explicit fields.
fn wfx(
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&format_tag.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&samples_per_sec.to_le_bytes());
    b.extend_from_slice(&avg_bytes_per_sec.to_le_bytes());
    b.extend_from_slice(&block_align.to_le_bytes());
    b.extend_from_slice(&bits_per_sample.to_le_bytes());
    b
}

fn build_audio_avi(strf: &[u8]) -> Vec<u8> {
    // For a CBR format-tag (PCM 0x0001), the open()-time sample-size
    // invariant requires strh.dwSampleSize > 0. Derive it from the
    // strf's nBlockAlign (byte offset 12 of the WAVEFORMATEX). VBR tags
    // require 0.
    let format_tag = u16::from_le_bytes([strf[0], strf[1]]);
    let block_align = u16::from_le_bytes([strf[12], strf[13]]);
    let sample_size: u32 = if matches!(format_tag, 0x0001 | 0x0006 | 0x0007 | 0x0011) {
        block_align.max(1) as u32
    } else {
        0
    };

    let mut avih = vec![0u8; 56];
    avih[24..28].copy_from_slice(&1u32.to_le_bytes()); // dwStreams = 1

    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"auds");
    strh[20..24].copy_from_slice(&1u32.to_le_bytes()); // dwScale = 1
    strh[24..28].copy_from_slice(&44100u32.to_le_bytes()); // dwRate
    strh[44..48].copy_from_slice(&sample_size.to_le_bytes()); // dwSampleSize

    let strl = list(b"strl", &[chunk(b"strh", &strh), chunk(b"strf", strf)]);
    let hdrl = list(b"hdrl", &[chunk(b"avih", &avih), strl]);

    let mut movi_body = Vec::new();
    movi_body.extend_from_slice(b"movi");
    movi_body.extend_from_slice(&chunk(b"00wb", &[0u8; 8]));
    let movi = chunk(b"LIST", &movi_body);

    let mut form_body = Vec::new();
    form_body.extend_from_slice(b"AVI ");
    form_body.extend_from_slice(&hdrl);
    form_body.extend_from_slice(&movi);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(form_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&form_body);
    out
}

fn open(bytes: Vec<u8>) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = CodecRegistry::new();
    let input: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    open_avi(input, &reg).expect("open_avi")
}

#[test]
fn pcm_avg_bytes_and_bits_surfaced() {
    // 16-bit stereo PCM @ 44100: nBlockAlign = 4, nAvgBytesPerSec = 176400.
    let strf = wfx(0x0001, 2, 44100, 176400, 4, 16);
    let dmx = open(build_audio_avi(&strf));
    assert_eq!(dmx.stream_avg_bytes_per_sec(0), Some(176400));
    assert_eq!(dmx.stream_bits_per_sample(0), Some(16));

    let md = dmx.metadata();
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:auds.0.avg_bytes_per_sec")
            .map(|(_, v)| v.as_str()),
        Some("176400")
    );
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:auds.0.bits_per_sample")
            .map(|(_, v)| v.as_str()),
        Some("16")
    );
}

#[test]
fn vbr_bits_zero_folds_to_none() {
    // An MP3-tagged stream (0x0055) with wBitsPerSample = 0 ("not
    // applicable") but a real average byte rate.
    let strf = wfx(0x0055, 2, 44100, 16000, 1, 0);
    let dmx = open(build_audio_avi(&strf));
    assert_eq!(dmx.stream_bits_per_sample(0), None, "0 ⇒ not applicable");
    assert_eq!(dmx.stream_avg_bytes_per_sec(0), Some(16000));

    // bits_per_sample key omitted; avg key present.
    let md = dmx.metadata();
    assert!(!md.iter().any(|(k, _)| k == "avi:auds.0.bits_per_sample"));
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:auds.0.avg_bytes_per_sec")
            .map(|(_, v)| v.as_str()),
        Some("16000")
    );
}

#[test]
fn avg_bytes_zero_folds_to_none() {
    let strf = wfx(0x0001, 1, 8000, 0, 1, 8);
    let dmx = open(build_audio_avi(&strf));
    assert_eq!(dmx.stream_avg_bytes_per_sec(0), None);
    assert!(!dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:auds.0.avg_bytes_per_sec"));
}

#[test]
fn accessors_none_for_out_of_range() {
    let strf = wfx(0x0001, 2, 44100, 176400, 4, 16);
    let dmx = open(build_audio_avi(&strf));
    assert_eq!(dmx.stream_avg_bytes_per_sec(5), None);
    assert_eq!(dmx.stream_bits_per_sample(5), None);
}

// --- muxer round-trip: the muxer auto-derives both for PCM ------------

fn pcm_registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    reg
}

#[test]
fn pcm_mux_roundtrips_derived_avg_and_bits() {
    let mut params =
        CodecParameters::audio(CodecId::new("pcm_s16le")).with_tag(CodecTag::wave_format(0x0001));
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

    let tmp = std::env::temp_dir().join("oxideav-avi-r381-pcm.avi");
    let ws: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&tmp).unwrap());
    let mut mux = open_with_options(
        ws,
        std::slice::from_ref(&stream),
        AviKind::Avi10,
        AviMuxOptions::new(),
    )
    .unwrap();
    mux.write_header().unwrap();
    for i in 0..4 {
        let mut pkt = Packet::new(0, stream.time_base, vec![0u8; 192]);
        pkt.pts = Some(i as i64);
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = open_avi(rs, &pcm_registry()).unwrap();

    // 16-bit stereo @ 48000 ⇒ nBlockAlign 4 ⇒ nAvgBytesPerSec 192000.
    assert_eq!(dmx.stream_avg_bytes_per_sec(0), Some(192_000));
    assert_eq!(dmx.stream_bits_per_sample(0), Some(16));
}

#[test]
fn avg_bytes_override_replaces_computed() {
    let mut params =
        CodecParameters::audio(CodecId::new("pcm_s16le")).with_tag(CodecTag::wave_format(0x0001));
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

    let tmp = std::env::temp_dir().join("oxideav-avi-r381-avg-override.avi");
    let ws: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&tmp).unwrap());
    // Override the computed 192000 with a nominal 200000.
    let opts = AviMuxOptions::new().with_avg_bytes_per_sec(0, 200_000);
    let mut mux =
        open_with_options(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
    mux.write_header().unwrap();
    for i in 0..3 {
        let mut pkt = Packet::new(0, stream.time_base, vec![0u8; 192]);
        pkt.pts = Some(i as i64);
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = open_avi(rs, &pcm_registry()).unwrap();

    // The override replaced the computed value; wBitsPerSample +
    // nBlockAlign (and thus the CBR sample-size invariant) untouched.
    assert_eq!(dmx.stream_avg_bytes_per_sec(0), Some(200_000));
    assert_eq!(dmx.stream_bits_per_sample(0), Some(16));
    assert_eq!(dmx.stream_block_align(0), Some(4));
    assert_eq!(
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == "avi:auds.0.avg_bytes_per_sec")
            .map(|(_, v)| v.as_str()),
        Some("200000")
    );
}
