//! Round 349 — per-packet keyframe flags surfaced through the demux
//! read path.
//!
//! AVI payload chunk headers in `movi` carry no keyframe bit; the only
//! per-chunk random-access metadata is the `idx1` chunk's
//! `AVIIF_KEYFRAME` (`0x10`) — per
//! `docs/container/riff/avi-riff-file-reference.md` Appendix C, *"The
//! chunk is a key frame."* — and, for OpenDML 2.0 files, the `ix##`
//! (`AVISTDINDEX`) per-entry `dwSize` high bit (*"high bit set =>
//! non-keyframe (delta)"*).
//!
//! Before this round the demuxer hard-coded `keyframe = true` on every
//! packet it returned, so a player or seek consumer could not tell an
//! I-frame from a P-frame. These tests stage AVIs with a deliberate
//! key/delta pattern through the muxer (which writes the index flags
//! honestly) and assert the demuxer now reflects the true per-chunk
//! flag.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, MediaType, Packet, PixelFormat,
    Rational, StreamInfo, TimeBase,
};
use oxideav_core::{ReadSeek, WriteSeek};

fn register_fake_video(reg: &mut CodecRegistry, codec_id: &str, fourcc: &[u8; 4]) {
    let info = CodecInfo::new(CodecId::new(codec_id)).tag(CodecTag::fourcc(fourcc));
    reg.register(info);
}

fn video_stream(fourcc: &[u8; 4], codec_id: &str) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new(codec_id)).with_tag(CodecTag::fourcc(fourcc));
    params.media_type = MediaType::Video;
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Distinctive opaque payload so a body mismatch is obvious.
fn frame_bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A GOP-like keyframe pattern: index 0 is a keyframe, then a run of
/// delta frames, periodically refreshed. `true` = keyframe.
fn keyframe_pattern(n: usize) -> Vec<bool> {
    (0..n).map(|i| i % 5 == 0).collect()
}

#[test]
fn idx1_keyframe_flags_roundtrip() {
    let stream = video_stream(b"XVID", "mpeg4");
    let pattern = keyframe_pattern(13);

    let mut reg = CodecRegistry::new();
    register_fake_video(&mut reg, "mpeg4", b"XVID");

    let tmp = std::env::temp_dir().join("oxideav-avi-r349-idx1-keyframes.avi");
    let mut sent: Vec<Vec<u8>> = Vec::new();
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for (i, &is_key) in pattern.iter().enumerate() {
            let body = frame_bytes(i as u8, 48 + i);
            sent.push(body.clone());
            let mut pkt = Packet::new(0, stream.time_base, body);
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = is_key;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();

    let mut got_flags: Vec<bool> = Vec::new();
    let mut got_bodies: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                got_flags.push(p.flags.keyframe);
                got_bodies.push(p.data);
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }

    assert_eq!(got_bodies, sent, "payload roundtrip mismatch");
    assert_eq!(
        got_flags, pattern,
        "per-packet keyframe flags must match the staged GOP pattern, \
         not be blanket-true"
    );
    // Sanity: the pattern genuinely contains delta frames, otherwise the
    // test would pass even with the old blanket-true behaviour.
    assert!(
        got_flags.iter().any(|&k| !k),
        "test fixture must contain at least one delta frame"
    );
}

#[test]
fn all_keyframe_stream_stays_all_keyframe() {
    // A stream where every frame is a keyframe (e.g. MJPEG / uncompressed)
    // must still report every packet as a keyframe.
    let stream = video_stream(b"MJPG", "mjpeg");
    let n = 6;

    let mut reg = CodecRegistry::new();
    register_fake_video(&mut reg, "mjpeg", b"MJPG");

    let tmp = std::env::temp_dir().join("oxideav-avi-r349-allkey.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for i in 0..n {
            let mut pkt = Packet::new(0, stream.time_base, frame_bytes(i as u8, 40));
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_avi::demuxer::open(rs, &reg).unwrap();
    let mut count = 0;
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                assert!(p.flags.keyframe, "all-keyframe stream lost a keyframe flag");
                count += 1;
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(count, n);
}
