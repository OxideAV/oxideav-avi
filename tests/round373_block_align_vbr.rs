//! Round-373: raw `nBlockAlign` accessor + VBR/CBR classification.
//!
//! Per AVI 1.0 §"AVISTREAMHEADER" the audio per-stream time base
//! "corresponds to the time needed to play nBlockAlign bytes of
//! audio", and the §"AVISTREAMHEADER" `dwSampleSize` row pins the
//! VBR/CBR split: VBR codecs (MP3 etc.) carry `dwSampleSize == 0`,
//! CBR codecs (PCM etc.) carry the fixed `nBlockAlign`.
//!
//! `stream_block_align(stream) -> Option<u16>` surfaces the raw
//! `WAVEFORMATEX.nBlockAlign` for ANY audio stream (the round-96
//! `cbr_audio_block_alignment_violations` only consults CBR streams
//! with `nBlockAlign > 1`). `audio_is_vbr(stream) -> Option<bool>`
//! classifies the stream from its `wFormatTag`.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, MediaType, Muxer, Packet,
    Rational, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi as muxer_open_avi, AviKind, AviMuxOptions};

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    // WAVE_FORMAT_MPEGLAYER3 == 0x0055 (VBR — dwSampleSize == 0).
    reg.register(CodecInfo::new(CodecId::new("mp3")).tag(CodecTag::wave_format(0x0055)));
    reg
}

fn video_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(64);
    params.height = Some(64);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Stereo s16 PCM: nBlockAlign = 2ch * 2B = 4 (CBR).
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

/// VBR MP3 stream (dwSampleSize == 0).
fn mp3_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::audio(CodecId::new("mp3")).with_tag(CodecTag::wave_format(0x0055));
    params.media_type = MediaType::Audio;
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Mux video(0) + pcm(1) + mp3(2). MP3 packets carry a per-packet
/// `duration` so the VBR `strh.dwLength` derivation works.
fn mux_three_streams(name: &str) -> std::path::PathBuf {
    let vid = video_stream(0);
    let pcm = pcm_stream(1);
    let mp3 = mp3_stream(2);
    let streams = vec![vid.clone(), pcm.clone(), mp3.clone()];

    let tmp = std::env::temp_dir().join(name);
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = muxer_open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::new()).unwrap();
    mux.write_header().unwrap();
    for i in 0..4 {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 256]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();

        // PCM: 4-byte-aligned payloads.
        let mut apkt = Packet::new(1, pcm.time_base, vec![0u8; 64]);
        apkt.pts = Some(i as i64);
        apkt.flags.keyframe = true;
        mux.write_packet(&apkt).unwrap();

        // MP3: VBR — give each packet a duration so dwLength works.
        let mut mpkt = Packet::new(2, mp3.time_base, vec![0xAAu8; 417]);
        mpkt.pts = Some(i as i64);
        mpkt.duration = Some(1152);
        mpkt.flags.keyframe = true;
        mux.write_packet(&mpkt).unwrap();
    }
    mux.write_trailer().unwrap();
    tmp
}

#[test]
fn block_align_and_vbr_classification_per_stream() {
    let path = mux_three_streams("oxideav-avi-r373-blockalign.avi");
    let f = std::fs::File::open(&path).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    // Video stream 0: not audio.
    assert_eq!(dem.stream_block_align(0), None);
    assert_eq!(dem.audio_is_vbr(0), None);

    // PCM stream 1: CBR, nBlockAlign = 2ch * 2B = 4.
    assert_eq!(dem.stream_block_align(1), Some(4));
    assert_eq!(dem.audio_is_vbr(1), Some(false));

    // MP3 stream 2: VBR. nBlockAlign for VBR is advisory (typically 1);
    // whatever the muxer stamped, the VBR classification is the key
    // signal and `nBlockAlign == 1` maps through the accessor verbatim.
    assert_eq!(dem.audio_is_vbr(2), Some(true));
    // The accessor returns the raw value (or None for a 0 stamp); for a
    // VBR stream the muxer stamps nBlockAlign = 1.
    let mp3_ba = dem.stream_block_align(2);
    assert!(
        mp3_ba == Some(1) || mp3_ba.is_none(),
        "VBR nBlockAlign should be 1 or unspecified, got {mp3_ba:?}"
    );

    // Out-of-range stream index: None, never panics.
    assert_eq!(dem.stream_block_align(99), None);
    assert_eq!(dem.audio_is_vbr(99), None);
}
