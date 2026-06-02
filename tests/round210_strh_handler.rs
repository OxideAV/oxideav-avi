//! Round-210 per-stream `strh.fccHandler` AVI tests.
//!
//! `fccHandler` is the 4-byte FOURCC at byte offset 4 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, Appendix B
//! `fccHandler` row, line 236): *"An optional FOURCC that identifies
//! a specific data handler. The data handler is the preferred handler
//! for the stream. For audio and video streams, this specifies the
//! codec for decoding the stream."*
//!
//! The spec phrases the field as the VfW preferred-driver hint — it
//! sits beside (and is logically distinct from) the video stream's
//! `BITMAPINFOHEADER.biCompression` FourCC and the audio stream's
//! `WAVEFORMATEX.wFormatTag`. Writers in the wild typically mirror
//! `biCompression` into `fccHandler` on video streams (so an `MJPG`
//! video gets `fccHandler = b"MJPG"`) but the spec does not require
//! the two to match, and audio writers almost always leave the
//! field zero (the spec's *optional* qualifier).
//!
//! The demuxer surfaces the raw 4 bytes verbatim and the muxer
//! writes whatever 4 bytes the caller supplies — applications that
//! preserve a driver-suite identifier in fccHandler distinct from
//! biCompression round-trip exactly.
//!
//! `[0, 0, 0, 0]` is the spec-aligned "no preferred handler" default
//! and maps to `None` so an unspecified hint reads the same as an
//! absent one, mirroring the round-203 `dwStart` / round-182
//! `wPriority` / round-176 `dwQuality` / round-153 `dwInitialFrames`
//! / round-119 `wLanguage` / round-115 `rcFrame` / round-80 `strn` /
//! round-107 `IDIT` "default == absent" convention.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip** of a non-default per-stream
//!   `fccHandler` via the typed accessor and the metadata key.
//! - **No-override baseline**: with no override, the muxer keeps the
//!   packaging-derived default — video streams mirror
//!   `BITMAPINFOHEADER.biCompression` (so an `MJPG` video stream
//!   reads back as `Some(b"MJPG")`), audio streams stay all-zero
//!   (which the demuxer maps to `None`).
//! - **Builder idempotency**: the last `with_stream_handler(...)`
//!   wins per stream index.
//! - **Explicit `[0, 0, 0, 0]` override** zeroes the field on a
//!   video stream (overriding the `biCompression`-mirror default)
//!   and reads back as `None`.
//! - **Audio-stream stamp**: a caller can put a driver hint on an
//!   audio stream whose default is all-zero, and the value
//!   round-trips.
//! - **Boundary values**: a `[0xFF, 0xFF, 0xFF, 0xFF]` non-printable
//!   FourCC round-trips verbatim (the spec's *optional FOURCC* row
//!   does not pin printability); a printable-ASCII `b"AVRn"`
//!   surfaces in the metadata-string form as `"AVRn"`; a binary
//!   FourCC surfaces as `"0xHHHHHHHH"`.
//! - **Independence across streams**: a handler on stream 1 doesn't
//!   perturb stream 0's accessor, and vice versa.
//! - **Independence from sibling DWORDs**: stamping `fccHandler`
//!   doesn't perturb `dwStart` / `wPriority` / `dwQuality` /
//!   `dwInitialFrames` / `wLanguage` readbacks.
//! - **Hand-rolled fixtures**: an explicit non-zero `fccHandler` in
//!   a 56-byte strh decodes to the expected raw 4 bytes; an
//!   all-zero `fccHandler` parses as `None`.

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

fn write_minimal(path: &std::path::Path, options: AviMuxOptions) {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();

    let mut v = Packet::new(0, streams[0].time_base, vec![0x55u8; 64]);
    v.pts = Some(0);
    v.flags.keyframe = true;
    mux.write_packet(&v).unwrap();

    let mut a = Packet::new(1, streams[1].time_base, vec![0u8; 8]);
    a.pts = Some(0);
    a.flags.keyframe = true;
    mux.write_packet(&a).unwrap();

    mux.write_trailer().unwrap();
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("oxideav-avi-r210-{name}-{pid}-{nanos}.avi"))
}

// ---------------------------------------------------------------------------
// Round-trip: a non-default per-stream handler survives mux → demux.
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_override_roundtrip_accessor_and_metadata() {
    // Stamp a driver-suite FourCC (`AVRn`, a documented MJPEG
    // capture-card variant in the AVI 1.0 video-FourCC table) onto
    // the audio stream — a configuration that doesn't arise from the
    // packaging defaults (audio defaults to all-zero) so the override
    // path is exercised cleanly.
    let tmp = tmp_path("override-roundtrip");
    let opts = AviMuxOptions::default().with_stream_handler(1, *b"AVRn");
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_handler(1), Some(*b"AVRn"));
    // Stream 0 keeps the packaging default = b"MJPG" (mirror of
    // biCompression). The spec says fccHandler typically equals
    // biCompression for video; the muxer follows that convention.
    assert_eq!(dmx.stream_handler(0), Some(*b"MJPG"));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.handler")
        .expect("expected `avi:strh.1.handler` metadata key for the override");
    assert_eq!(entry.1, "AVRn");
    // Stream 0's MJPG default also surfaces (non-zero ⇒ Some ⇒ key emitted).
    let m0 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.handler")
        .expect("expected `avi:strh.0.handler` for video MJPG default");
    assert_eq!(m0.1, "MJPG");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Baseline: no override ⇒ packaging defaults (video MJPG / audio zero).
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_no_override_video_mirrors_bicompression_audio_omits() {
    let tmp = tmp_path("no-override");
    write_minimal(&tmp, AviMuxOptions::default());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Video stream: packaging mirrors biCompression so fccHandler == "MJPG".
    assert_eq!(dmx.stream_handler(0), Some(*b"MJPG"));
    // Audio stream: packaging default is all-zero ⇒ demuxer maps to None.
    assert_eq!(dmx.stream_handler(1), None);

    let md = dmx.metadata();
    let m0 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.handler")
        .expect("video stream's MJPG fccHandler must surface as metadata");
    assert_eq!(m0.1, "MJPG");
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.1.handler"),
        "audio stream's all-zero default must omit the metadata key (default == absent)"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: last `with_stream_handler` for a given index wins.
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_builder_idempotency_last_call_wins() {
    let tmp = tmp_path("builder-idempotency");
    let opts = AviMuxOptions::default()
        .with_stream_handler(1, *b"AAAA")
        .with_stream_handler(1, *b"BBBB")
        .with_stream_handler(1, *b"FINL");
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_handler(1), Some(*b"FINL"));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Explicit `[0, 0, 0, 0]` on a video stream overrides the biCompression
// mirror — the demuxer maps the resulting all-zero field to None.
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_explicit_zero_on_video_clears_bicompression_mirror() {
    let tmp = tmp_path("explicit-zero-video");
    // Without the override, stream 0 would carry `b"MJPG"` (mirror of
    // BITMAPINFOHEADER.biCompression); the explicit zero replaces it.
    let opts = AviMuxOptions::default().with_stream_handler(0, [0, 0, 0, 0]);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Override won: video stream's fccHandler now all-zero ⇒ None.
    assert_eq!(dmx.stream_handler(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.handler"),
        "explicit `[0, 0, 0, 0]` override must omit the metadata key just like the audio default"
    );

    // biCompression itself is unaffected — it lives in the strf
    // (BITMAPINFOHEADER), not the strh, so the stream's wire FourCC
    // tag in `params.tag` still says MJPG (set by the demuxer from
    // BITMAPINFOHEADER.biCompression, independent of fccHandler).
    let streams = dmx.streams();
    assert_eq!(
        streams[0].params.tag,
        Some(CodecTag::fourcc(b"MJPG")),
        "fccHandler-clearing must not affect BITMAPINFOHEADER.biCompression \
         (different field in different chunk)"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Audio-stream stamp: an explicit handler on a stream whose default is
// all-zero round-trips verbatim.
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_audio_stream_explicit_stamp_roundtrips() {
    let tmp = tmp_path("audio-stamp");
    // A made-up four-letter handler suite tag.
    let opts = AviMuxOptions::default().with_stream_handler(1, *b"snds");
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_handler(1), Some(*b"snds"));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.handler")
        .expect("explicit audio-stream handler must surface as metadata");
    assert_eq!(entry.1, "snds");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Boundary values: non-printable FourCCs round-trip verbatim; the
// metadata-string form switches to the `0xHHHHHHHH` hex fallback.
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_non_printable_fourcc_surfaces_as_hex() {
    let tmp = tmp_path("non-printable");
    // 0xFF bytes are not printable ASCII so the metadata-string form
    // falls back to lower-case `0xHHHHHHHH`. The accessor still
    // returns the raw 4 bytes — applications that round-trip a
    // capture whose vendor stamped a binary driver token preserve it.
    let opts = AviMuxOptions::default().with_stream_handler(0, [0xFF, 0xFF, 0xFF, 0xFF]);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_handler(0), Some([0xFF, 0xFF, 0xFF, 0xFF]));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.handler")
        .expect("non-printable handler must surface as a metadata key");
    assert_eq!(entry.1, "0xffffffff");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_handler_printable_ascii_surfaces_as_string() {
    let tmp = tmp_path("printable-ascii");
    // Includes a space (0x20, the printable-range lower bound) to
    // confirm the format helper's range check: `DIB ` is a documented
    // VfW driver name for uncompressed RGB.
    let opts = AviMuxOptions::default().with_stream_handler(0, *b"DIB ");
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_handler(0), Some(*b"DIB "));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.handler")
        .expect("printable-ASCII handler must surface as a metadata key");
    assert_eq!(entry.1, "DIB ");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_handler_mixed_bytes_falls_back_to_hex() {
    let tmp = tmp_path("mixed-bytes");
    // One non-printable byte (0x1f, below 0x20) ⇒ the whole 4 bytes
    // serialise as hex, not the partial-printable form. Matches the
    // helper's all-or-nothing range check.
    let opts = AviMuxOptions::default().with_stream_handler(0, [b'A', 0x1f, b'C', b'D']);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_handler(0), Some([b'A', 0x1f, b'C', b'D']));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.handler")
        .expect("mixed-bytes handler must surface as a metadata key");
    assert_eq!(entry.1, "0x411f4344");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence across streams: handler on stream 1 doesn't perturb
// stream 0's packaging default, and vice versa.
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_per_stream_independence() {
    let tmp = tmp_path("per-stream-independence");
    let opts = AviMuxOptions::default()
        .with_stream_handler(0, *b"AVRn")
        .with_stream_handler(1, *b"snds");
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_handler(0), Some(*b"AVRn"));
    assert_eq!(dmx.stream_handler(1), Some(*b"snds"));

    let md = dmx.metadata();
    let m0 = md.iter().find(|(k, _)| k == "avi:strh.0.handler").unwrap();
    let m1 = md.iter().find(|(k, _)| k == "avi:strh.1.handler").unwrap();
    assert_eq!(m0.1, "AVRn");
    assert_eq!(m1.1, "snds");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Out-of-range stream-index accessor returns `None`.
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_out_of_range_stream_index_is_none() {
    let tmp = tmp_path("out-of-range");
    write_minimal(&tmp, AviMuxOptions::default());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Only two streams exist (indices 0, 1).
    assert_eq!(dmx.stream_handler(2), None);
    assert_eq!(dmx.stream_handler(99), None);
    assert_eq!(dmx.stream_handler(u32::MAX), None);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from sibling DWORDs: stamping fccHandler leaves dwStart,
// wPriority, dwQuality, dwInitialFrames, wLanguage at their own defaults.
// ---------------------------------------------------------------------------

#[test]
fn strh_handler_independent_of_sibling_dwords() {
    let tmp = tmp_path("sibling-independence");
    let opts = AviMuxOptions::default().with_stream_handler(1, *b"snds");
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // fccHandler got stamped.
    assert_eq!(dmx.stream_handler(1), Some(*b"snds"));
    // Every sibling per-stream DWORD must still read as its own default.
    assert_eq!(dmx.stream_start(1), None);
    assert_eq!(dmx.stream_priority(1), None);
    assert_eq!(dmx.stream_quality(1), None);
    assert_eq!(dmx.stream_initial_frames(1), None);
    assert_eq!(dmx.stream_language(1), None);

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: an AVI with a chosen `fccHandler` at byte offset 4
// of the strh decodes to the expected raw 4 bytes.
// ---------------------------------------------------------------------------

fn build_hand_rolled_avi(handler_raw: [u8; 4]) -> Vec<u8> {
    // Builds the minimal AVI envelope:
    //   RIFF 'AVI '
    //     LIST 'hdrl'
    //       'avih' (56 B)
    //       LIST 'strl'
    //         'strh' (56 B) — vids
    //         'strf' (40 B) — BITMAPINFOHEADER, MJPG
    //     LIST 'movi' (1 frame)
    //     'idx1'
    let mut out: Vec<u8> = Vec::new();

    // strh body (56 B):
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids");
    strh.extend_from_slice(&handler_raw); // fccHandler (offset 4)
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&25u32.to_le_bytes()); // dwRate
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwLength = 1 frame
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwQuality (-1)
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
                                                 // rcFrame (left, top, right, bottom) = (0,0,64,48)
    strh.extend_from_slice(&0i16.to_le_bytes());
    strh.extend_from_slice(&0i16.to_le_bytes());
    strh.extend_from_slice(&64i16.to_le_bytes());
    strh.extend_from_slice(&48i16.to_le_bytes());
    assert_eq!(strh.len(), 56);

    // strf body (40 B BITMAPINFOHEADER) for MJPG:
    let mut strf = Vec::with_capacity(40);
    strf.extend_from_slice(&40u32.to_le_bytes()); // biSize
    strf.extend_from_slice(&64i32.to_le_bytes()); // biWidth
    strf.extend_from_slice(&48i32.to_le_bytes()); // biHeight
    strf.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    strf.extend_from_slice(b"MJPG"); // biCompression
    strf.extend_from_slice(&(64u32 * 48 * 3).to_le_bytes()); // biSizeImage
    strf.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    strf.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    assert_eq!(strf.len(), 40);

    // strl LIST = "strl" + strh chunk + strf chunk.
    let mut strl: Vec<u8> = Vec::new();
    strl.extend_from_slice(b"strl");
    // strh chunk
    strl.extend_from_slice(b"strh");
    strl.extend_from_slice(&(strh.len() as u32).to_le_bytes());
    strl.extend_from_slice(&strh);
    // strf chunk
    strl.extend_from_slice(b"strf");
    strl.extend_from_slice(&(strf.len() as u32).to_le_bytes());
    strl.extend_from_slice(&strf);

    // avih body (56 B):
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame (25fps)
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&0x10u32.to_le_bytes()); // dwFlags (HASINDEX)
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwTotalFrames
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    avih.extend_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih.extend_from_slice(&48u32.to_le_bytes()); // dwHeight
    for _ in 0..4 {
        avih.extend_from_slice(&0u32.to_le_bytes()); // reserved
    }
    assert_eq!(avih.len(), 56);

    // hdrl LIST body = "hdrl" + avih chunk + strl LIST.
    let mut hdrl: Vec<u8> = Vec::new();
    hdrl.extend_from_slice(b"hdrl");
    hdrl.extend_from_slice(b"avih");
    hdrl.extend_from_slice(&(avih.len() as u32).to_le_bytes());
    hdrl.extend_from_slice(&avih);
    hdrl.extend_from_slice(b"LIST");
    hdrl.extend_from_slice(&(strl.len() as u32).to_le_bytes());
    hdrl.extend_from_slice(&strl);

    // movi LIST body = "movi" + one tiny '00dc' frame.
    let frame = vec![0xFFu8; 4];
    let mut movi: Vec<u8> = Vec::new();
    movi.extend_from_slice(b"movi");
    movi.extend_from_slice(b"00dc");
    movi.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    movi.extend_from_slice(&frame);

    // idx1 chunk: one entry pointing at the frame.
    let mut idx1_body: Vec<u8> = Vec::new();
    idx1_body.extend_from_slice(b"00dc");
    idx1_body.extend_from_slice(&0x10u32.to_le_bytes()); // AVIIF_KEYFRAME
    idx1_body.extend_from_slice(&4u32.to_le_bytes()); // movi-relative offset to '00dc'
    idx1_body.extend_from_slice(&(frame.len() as u32).to_le_bytes());

    // riff body = "AVI " + LIST hdrl + LIST movi + idx1 chunk
    let mut riff_body: Vec<u8> = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(hdrl.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&hdrl);
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(movi.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&movi);
    riff_body.extend_from_slice(b"idx1");
    riff_body.extend_from_slice(&(idx1_body.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&idx1_body);

    // RIFF envelope.
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);

    out
}

#[test]
fn hand_rolled_fixture_non_zero_handler_decodes_verbatim() {
    let bytes = build_hand_rolled_avi(*b"iv32");
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_handler(0), Some(*b"iv32"));

    let md = dmx.metadata();
    let entry = md.iter().find(|(k, _)| k == "avi:strh.0.handler").unwrap();
    assert_eq!(entry.1, "iv32");
}

#[test]
fn hand_rolled_fixture_zero_handler_parses_as_none() {
    let bytes = build_hand_rolled_avi([0, 0, 0, 0]);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_handler(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.handler"),
        "fixture's `[0, 0, 0, 0]` fccHandler must omit the metadata key (default == absent)"
    );
}
