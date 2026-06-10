//! Round-275 file-global `avih.dwWidth` / `dwHeight` movie-rectangle
//! typed accessor.
//!
//! `dwWidth` / `dwHeight` are the 32-bit DWORDs at byte offsets 32 + 36
//! of the 56-byte AVIMAINHEADER body per AVI 1.0 §"AVIMAINHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
//! `dwWidth` row line 203 + `dwHeight` row line 204): *"Width of the AVI
//! file in pixels."* / *"Height of the AVI file in pixels."*
//!
//! This is the file-global movie rectangle the per-stream `strh.rcFrame`
//! destination rectangle (round-115) is expressed relative to. Per the
//! spec's `rcFrame` row (line 248): the destination rectangle is *"within
//! the movie rectangle specified by the `dwWidth` and `dwHeight` members
//! of the AVI main header structure"* and its upper-left corner is
//! *"relative to the upper-left corner of the movie rectangle."*
//!
//! Pre-round-275 the demuxer already parsed both DWORDs and surfaced them
//! as the `avi:width` / `avi:height` metadata keys (verbatim, including
//! 0), but no typed accessor existed. Round-275 adds:
//!
//! - `AviDemuxer::avih_movie_rect() -> Option<(u32, u32)>` returning the
//!   `(width, height)` pair, with the "either DWORD zero ⇒ None"
//!   "default == absent" mapping (matching the round-249
//!   `stream_timebase` "zero in either DWORD ⇒ None" shape) that the raw
//!   metadata keys deliberately don't apply.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip**: the muxer's auto-derived stamp (first
//!   video stream's coded width/height) surfaces verbatim via the typed
//!   accessor.
//! - **Accessor / metadata agreement** on the on-disk byte pattern.
//! - **Hand-rolled fixture**: explicit non-default `dwWidth`/`dwHeight`
//!   in a 56-byte avih decode to the expected raw pair.
//! - **Hand-rolled fixture**: a zero in either dimension collapses the
//!   pair to `None`.
//! - **Independence from the per-stream `strh.rcFrame`**: a fixture
//!   stamping a movie rectangle larger than a stream's `rcFrame`
//!   surfaces both verbatim through their separate accessors.
//! - **Independence from neighbouring AVIMAINHEADER DWORDs**: the
//!   round-268 `dwTotalFrames` (offset 16) and round-157
//!   `dwInitialFrames` (offset 20) read back their own bytes alongside a
//!   stamped movie rectangle.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer, Packet, Rational,
    ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn video_stream(index: u32, w: u32, h: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(w);
    params.height = Some(h);
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
    params.media_type = MediaType::Audio;
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

/// Mux a video+audio AVI 1.0 file with the requested video frame size.
fn write_video(path: &std::path::Path, w: u32, h: u32) {
    let streams = [video_stream(0, w, h), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::new()).unwrap();
    mux.write_header().unwrap();

    for i in 0..3 {
        let mut v = Packet::new(0, streams[0].time_base, vec![0x55u8; 64]);
        v.pts = Some(i as i64);
        v.flags.keyframe = true;
        mux.write_packet(&v).unwrap();
    }

    let mut a = Packet::new(1, streams[1].time_base, vec![0u8; 8]);
    a.pts = Some(0);
    a.flags.keyframe = true;
    mux.write_packet(&a).unwrap();

    mux.write_trailer().unwrap();
}

// ---------------------------------------------------------------------------
// Mux → demux round-trip: the muxer's auto-derived movie rectangle (first
// video stream's coded width/height) surfaces verbatim via the typed
// accessor.
// ---------------------------------------------------------------------------

#[test]
fn muxer_auto_stamp_roundtrips_via_accessor() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r275-movie-rect.avi");
    write_video(&tmp, 320, 240);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_movie_rect(), Some((320, 240)));
}

// ---------------------------------------------------------------------------
// Accessor / metadata agreement on the on-disk byte pattern.
// ---------------------------------------------------------------------------

#[test]
fn accessor_agrees_with_metadata_keys() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r275-movie-rect-agree.avi");
    write_video(&tmp, 176, 144);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let from_accessor = dmx.avih_movie_rect();
    assert!(from_accessor.is_some(), "176x144 fixture stamps a rect");

    let md = dmx.metadata();
    let w: Option<u32> = md
        .iter()
        .find(|(k, _)| k == "avi:width")
        .and_then(|(_, v)| v.parse::<u32>().ok());
    let h: Option<u32> = md
        .iter()
        .find(|(k, _)| k == "avi:height")
        .and_then(|(_, v)| v.parse::<u32>().ok());
    assert_eq!(
        from_accessor,
        w.zip(h),
        "typed accessor must agree with avi:width / avi:height metadata"
    );
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact avih dwWidth / dwHeight bytes.
// ---------------------------------------------------------------------------

/// Push a chunk (`id` + LE size + body, RIFF word-pad) onto `out`.
fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() & 1 == 1 {
        out.push(0);
    }
}

/// Wrap `body` in a `LIST <form> ...` (LE size = 4 + body, word-pad).
fn list(form: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"LIST");
    v.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
    v.extend_from_slice(form);
    v.extend_from_slice(body);
    if (4 + body.len()) & 1 == 1 {
        v.push(0);
    }
    v
}

/// Build a 56-byte AVISTREAMHEADER body for a video stream, stamping the
/// supplied `rcFrame` so the independence test can read it back.
fn strh_video(rc: (i16, i16, i16, i16)) -> Vec<u8> {
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids"); // fccType
    strh.extend_from_slice(b"MJPG"); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwLength
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwQuality
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
    strh.extend_from_slice(&rc.0.to_le_bytes()); // rcFrame.left
    strh.extend_from_slice(&rc.1.to_le_bytes()); // rcFrame.top
    strh.extend_from_slice(&rc.2.to_le_bytes()); // rcFrame.right
    strh.extend_from_slice(&rc.3.to_le_bytes()); // rcFrame.bottom
    assert_eq!(strh.len(), 56);
    strh
}

/// Build a minimal BITMAPINFOHEADER strf for an MJPG video stream.
fn strf_video_mjpg() -> Vec<u8> {
    let mut strf = Vec::with_capacity(40);
    strf.extend_from_slice(&40u32.to_le_bytes()); // biSize
    strf.extend_from_slice(&64u32.to_le_bytes()); // biWidth
    strf.extend_from_slice(&48u32.to_le_bytes()); // biHeight
    strf.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    strf.extend_from_slice(b"MJPG"); // biCompression
    strf.extend_from_slice(&(64u32 * 48 * 3).to_le_bytes()); // biSizeImage
    strf.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
    strf.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    strf
}

/// Build the 56-byte AVIMAINHEADER body with the requested `dwWidth` /
/// `dwHeight` LE-stamped at body offsets 32 / 36 (plus distinct values in
/// the round-268 `dwTotalFrames` and round-157 `dwInitialFrames` DWORDs
/// so the independence test can read each field back).
fn avih_body(width: u32, height: u32) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame (body offset 0)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec (body offset 4)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity (offset 8)
    avih.extend_from_slice(&0x0010u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX (offset 12)
    avih.extend_from_slice(&7u32.to_le_bytes()); // dwTotalFrames (body offset 16)
    avih.extend_from_slice(&2u32.to_le_bytes()); // dwInitialFrames (body offset 20)
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams (offset 24)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize (offset 28)
    avih.extend_from_slice(&width.to_le_bytes()); // dwWidth (body offset 32)
    avih.extend_from_slice(&height.to_le_bytes()); // dwHeight (body offset 36)
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    assert_eq!(avih.len(), 56);
    avih
}

/// Assemble an entire AVI 1.0 file in memory with one video stream, the
/// requested movie rectangle, and the requested per-stream `rcFrame`.
fn build_avi(width: u32, height: u32, rc: (i16, i16, i16, i16)) -> Vec<u8> {
    let avih = avih_body(width, height);

    let strh_body = strh_video(rc);
    let strf_body = strf_video_mjpg();
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_body);
    push_chunk(&mut strl_body, b"strf", &strf_body);
    let strl = list(b"strl", &strl_body);

    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih);
    hdrl_body.extend_from_slice(&strl);
    let hdrl = list(b"hdrl", &hdrl_body);

    let mut movi_body = Vec::new();
    push_chunk(&mut movi_body, b"00dc", &[0x55u8; 64]);
    let movi = list(b"movi", &movi_body);

    let mut riff_body = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(&hdrl);
    riff_body.extend_from_slice(&movi);
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);
    out
}

#[test]
fn handrolled_explicit_movie_rect_decodes() {
    let buf = build_avi(720, 576, (0, 0, 64, 48));
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.avih_movie_rect(), Some((720, 576)));
}

#[test]
fn handrolled_zero_width_parses_as_none() {
    let buf = build_avi(0, 480, (0, 0, 64, 48));
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.avih_movie_rect(),
        None,
        "a zero dwWidth must collapse the movie rectangle to None"
    );
}

#[test]
fn handrolled_zero_height_parses_as_none() {
    let buf = build_avi(640, 0, (0, 0, 64, 48));
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.avih_movie_rect(),
        None,
        "a zero dwHeight must collapse the movie rectangle to None"
    );
}

// ---------------------------------------------------------------------------
// Independence from the per-stream strh.rcFrame: the file-global movie
// rectangle and the per-stream destination rectangle are distinct fields
// and both surface verbatim through their separate accessors.
// ---------------------------------------------------------------------------

#[test]
fn movie_rect_independent_of_stream_frame_rect() {
    // Movie rectangle 720x576, a sub-stream rcFrame placed at (10, 20)
    // sized 64x48 inside it.
    let buf = build_avi(720, 576, (10, 20, 74, 68));
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_movie_rect(), Some((720, 576)));
    assert_eq!(dmx.stream_frame_rect(0), Some((10, 20, 74, 68)));
}

// ---------------------------------------------------------------------------
// Independence from neighbouring AVIMAINHEADER DWORDs: offsets 16 / 20 /
// 32 / 36 each read back their own bytes.
// ---------------------------------------------------------------------------

#[test]
fn movie_rect_independent_of_neighbouring_fields() {
    let buf = build_avi(352, 288, (0, 0, 64, 48));
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_total_frames(), Some(7)); // body offset 16 (round-268)
    assert_eq!(dmx.initial_frames(), Some(2)); // body offset 20 (round-157)
    assert_eq!(dmx.avih_movie_rect(), Some((352, 288))); // offsets 32 / 36 (round-275)
}
