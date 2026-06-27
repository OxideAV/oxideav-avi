//! Round-377: OpenDML 2.0 `dmlh` (Extended AVI Header) declared body
//! size + trailing reserved bytes read + write symmetry.
//!
//! The OpenDML 2.0 spec (`docs/container/riff/opendml-avi-2.0.pdf`
//! §"Extended AVI Header (dmlh)") documents the `dmlh` chunk body as
//! exactly one 4-byte DWORD (`dwTotalFrames`). In practice OpenDML
//! writers allocate a larger zero-padded body — the spec's own
//! worked-example layout shows `dmlh (00000054)`, an 84-byte body —
//! reserving space after `dwTotalFrames`. That trailing space is
//! unspecified, so the demuxer surfaces it verbatim.
//!
//! Covers:
//! - default muxer writes the spec-minimal 4-byte `dmlh`:
//!   `dmlh_declared_body_size() == Some(4)`, `dmlh_reserved() == Some(&[])`.
//! - `AviMuxOptions::with_dmlh_body_size(84)` writes an 84-byte body:
//!   the demuxer reports the declared size + 80 zero reserved bytes,
//!   and `dwTotalFrames` still round-trips intact.
//! - odd body sizes exercise the RIFF word-pad path.
//! - `with_dmlh_body_size(n < 4)` is clamped up to 4.
//! - AVI 1.0 mode emits no `dmlh`, so the accessors return `None`.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("mjpeg")).tag(CodecTag::fourcc(b"MJPG")));
    reg
}

fn video_stream() -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(64);
    params.height = Some(48);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn mux_video_opendml(path: &std::path::Path, frames: usize, options: AviMuxOptions) {
    let vid = video_stream();
    let streams = vec![vid.clone()];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(
        ws,
        &streams,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(65_536)),
        options,
    )
    .unwrap();
    mux.write_header().unwrap();
    for i in 0..frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 256]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();
    }
    mux.write_trailer().unwrap();
}

fn mux_video_avi10(path: &std::path::Path, frames: usize, options: AviMuxOptions) {
    let vid = video_stream();
    let streams = vec![vid.clone()];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();
    for i in 0..frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 256]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();
    }
    mux.write_trailer().unwrap();
}

fn open(path: &std::path::Path) -> oxideav_avi::demuxer::AviDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(path).unwrap());
    demuxer_open_avi(rs, &registry()).unwrap()
}

#[test]
fn default_dmlh_is_spec_minimal_four_bytes() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r377-dmlh-default.avi");
    mux_video_opendml(&tmp, 5, AviMuxOptions::default());
    let dem = open(&tmp);

    assert_eq!(dem.dmlh_declared_body_size(), Some(4));
    assert_eq!(dem.dmlh_reserved(), Some(&[][..]));
    // dwTotalFrames still surfaces.
    assert_eq!(dem.dmlh_total_frames(), Some(5));
}

#[test]
fn padded_dmlh_body_roundtrips_with_reserved_zeros() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r377-dmlh-84.avi");
    mux_video_opendml(&tmp, 7, AviMuxOptions::default().with_dmlh_body_size(84));
    let dem = open(&tmp);

    assert_eq!(dem.dmlh_declared_body_size(), Some(84));
    let reserved = dem.dmlh_reserved().unwrap();
    assert_eq!(
        reserved.len(),
        80,
        "84-byte body == 4 (total) + 80 reserved"
    );
    assert!(reserved.iter().all(|&b| b == 0), "reserved bytes are zero");
    // dwTotalFrames is unaffected by the padding.
    assert_eq!(dem.dmlh_total_frames(), Some(7));
}

#[test]
fn padded_dmlh_with_explicit_total_frames_override() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r377-dmlh-84-override.avi");
    let opts = AviMuxOptions::default()
        .with_dmlh_body_size(64)
        .with_dmlh_total_frames(123_456);
    mux_video_opendml(&tmp, 3, opts);
    let dem = open(&tmp);

    assert_eq!(dem.dmlh_declared_body_size(), Some(64));
    assert_eq!(dem.dmlh_reserved().unwrap().len(), 60);
    // The total-frames override sits in the first 4 bytes; the padding
    // does not disturb it.
    assert_eq!(dem.dmlh_total_frames(), Some(123_456));
}

#[test]
fn odd_dmlh_body_size_exercises_word_pad() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r377-dmlh-odd.avi");
    // 23-byte body: 4 (total) + 19 reserved, odd → RIFF word-pad byte.
    mux_video_opendml(&tmp, 2, AviMuxOptions::default().with_dmlh_body_size(23));
    let dem = open(&tmp);

    assert_eq!(dem.dmlh_declared_body_size(), Some(23));
    assert_eq!(dem.dmlh_reserved().unwrap().len(), 19);
    assert_eq!(dem.dmlh_total_frames(), Some(2));

    // The whole file still demuxes (the word-pad byte didn't desync the
    // following chunk walk).
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut d2 = demuxer_open_avi(rs, &registry()).unwrap();
    let mut n = 0;
    loop {
        match d2.next_packet() {
            Ok(_) => n += 1,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(n, 2);
}

#[test]
fn body_size_below_four_is_clamped_up() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r377-dmlh-clamp.avi");
    // Request 2 → clamped to 4 so dwTotalFrames always fits.
    mux_video_opendml(&tmp, 4, AviMuxOptions::default().with_dmlh_body_size(2));
    let dem = open(&tmp);

    assert_eq!(dem.dmlh_declared_body_size(), Some(4));
    assert_eq!(dem.dmlh_reserved(), Some(&[][..]));
    assert_eq!(dem.dmlh_total_frames(), Some(4));
}

#[test]
fn avi10_emits_no_dmlh() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r377-dmlh-avi10.avi");
    // The body-size override is meaningless without a dmlh; AVI 1.0
    // emits none, so the accessors stay None.
    mux_video_avi10(&tmp, 5, AviMuxOptions::default().with_dmlh_body_size(84));
    let dem = open(&tmp);

    assert_eq!(dem.dmlh_declared_body_size(), None);
    assert_eq!(dem.dmlh_reserved(), None);
    assert_eq!(dem.dmlh_total_frames(), None);
}
