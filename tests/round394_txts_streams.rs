//! Round 394 — first-class `txts` text streams.
//!
//! Clean-room source: AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, `fccType` row):
//! *"The following standard AVI values are defined: `auds` (audio
//! stream), `mids` (MIDI stream), `txts` (text stream), `vids`
//! (video stream)."* A declared `txts` stream's `movi` chunks carry
//! the `##tx` suffix (the `mmsystem.h` text-chunk family).
//!
//! Pre-round-394 a declared text stream was represented as
//! `MediaType::Data` with the never-matching `xx` packet suffix, and
//! `next_packet` unconditionally side-banded every `##tx` chunk — so
//! the stream's own packets were unreachable through the packet API.
//! Now: demux delivers `##tx` chunks as packets on the declared
//! stream (`MediaType::Subtitle`, codec id `avi:txts`, `strf` bytes
//! surfaced verbatim as extradata), while the side-band
//! `text_chunk_*` surfaces remain for `##tx` chunks riding on
//! non-text streams. Mux accepts `MediaType::Subtitle` streams:
//! `fccType = txts`, packet suffix `tx`, `strf` = extradata verbatim,
//! `(dwScale, dwRate)` from the stream's own time base.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer as _, MediaType,
    Muxer as _, Packet, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG"));
    reg.register(info);
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

fn audio_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn text_stream(index: u32, extradata: &[u8]) -> StreamInfo {
    let codec_id = CodecId::new("avi:txts");
    let mut params = CodecParameters::audio(codec_id);
    params.media_type = MediaType::Subtitle;
    params.extradata = extradata.to_vec();
    StreamInfo {
        index,
        // Millisecond ticks — a natural subtitle timing base.
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn payload(seed: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed.wrapping_mul(0x9E37_79B9).wrapping_add(3);
    for _ in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 24) as u8);
    }
    out
}

fn text_line(i: usize) -> Vec<u8> {
    format!("subtitle line {i} at {}ms", i * 500).into_bytes()
}

/// Mux video (stream 0) + audio (stream 1) + text (stream 2).
fn mux_avt(tag: &str, n: usize, kind: AviKind, opts: AviMuxOptions) -> Vec<u8> {
    let streams = [
        video_stream(0),
        audio_stream(1),
        text_stream(2, b"fmt-blob"),
    ];
    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r394-txts-{tag}.avi"));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(ws, &streams, kind, opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..n {
            let mut v = Packet::new(0, streams[0].time_base, payload(i as u32, 400));
            v.pts = Some(i as i64);
            v.flags.keyframe = true;
            mux.write_packet(&v).unwrap();
            let mut a = Packet::new(1, streams[1].time_base, payload(0xA000 + i as u32, 96));
            a.pts = Some(i as i64 * 24);
            a.flags.keyframe = true;
            mux.write_packet(&a).unwrap();
            let mut t = Packet::new(2, streams[2].time_base, text_line(i));
            t.pts = Some(i as i64 * 500);
            t.flags.keyframe = true;
            mux.write_packet(&t).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn open_dmx(bytes: &[u8]) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.to_vec()));
    oxideav_avi::demuxer::open_avi(rs, &reg).unwrap()
}

#[test]
fn txts_stream_roundtrip_avi10() {
    let bytes = mux_avt("avi10", 6, AviKind::Avi10, AviMuxOptions::default());
    let dmx = open_dmx(&bytes);

    // Stream declaration round-trips.
    let s2 = &dmx.streams()[2];
    assert_eq!(s2.params.media_type, MediaType::Subtitle);
    assert_eq!(s2.params.codec_id.as_str(), "avi:txts");
    assert_eq!(
        s2.params.extradata, b"fmt-blob",
        "text strf bytes surface verbatim as extradata"
    );
    assert_eq!(dmx.stream_fcc_type(2), Some(*b"txts"));
    assert_eq!(
        dmx.stream_timebase(2),
        Some((1, 1000)),
        "text (dwScale, dwRate) from the stream time base"
    );

    // The declared text stream's tx chunks are NOT side-band...
    assert_eq!(
        dmx.text_chunk_count(2),
        0,
        "declared txts stream's chunks are packets, not side-band"
    );

    // ...they arrive as packets, in order, byte-exact.
    let mut dmx = dmx;
    let mut texts: Vec<Vec<u8>> = Vec::new();
    let (mut v, mut a) = (0u32, 0u32);
    loop {
        match dmx.next_packet() {
            Ok(p) => match p.stream_index {
                0 => {
                    assert_eq!(p.data, payload(v, 400));
                    v += 1;
                }
                1 => {
                    assert_eq!(p.data, payload(0xA000 + a, 96));
                    a += 1;
                }
                2 => texts.push(p.data),
                other => panic!("unexpected stream {other}"),
            },
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!((v, a), (6, 6));
    assert_eq!(texts.len(), 6, "all text packets delivered");
    for (i, t) in texts.iter().enumerate() {
        assert_eq!(t, &text_line(i), "text packet {i} byte-exact");
    }
}

#[test]
fn txts_stream_roundtrip_opendml() {
    let bytes = mux_avt(
        "odml",
        6,
        AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        AviMuxOptions::default(),
    );
    // The text stream gets its own ix02 std index + indx super-index
    // whose dwChunkId spells the text packet FourCC.
    assert!(bytes.windows(4).any(|w| w == b"ix02"));
    let dmx = open_dmx(&bytes);
    assert_eq!(dmx.super_index_chunk_id(2), Some(*b"02tx"));
    let entries = dmx.super_index_entries(2).expect("text stream indx");
    assert_eq!(entries.len(), 1);
    assert!(dmx.super_index_target_violations().is_empty());
    assert_eq!(dmx.std_index_base_offsets(2).len(), 1);

    // Per-packet keyframe flags reachable for the text stream too.
    for seq in 0..6 {
        assert_eq!(dmx.packet_is_keyframe(2, seq), Some(true));
    }

    let mut dmx = dmx;
    let mut texts = 0;
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                if p.stream_index == 2 {
                    assert_eq!(p.data, text_line(texts));
                    texts += 1;
                }
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(texts, 6);
}

#[test]
fn sideband_tx_on_video_stream_stays_sideband() {
    // ##tx chunks written under a VIDEO stream's slot (the round-10
    // side-band shape) keep the side-band semantics: counted +
    // buffered via text_chunk_*, never delivered as packets.
    let streams = [video_stream(0)];
    let tmp = std::env::temp_dir().join("oxideav-avi-r394-txts-sideband.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux =
            oxideav_avi::muxer::open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::default())
                .unwrap();
        mux.write_header().unwrap();
        for i in 0..4 {
            let mut v = Packet::new(0, streams[0].time_base, payload(i, 300));
            v.pts = Some(i as i64);
            v.flags.keyframe = true;
            mux.write_packet(&v).unwrap();
            mux.write_text_chunk(0, format!("overlay {i}").as_bytes())
                .unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);

    let dmx = open_dmx(&bytes);
    assert_eq!(dmx.text_chunk_count(0), 4, "side-band count intact");
    assert_eq!(dmx.text_chunk_data(0).len(), 4, "side-band bodies buffered");
    assert_eq!(dmx.text_chunk_data(0)[2], b"overlay 2");
    let mut dmx = dmx;
    let mut n = 0;
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                assert_eq!(p.stream_index, 0);
                n += 1;
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(n, 4, "only video packets in the packet stream");
}

#[test]
fn txts_stream_empty_extradata_gets_empty_strf() {
    // No format blob → zero-length strf, still structurally valid and
    // round-trips as empty extradata.
    let streams = [video_stream(0), text_stream(1, b"")];
    let tmp = std::env::temp_dir().join("oxideav-avi-r394-txts-empty.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux =
            oxideav_avi::muxer::open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::default())
                .unwrap();
        mux.write_header().unwrap();
        let mut v = Packet::new(0, streams[0].time_base, payload(0, 200));
        v.pts = Some(0);
        v.flags.keyframe = true;
        mux.write_packet(&v).unwrap();
        let mut t = Packet::new(1, streams[1].time_base, b"hello".to_vec());
        t.pts = Some(0);
        t.flags.keyframe = true;
        mux.write_packet(&t).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);

    let dmx = open_dmx(&bytes);
    let s1 = &dmx.streams()[1];
    assert_eq!(s1.params.media_type, MediaType::Subtitle);
    assert!(s1.params.extradata.is_empty());
    let mut dmx = dmx;
    let mut saw_text = false;
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                if p.stream_index == 1 {
                    assert_eq!(p.data, b"hello");
                    saw_text = true;
                }
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert!(saw_text);
}
