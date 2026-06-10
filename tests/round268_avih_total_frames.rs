//! Round-268 file-global `avih.dwTotalFrames` typed accessor +
//! `avi:total_frames` metadata key.
//!
//! `dwTotalFrames` is the 32-bit DWORD at byte offset 16 of the
//! 56-byte AVIMAINHEADER body (byte 24 of the `avih` chunk) per AVI 1.0
//! §"AVIMAINHEADER" (`docs/container/riff/avi-riff-file-reference.md`,
//! Appendix A `dwTotalFrames` row, line 199): *"Total number of frames
//! of data in the file."*
//!
//! `0` is the writer-skips-it / empty-file sentinel mapped here to
//! `None`, mirroring the round-260 `dwMaxBytesPerSec` / round-256
//! `dwMicroSecPerFrame` / round-249 `(dwScale, dwRate)` / round-247
//! `dwFlags` / round-229 `dwLength` "default == absent" idiom.
//!
//! Pre-round-268 the demuxer already parsed `dwTotalFrames` and
//! consumed it internally to derive `duration_micros = total_frames *
//! micro_sec_per_frame` (the source of `Demuxer::duration`), but the
//! raw value was never surfaced — neither a typed accessor nor a
//! metadata key existed. Round-268 closes both gaps:
//!
//! - `AviDemuxer::avih_total_frames() -> Option<u32>` raw accessor
//!   returning the verbatim 32-bit value at byte offset 16 of the
//!   56-byte AVIMAINHEADER body.
//! - The `avi:total_frames = "<N>"` decimal metadata key (omitted when
//!   the field carried the all-zero sentinel, so absence stays
//!   observable).
//!
//! For a multi-segment OpenDML file this field only carries the
//! primary `RIFF AVI ` segment's frame count (per OpenDML 2.0 §5.0);
//! the cross-segment truth lives in the separate `dmlh.dwTotalFrames`
//! surfaced via `dmlh_total_frames()` /
//! `avi:total_frames_all_segments`. The two surfaces are
//! spec-independent and both round-trip verbatim.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip**: the muxer's auto-derived stamp (first
//!   video stream's emitted packet count, patched at `write_trailer`)
//!   surfaces verbatim via the typed accessor + metadata key.
//! - **Accessor / metadata agreement** on the on-disk byte pattern.
//! - **Hand-rolled fixture**: an explicit non-zero `dwTotalFrames` in
//!   a 56-byte avih decodes to the expected raw u32.
//! - **Hand-rolled fixture**: an all-zero `dwTotalFrames` parses as
//!   `None` and the metadata key is absent.
//! - **Independence from `dmlh.dwTotalFrames`**: a fixture stamping
//!   different avih / dmlh frame counts surfaces both verbatim through
//!   their separate accessors + metadata keys.
//! - **Independence from neighbouring AVIMAINHEADER DWORDs**: the
//!   round-256 `dwMicroSecPerFrame` (offset 0), round-260
//!   `dwMaxBytesPerSec` (offset 4) and round-157 `dwInitialFrames`
//!   (offset 20) all read back their own bytes alongside a stamped
//!   `dwTotalFrames`.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer, Packet, Rational,
    ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn video_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(64);
    params.height = Some(48);
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

/// Mux a video+audio AVI 1.0 file with `n_video` video packets (and one
/// audio packet, so the per-stream interleave path is exercised).
fn write_n_video_frames(path: &std::path::Path, n_video: u32) {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::new()).unwrap();
    mux.write_header().unwrap();

    for i in 0..n_video {
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
// Mux → demux round-trip: the muxer's auto-derived dwTotalFrames stamp
// (first video stream's emitted packet count, patched at write_trailer)
// surfaces verbatim via the typed accessor + metadata key.
// ---------------------------------------------------------------------------

#[test]
fn muxer_auto_stamp_roundtrips_via_accessor_and_metadata() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r268-avih-total-frames.avi");
    write_n_video_frames(&tmp, 3);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_total_frames(), Some(3));
    assert!(
        dmx.metadata()
            .iter()
            .any(|(k, v)| k == "avi:total_frames" && v == "3"),
        "missing avi:total_frames=3 metadata key; got {:?}",
        dmx.metadata()
            .iter()
            .filter(|(k, _)| k.contains("total_frames"))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Accessor / metadata agreement on the on-disk byte pattern, whatever
// the muxer stamped.
// ---------------------------------------------------------------------------

#[test]
fn accessor_agrees_with_metadata_key() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r268-total-frames-agree.avi");
    write_n_video_frames(&tmp, 7);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let from_accessor = dmx.avih_total_frames();
    assert!(from_accessor.is_some(), "7-frame fixture stamps a count");
    let from_md: Option<u32> = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "avi:total_frames")
        .and_then(|(_, v)| v.parse::<u32>().ok());
    assert_eq!(
        from_accessor, from_md,
        "typed accessor must agree with avi:total_frames metadata"
    );
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact avih dwTotalFrames bytes.
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

/// Build a 56-byte AVISTREAMHEADER body for a video stream. Other fields
/// use parseable defaults; we don't exercise strh here.
fn strh_video() -> Vec<u8> {
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
    strh.extend_from_slice(&0i16.to_le_bytes()); // rcFrame.left
    strh.extend_from_slice(&0i16.to_le_bytes()); // rcFrame.top
    strh.extend_from_slice(&64i16.to_le_bytes()); // rcFrame.right
    strh.extend_from_slice(&48i16.to_le_bytes()); // rcFrame.bottom
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

/// Build the 56-byte AVIMAINHEADER body with the requested
/// `dwTotalFrames` LE-stamped at body offset 16 (plus distinct values
/// in the neighbouring DWORDs so the independence test can read each
/// field back).
fn avih_body(total_frames: u32) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame (body offset 0)
    avih.extend_from_slice(&1_500_000u32.to_le_bytes()); // dwMaxBytesPerSec (body offset 4)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&0x0010u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX
    avih.extend_from_slice(&total_frames.to_le_bytes()); // dwTotalFrames (body offset 16)
    avih.extend_from_slice(&2u32.to_le_bytes()); // dwInitialFrames (body offset 20)
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    avih.extend_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih.extend_from_slice(&48u32.to_le_bytes()); // dwHeight
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]
    assert_eq!(avih.len(), 56);
    avih
}

/// Assemble an entire AVI 1.0 file in memory with one video stream, the
/// requested `avih.dwTotalFrames`, and (optionally) a `LIST odml dmlh`
/// extended header carrying a separate cross-segment frame count.
fn build_avi_with_total_frames(total_frames: u32, dmlh_total: Option<u32>) -> Vec<u8> {
    let avih = avih_body(total_frames);

    let strh_body = strh_video();
    let strf_body = strf_video_mjpg();
    let mut strl_body = Vec::new();
    push_chunk(&mut strl_body, b"strh", &strh_body);
    push_chunk(&mut strl_body, b"strf", &strf_body);
    let strl = list(b"strl", &strl_body);

    let mut hdrl_body = Vec::new();
    push_chunk(&mut hdrl_body, b"avih", &avih);
    hdrl_body.extend_from_slice(&strl);
    if let Some(n) = dmlh_total {
        // OpenDML 2.0 §5.0: `LIST odml` with a `dmlh` chunk whose body
        // is a single DWORD `dwTotalFrames` — the real frame count
        // across every RIFF segment.
        let mut odml_body = Vec::new();
        push_chunk(&mut odml_body, b"dmlh", &n.to_le_bytes());
        hdrl_body.extend_from_slice(&list(b"odml", &odml_body));
    }
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
fn handrolled_explicit_nonzero_avih_total_frames_decodes() {
    let buf = build_avi_with_total_frames(0xDEAD_BEEF, None);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.avih_total_frames(), Some(0xDEAD_BEEF));
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:total_frames" && v == "3735928559"));
}

#[test]
fn handrolled_zero_avih_total_frames_parses_as_none() {
    let buf = build_avi_with_total_frames(0, None);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.avih_total_frames(),
        None,
        "an all-zero avih.dwTotalFrames must read back as None"
    );
    assert!(!dmx.metadata().iter().any(|(k, _)| k == "avi:total_frames"));
}

// ---------------------------------------------------------------------------
// Independence from dmlh.dwTotalFrames: the avih (primary-segment) and
// dmlh (cross-segment) frame counts are spec-independent fields and
// both surface verbatim through their separate accessors + keys.
// ---------------------------------------------------------------------------

#[test]
fn avih_total_frames_independent_of_dmlh_total_frames() {
    let buf = build_avi_with_total_frames(5, Some(99));
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.avih_total_frames(), Some(5));
    assert_eq!(dmx.dmlh_total_frames(), Some(99));
    let md = dmx.metadata();
    assert!(md.iter().any(|(k, v)| k == "avi:total_frames" && v == "5"));
    assert!(md
        .iter()
        .any(|(k, v)| k == "avi:total_frames_all_segments" && v == "99"));
}

// ---------------------------------------------------------------------------
// Independence from neighbouring AVIMAINHEADER DWORDs: offsets 0 / 4 /
// 16 / 20 each read back their own bytes.
// ---------------------------------------------------------------------------

#[test]
fn avih_total_frames_independent_of_neighbouring_fields() {
    let buf = build_avi_with_total_frames(12, None);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.micro_sec_per_frame(), Some(40_000)); // body offset 0 (round-256)
    assert_eq!(dmx.max_bytes_per_sec(), Some(1_500_000)); // body offset 4 (round-260)
    assert_eq!(dmx.avih_total_frames(), Some(12)); // body offset 16 (round-268)
    assert_eq!(dmx.initial_frames(), Some(2)); // body offset 20 (round-157)
}
