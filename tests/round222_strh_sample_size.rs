//! Round-222 per-stream `strh.dwSampleSize` AVI tests.
//!
//! `dwSampleSize` is the 32-bit DWORD at byte offset 44 of the 56-byte
//! AVISTREAMHEADER per AVI 1.0 §"AVISTREAMHEADER"
//! (`docs/container/riff/avi-riff-file-reference.md`, `dwSampleSize`
//! row line 247): *"The size of a single sample of data. This is set
//! to zero if the samples can vary in size. If this number is nonzero,
//! then multiple samples of data can be grouped into a single chunk
//! within the file. If it is zero, each sample of data (such as a
//! video frame) must be in a separate chunk. For video streams, this
//! number is typically zero, although it can be nonzero if all video
//! frames are the same size. For audio streams, this number should be
//! the same as the nBlockAlign member of the WAVEFORMATEX structure
//! describing the audio."*
//!
//! The pre-round-222 muxer always stamped the packaging-derived default
//! (`nBlockAlign` for PCM / CBR audio, `0` for VBR audio, `0` for
//! video). Round-222 adds:
//!
//! - the typed `AviDemuxer::stream_sample_size(stream_index) ->
//!   Option<u32>` accessor mapping the spec-documented `0` "samples
//!   can vary in size" sentinel back to `None` so an unspecified hint
//!   reads the same as an absent one — mirroring the round-217
//!   `dwSuggestedBufferSize` / round-210 `fccHandler` / round-203
//!   `dwStart` / round-182 `wPriority` / round-176 `dwQuality` /
//!   round-153 `dwInitialFrames` / round-119 `wLanguage` / round-115
//!   `rcFrame` / round-80 `strn` / round-107 `IDIT` "default ==
//!   absent" convention,
//! - the `avi:strh.<n>.sample_size` metadata key (omitted for the
//!   `0` sentinel),
//! - the `AviMuxOptions::with_stream_sample_size(stream_index, n)`
//!   builder writing the supplied 32-bit value verbatim at byte
//!   offset 44 of the strh.
//!
//! Exercises:
//!
//! - **Audio-PCM baseline**: no override ⇒ the packaging-derived
//!   `nBlockAlign` value (`4` for 2-channel s16le) surfaces on the
//!   audio stream; the video stream stays `None`.
//! - **Video override round-trip**: a fixed-frame-size raw-yuv hint
//!   on the video stream round-trips via the typed accessor and the
//!   metadata key.
//! - **Builder idempotency**: the last
//!   `with_stream_sample_size(...)` for a given index wins.
//! - **Explicit `0` override on audio**: stamps the spec-documented
//!   "samples can vary in size" sentinel onto the PCM stream; the
//!   demuxer (opened lenient — the round-14 C2 audio sample-size
//!   invariant correctly rejects a PCM stream with `dwSampleSize == 0`
//!   under the strict `open_avi`) maps it to `None`, and the metadata
//!   key is omitted.
//! - **Boundary values**: `1`, `u32::MAX`, and a typical 1280×720 raw
//!   frame size (= 921600 / 1382400) round-trip exactly through the
//!   video stream.
//! - **Independence across streams**: an override on stream 0 doesn't
//!   perturb stream 1's accessor (the auto-derived `nBlockAlign` on
//!   the audio stream stays intact), and vice versa.
//! - **Independence from sibling DWORDs**: stamping `dwSampleSize`
//!   doesn't perturb `dwSuggestedBufferSize` / `fccHandler` /
//!   `dwStart` / `wPriority` / `dwQuality` / `dwInitialFrames` /
//!   `wLanguage` readbacks.
//! - **Hand-rolled fixture**: an explicit non-zero `dwSampleSize` in
//!   a 56-byte strh decodes to the expected raw u32; an all-zero
//!   `dwSampleSize` parses as `None`.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer, Packet, Rational,
    ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{open_avi as demuxer_open_avi, open_avi_lenient};
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

// One video packet of `video_payload_len` bytes and one 8-byte audio
// packet.
fn write_minimal(path: &std::path::Path, options: AviMuxOptions, video_payload_len: usize) {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();

    let mut v = Packet::new(0, streams[0].time_base, vec![0x55u8; video_payload_len]);
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
    std::env::temp_dir().join(format!("oxideav-avi-r222-{name}-{pid}-{nanos}.avi"))
}

// ---------------------------------------------------------------------------
// Audio-PCM baseline: no override ⇒ packaging-derived nBlockAlign on
// the audio stream; video stream stays None.
// ---------------------------------------------------------------------------

#[test]
fn strh_sample_size_no_override_audio_pcm_baseline() {
    let tmp = tmp_path("no-override");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Video stream: spec-documented `0` "samples can vary in size" ⇒ None.
    assert_eq!(dmx.stream_sample_size(0), None);
    // Audio stream: 2ch × s16le = 4-byte nBlockAlign per WAVEFORMATEX.
    assert_eq!(dmx.stream_sample_size(1), Some(4));

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.sample_size"),
        "video-stream default `0` must omit the metadata key (default == absent)"
    );
    let m1 = md
        .iter()
        .find(|(k, _)| k == "avi:strh.1.sample_size")
        .expect("audio-stream nBlockAlign must surface as metadata");
    assert_eq!(m1.1, "4");

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Round-trip: a non-default per-stream sample-size on the VIDEO stream
// survives mux → demux via the typed accessor and the metadata key.
// ---------------------------------------------------------------------------

#[test]
fn strh_sample_size_video_override_roundtrip_accessor_and_metadata() {
    let tmp = tmp_path("video-override");
    // Pretend this is a fixed-frame-size raw-yuv stream where every
    // chunk is exactly 6144 bytes (64×48 ×2 bytes/pixel, e.g.
    // YUYV422). A legacy raw recorder might stamp this in
    // `dwSampleSize` to enable multi-sample chunking.
    let opts = AviMuxOptions::default().with_stream_sample_size(0, 6_144);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_sample_size(0), Some(6_144));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.sample_size")
        .expect("expected `avi:strh.0.sample_size` metadata key for the override");
    assert_eq!(entry.1, "6144");

    // The audio stream is untouched: nBlockAlign still surfaces.
    assert_eq!(dmx.stream_sample_size(1), Some(4));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Builder idempotency: last `with_stream_sample_size` for a given index
// wins.
// ---------------------------------------------------------------------------

#[test]
fn strh_sample_size_builder_idempotency_last_call_wins() {
    let tmp = tmp_path("idempotency");
    let opts = AviMuxOptions::default()
        .with_stream_sample_size(0, 256)
        .with_stream_sample_size(0, 512)
        .with_stream_sample_size(0, 1_024);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_sample_size(0), Some(1_024));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Explicit `0` override on audio: stamps the spec-documented "samples
// can vary in size" sentinel onto the PCM stream; the strict open_avi
// rejects it via the round-14 C2 audio sample-size invariant; the
// lenient demuxer maps it to None and omits the metadata key.
// ---------------------------------------------------------------------------

#[test]
fn strh_sample_size_audio_explicit_zero_rejected_strict_lenient_none() {
    let tmp = tmp_path("audio-zero");
    let opts = AviMuxOptions::default().with_stream_sample_size(1, 0);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();

    // Strict open: round-14 C2 sample-size invariant fires. PCM
    // (wFormatTag=1) is CBR and requires sample_size > 0.
    let rs_strict: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let err = match demuxer_open_avi(rs_strict, &reg) {
        Ok(_) => panic!(
            "PCM stream with dwSampleSize=0 must fail the round-14 C2 invariant under strict open",
        ),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("audio stream 1"),
        "expected validator to call out audio stream 1: {msg}"
    );

    // Lenient open: validator is skipped; accessor reads back `None`
    // for the sentinel; metadata key is omitted.
    let rs_lenient: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = open_avi_lenient(rs_lenient, &reg).unwrap();
    assert_eq!(dmx.stream_sample_size(1), None);
    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.1.sample_size"),
        "explicit `0` override must omit the metadata key (default == absent)"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Boundary values on the video stream.
// ---------------------------------------------------------------------------

#[test]
fn strh_sample_size_boundary_one_roundtrips() {
    let tmp = tmp_path("boundary-one");
    let opts = AviMuxOptions::default().with_stream_sample_size(0, 1);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_sample_size(0), Some(1));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_sample_size_boundary_u32_max_roundtrips() {
    let tmp = tmp_path("boundary-u32-max");
    let opts = AviMuxOptions::default().with_stream_sample_size(0, u32::MAX);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_sample_size(0), Some(u32::MAX));

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:strh.0.sample_size")
        .unwrap();
    assert_eq!(entry.1, u32::MAX.to_string());

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strh_sample_size_typical_raw_frame_size_roundtrips() {
    // 1280×720 ×1 byte/pixel (8-bit luma) = 921600.
    let tmp = tmp_path("raw-y8-720p");
    let opts = AviMuxOptions::default().with_stream_sample_size(0, 921_600);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_sample_size(0), Some(921_600));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence across streams.
// ---------------------------------------------------------------------------

#[test]
fn strh_sample_size_independence_across_streams() {
    let tmp = tmp_path("independence");
    // Override stream 0 only.
    let opts = AviMuxOptions::default().with_stream_sample_size(0, 4_096);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_sample_size(0), Some(4_096));
    // Audio stream still carries the auto-derived nBlockAlign.
    assert_eq!(dmx.stream_sample_size(1), Some(4));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Independence from sibling DWORDs: stamping dwSampleSize does not
// perturb the other strh DWORDs the demuxer surfaces.
// ---------------------------------------------------------------------------

#[test]
fn strh_sample_size_does_not_perturb_sibling_dwords() {
    let tmp = tmp_path("siblings");
    let opts = AviMuxOptions::default()
        // Stamp every sibling so we can confirm round-trip after also
        // adding dwSampleSize.
        .with_stream_suggested_buffer_size(0, 65_536)
        .with_stream_handler(0, *b"YV12")
        .with_stream_start(0, 7)
        .with_stream_priority(0, 9)
        .with_stream_quality(0, 8_000)
        .with_stream_initial_frames(0, 11)
        .with_stream_language(0, 0x0409)
        // …and the new dwSampleSize override.
        .with_stream_sample_size(0, 2_048);
    write_minimal(&tmp, opts, 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_sample_size(0), Some(2_048));
    assert_eq!(dmx.stream_suggested_buffer_size(0), Some(65_536));
    assert_eq!(dmx.stream_handler(0), Some(*b"YV12"));
    assert_eq!(dmx.stream_start(0), Some(7));
    assert_eq!(dmx.stream_priority(0), Some(9));
    assert_eq!(dmx.stream_quality(0), Some(8_000));
    assert_eq!(dmx.stream_initial_frames(0), Some(11));
    assert_eq!(dmx.stream_language(0), Some(0x0409));

    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------------------
// Hand-rolled fixture: explicit non-zero / all-zero dwSampleSize in a
// 56-byte strh decodes correctly.
// ---------------------------------------------------------------------------

fn hand_rolled_avi_with_video_sample_size(sample_size: u32) -> Vec<u8> {
    // Minimal one-video-stream AVI with all-zero strh fields except
    // fccType=`vids`, dwScale/dwRate (25 fps), and dwSampleSize at offset
    // 44. A 1-byte BI_RGB strf and an empty movi follow.
    let mut buf: Vec<u8> = Vec::new();

    // strh body (56 B).
    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"vids");
    // dwScale at 20, dwRate at 24.
    strh[20..24].copy_from_slice(&1u32.to_le_bytes());
    strh[24..28].copy_from_slice(&25u32.to_le_bytes());
    // dwSampleSize at 44.
    strh[44..48].copy_from_slice(&sample_size.to_le_bytes());

    // strf body: minimal BITMAPINFOHEADER (40 B), BI_RGB (compression=0).
    let mut strf = vec![0u8; 40];
    strf[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
    strf[4..8].copy_from_slice(&64i32.to_le_bytes()); // biWidth
    strf[8..12].copy_from_slice(&48i32.to_le_bytes()); // biHeight
    strf[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf[14..16].copy_from_slice(&24u16.to_le_bytes()); // biBitCount
                                                        // biCompression at 16: BI_RGB = 0 (already zero).
                                                        // biSizeImage at 20: leave zero (legal for BI_RGB).

    // avih body (56 B).
    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame
    avih[16..20].copy_from_slice(&0u32.to_le_bytes()); // dwFlags
    avih[20..24].copy_from_slice(&0u32.to_le_bytes()); // dwTotalFrames
    avih[24..28].copy_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih[28..32].copy_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih[36..40].copy_from_slice(&64u32.to_le_bytes()); // dwWidth
    avih[40..44].copy_from_slice(&48u32.to_le_bytes()); // dwHeight

    // Helper: write a chunk with FourCC + size + body (pad if odd).
    fn write_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
    }

    // hdrl body: avih chunk + LIST strl(strh + strf).
    let mut hdrl_body: Vec<u8> = Vec::new();
    hdrl_body.extend_from_slice(b"hdrl");
    write_chunk(&mut hdrl_body, b"avih", &avih);

    // LIST strl body
    let mut strl_body: Vec<u8> = Vec::new();
    strl_body.extend_from_slice(b"strl");
    write_chunk(&mut strl_body, b"strh", &strh);
    write_chunk(&mut strl_body, b"strf", &strf);

    hdrl_body.extend_from_slice(b"LIST");
    hdrl_body.extend_from_slice(&(strl_body.len() as u32).to_le_bytes());
    hdrl_body.extend_from_slice(&strl_body);

    // movi body: empty.
    let mut movi_body: Vec<u8> = Vec::new();
    movi_body.extend_from_slice(b"movi");

    // RIFF body: hdrl LIST + movi LIST.
    let mut riff_body: Vec<u8> = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(hdrl_body.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&hdrl_body);
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(movi_body.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&movi_body);

    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&riff_body);

    buf
}

#[test]
fn strh_sample_size_hand_rolled_nonzero_decodes() {
    let bytes = hand_rolled_avi_with_video_sample_size(31337);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_sample_size(0), Some(31_337));
}

#[test]
fn strh_sample_size_hand_rolled_zero_decodes_as_none() {
    let bytes = hand_rolled_avi_with_video_sample_size(0);
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_sample_size(0), None);

    let md = dmx.metadata();
    assert!(
        md.iter().all(|(k, _)| k != "avi:strh.0.sample_size"),
        "zero dwSampleSize must not surface a metadata key"
    );
}

// ---------------------------------------------------------------------------
// Out-of-range stream index returns None on the typed accessor.
// ---------------------------------------------------------------------------

#[test]
fn strh_sample_size_out_of_range_index_returns_none() {
    let tmp = tmp_path("out-of-range");
    write_minimal(&tmp, AviMuxOptions::default(), 64);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_sample_size(99), None);

    let _ = std::fs::remove_file(&tmp);
}
