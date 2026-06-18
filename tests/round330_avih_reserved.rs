//! Round-330 file-global `avih.dwReserved[4]` trailing-array typed
//! accessor.
//!
//! `dwReserved` is the four trailing DWORDs at byte offsets 40..56 of
//! the 56-byte AVIMAINHEADER body per AVI 1.0 §"AVIMAINHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, Appendix A
//! `dwReserved` row line 205): *"Reserved. Set this array to zero."*
//!
//! A spec-conformant writer leaves all four DWORDs `0`. Round-330 adds:
//!
//! - `AviDemuxer::avih_reserved() -> Option<[u32; 4]>` returning the
//!   verbatim four-DWORD array, with the "all-zero ⇒ None"
//!   "default == absent" mapping that lets a forensic / repair caller
//!   detect a non-conformant header that smuggled data into the
//!   reserved slot while a conformant file reads back `None`.
//! - the `avi:reserved` metadata key (comma-joined `0x`-hex), emitted
//!   only when any DWORD is non-zero.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip**: the muxer always zeroes the reserved
//!   array, so a normally-written file reads back `None` and emits no
//!   `avi:reserved` key.
//! - **Hand-rolled fixture**: an all-zero reserved array decodes to
//!   `None` and emits no metadata key (conformant default).
//! - **Hand-rolled fixture**: a non-conformant array (one or more
//!   non-zero DWORDs) decodes verbatim and emits the matching
//!   `avi:reserved` hex string.
//! - **Short (40-byte) avih body**: the reserved array is absent on
//!   disk and reads back `None`, indistinguishable from a zeroed one.
//! - **Independence from neighbouring AVIMAINHEADER DWORDs**: the
//!   round-275 `dwWidth`/`dwHeight` (offsets 32/36) and round-268
//!   `dwTotalFrames` (offset 16) read back their own bytes alongside a
//!   stamped reserved array.

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

/// Mux a video+audio AVI 1.0 file.
fn write_video(path: &std::path::Path) {
    let streams = [video_stream(0, 320, 240), audio_stream(1)];
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
// Mux → demux round-trip: the muxer always zeroes the reserved array, so a
// normally-written file reads back None and emits no avi:reserved key.
// ---------------------------------------------------------------------------

#[test]
fn muxer_zeroes_reserved_so_accessor_is_none() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r330-reserved.avi");
    write_video(&tmp);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.avih_reserved(),
        None,
        "a normally-muxed file leaves dwReserved all-zero ⇒ None"
    );
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k == "avi:reserved"),
        "no avi:reserved key for a conformant zeroed array"
    );
}

// ---------------------------------------------------------------------------
// Hand-rolled fixtures: control the exact avih dwReserved bytes.
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

/// Build a 56-byte AVISTREAMHEADER body for a video stream.
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

/// Build the AVIMAINHEADER body with the requested `dwReserved[4]`
/// array LE-stamped at body offsets 40..56. When `full` is `false`,
/// truncate the body to 40 bytes (no reserved array on disk) so the
/// short-body case can be exercised. Distinct non-zero values in
/// `dwTotalFrames` (offset 16) and `dwWidth`/`dwHeight` (offsets 32/36)
/// let the independence test read each neighbouring field back.
fn avih_body(reserved: [u32; 4], full: bool) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame (offset 0)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec (offset 4)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity (offset 8)
    avih.extend_from_slice(&0x0010u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX (offset 12)
    avih.extend_from_slice(&7u32.to_le_bytes()); // dwTotalFrames (offset 16)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames (offset 20)
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams (offset 24)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize (offset 28)
    avih.extend_from_slice(&720u32.to_le_bytes()); // dwWidth (offset 32)
    avih.extend_from_slice(&576u32.to_le_bytes()); // dwHeight (offset 36)
    assert_eq!(avih.len(), 40);
    if full {
        for w in reserved {
            avih.extend_from_slice(&w.to_le_bytes()); // dwReserved[4] (offsets 40..56)
        }
        assert_eq!(avih.len(), 56);
    }
    avih
}

/// Assemble an entire AVI 1.0 file in memory with one video stream and
/// the requested reserved array (or a short 40-byte avih).
fn build_avi(reserved: [u32; 4], full: bool) -> Vec<u8> {
    let avih = avih_body(reserved, full);

    let strh_body = strh_video();
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

fn open(buf: Vec<u8>) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(buf));
    demuxer_open_avi(rs, &reg).unwrap()
}

#[test]
fn handrolled_all_zero_reserved_is_none() {
    let dmx = open(build_avi([0, 0, 0, 0], true));
    assert_eq!(
        dmx.avih_reserved(),
        None,
        "the spec-conformant all-zero reserved array must read back None"
    );
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k == "avi:reserved"),
        "no avi:reserved key for a conformant zeroed array"
    );
}

#[test]
fn handrolled_nonconformant_reserved_decodes_verbatim() {
    let reserved = [0x0000_0000u32, 0xDEAD_BEEF, 0x0000_0000, 0x0000_0001];
    let dmx = open(build_avi(reserved, true));
    assert_eq!(
        dmx.avih_reserved(),
        Some(reserved),
        "a non-conformant reserved array must surface verbatim, whole array"
    );

    let md = dmx.metadata();
    let v = md
        .iter()
        .find(|(k, _)| k == "avi:reserved")
        .map(|(_, v)| v.clone());
    assert_eq!(
        v.as_deref(),
        Some("0x00000000,0xDEADBEEF,0x00000000,0x00000001"),
        "avi:reserved must be the comma-joined 0x-hex array in order"
    );
}

#[test]
fn handrolled_single_nonzero_dword_surfaces_whole_array() {
    // Only the first DWORD is non-zero; the accessor still returns the
    // whole array so the exact on-disk pattern is observable.
    let reserved = [0xCAFE_BABEu32, 0, 0, 0];
    let dmx = open(build_avi(reserved, true));
    assert_eq!(dmx.avih_reserved(), Some(reserved));
}

#[test]
fn short_avih_body_reserved_is_none() {
    // 40-byte avih: the reserved array is absent on disk and reads back
    // None, indistinguishable from a zeroed full-length one.
    let dmx = open(build_avi([0, 0, 0, 0], false));
    assert_eq!(
        dmx.avih_reserved(),
        None,
        "a short (40-byte) avih has no reserved array ⇒ None"
    );
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k == "avi:reserved"),
        "no avi:reserved key for a short avih body"
    );
}

// ---------------------------------------------------------------------------
// Independence from neighbouring AVIMAINHEADER DWORDs: a stamped reserved
// array does not disturb dwTotalFrames (offset 16) or the dwWidth/dwHeight
// movie rectangle (offsets 32/36), and vice versa.
// ---------------------------------------------------------------------------

#[test]
fn reserved_independent_of_neighbouring_fields() {
    let reserved = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
    let dmx = open(build_avi(reserved, true));

    assert_eq!(dmx.avih_total_frames(), Some(7)); // offset 16 (round-268)
    assert_eq!(dmx.avih_movie_rect(), Some((720, 576))); // offsets 32/36 (round-275)
    assert_eq!(dmx.avih_reserved(), Some(reserved)); // offsets 40..56 (round-330)
}
