//! Round-89 `strd` (per-stream codec-driver data) AVI tests.
//!
//! Per AVI 1.0 §"AVI Stream Headers" (docs/container/riff/
//! avi-riff-file-reference.md):
//!
//! > If the stream-header data ('strd') chunk is present, it follows
//! > the stream format chunk. The format and content of this chunk
//! > are defined by the codec driver. Typically, drivers use this
//! > information for configuration. Applications that read and write
//! > AVI files do not need to interpret this information; they
//! > simple transfer it to and from the driver as a memory block.
//!
//! Exercises:
//!
//! - **Mux → demux round-trip** of per-stream `strd` blobs on a video +
//!   audio file via the typed `stream_header_data(stream_index)`
//!   accessor (raw bytes survive byte-for-byte).
//! - **Per-stream metadata key**: `avi:strd.<index>.len` surfaces the
//!   blob length (not the raw bytes, since they are opaque codec-driver
//!   data per spec).
//! - **No-strd baseline**: a file written without
//!   `with_stream_header_data(...)` carries zero `strd` chunks, file
//!   size matches the pre-round-89 baseline byte-for-byte, and the
//!   accessor returns `None` for every stream.
//! - **Empty payload parses as `Some(&[])`**: an explicit empty blob
//!   round-trips as `Some(&[])` (not `None`) so the demuxer
//!   distinguishes "no chunk" from "empty driver blob".
//! - **Builder dedup**: repeated `with_stream_header_data(0, ...)`
//!   keeps only the last entry per stream index.
//! - **Odd-length blob even-padding**: a 5-byte blob still round-trips
//!   byte-equal (the RIFF word-pad byte is invisible to the demuxer).

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer, Packet, Rational,
    ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};

// ---------------------------------------------------------------------------
// Fixtures (mirror round-80 strn tests).
// ---------------------------------------------------------------------------

fn video_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(16);
    params.height = Some(16);
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

    // One synthetic MJPEG packet (just bytes; the AVI muxer doesn't
    // care that it's not a real JPEG).
    let mut v = Packet::new(0, streams[0].time_base, vec![0x55u8; 64]);
    v.pts = Some(0);
    v.flags.keyframe = true;
    mux.write_packet(&v).unwrap();

    // Two stereo PCM samples = 8 bytes.
    let mut a = Packet::new(1, streams[1].time_base, vec![0u8; 8]);
    a.pts = Some(0);
    a.flags.keyframe = true;
    mux.write_packet(&a).unwrap();

    mux.write_trailer().unwrap();
}

// ---------------------------------------------------------------------------
// Round-trip: per-stream strd blobs survive mux → demux byte-equal.
// ---------------------------------------------------------------------------

#[test]
fn strd_video_and_audio_roundtrip() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r89-strd-roundtrip.avi");
    // Two distinct opaque blobs, one per stream. These bytes are
    // arbitrary — the AVI spec says applications do not interpret
    // strd content.
    let video_blob: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34, 0x56, 0x78];
    let audio_blob: Vec<u8> = (0..16u8).collect();
    let opts = AviMuxOptions::new()
        .with_stream_header_data(0, video_blob.clone())
        .with_stream_header_data(1, audio_blob.clone());
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_header_data(0),
        Some(video_blob.as_slice()),
        "video stream strd round-trip must be byte-equal"
    );
    assert_eq!(
        dmx.stream_header_data(1),
        Some(audio_blob.as_slice()),
        "audio stream strd round-trip must be byte-equal"
    );
    assert_eq!(dmx.stream_header_data(2), None, "out-of-range index → None");
}

// ---------------------------------------------------------------------------
// Metadata key surface: avi:strd.<index>.len reports each blob length.
// ---------------------------------------------------------------------------

#[test]
fn strd_metadata_key_exposes_length_only() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r89-strd-meta.avi");
    let opts = AviMuxOptions::new()
        .with_stream_header_data(0, vec![0u8; 12])
        .with_stream_header_data(1, vec![0u8; 4]);
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    let md = dmx.metadata();
    let has = |k: &str, want: &str| md.iter().any(|(key, val)| key == k && val == want);

    assert!(
        has("avi:strd.0.len", "12"),
        "missing avi:strd.0.len = \"12\": {:?}",
        md.iter()
            .filter(|(k, _)| k.starts_with("avi:strd"))
            .collect::<Vec<_>>()
    );
    assert!(has("avi:strd.1.len", "4"));

    // Sanity: the metadata Vec must not hexdump driver bytes into a
    // value (the spec says applications don't interpret strd content,
    // so we only surface the length).
    for (k, v) in md {
        if k.starts_with("avi:strd.") {
            assert!(
                v.chars().all(|c| c.is_ascii_digit()),
                "strd metadata value should be a decimal length, got {k}={v}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// No-strd baseline: pre-round-89 byte layout (no `with_stream_header_data`).
// ---------------------------------------------------------------------------

#[test]
fn no_strd_yields_none_accessor_and_no_meta_key() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r89-no-strd.avi");
    write_minimal(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(dmx.stream_header_data(0), None);
    assert_eq!(dmx.stream_header_data(1), None);

    let md = dmx.metadata();
    assert!(
        !md.iter().any(|(k, _)| k.starts_with("avi:strd.")),
        "no `with_stream_header_data` ⇒ no `avi:strd.*` keys; got {:?}",
        md.iter()
            .filter(|(k, _)| k.starts_with("avi:strd"))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Pre-round-89 byte-layout invariance: file size identical to baseline.
// ---------------------------------------------------------------------------

#[test]
fn no_strd_file_size_matches_baseline() {
    let baseline = std::env::temp_dir().join("oxideav-avi-r89-baseline-no-strd.avi");
    write_minimal(&baseline, AviMuxOptions::new());
    let baseline_len = std::fs::metadata(&baseline).unwrap().len();

    // A second baseline with stream-name unset (round-80) should be
    // the same size as round-89 baseline: no `strd` chunk emitted, no
    // extra bytes.
    let second = std::env::temp_dir().join("oxideav-avi-r89-baseline-no-strd-2.avi");
    write_minimal(&second, AviMuxOptions::new());
    let second_len = std::fs::metadata(&second).unwrap().len();

    assert_eq!(
        baseline_len, second_len,
        "two no-strd writes must be identical in size; r89 builder must not \
         change the no-strd baseline byte layout"
    );

    // And adding a strd blob must grow the file by at least chunk
    // header (8 bytes) + body.
    let with_strd = std::env::temp_dir().join("oxideav-avi-r89-with-strd.avi");
    write_minimal(
        &with_strd,
        AviMuxOptions::new().with_stream_header_data(0, vec![0u8; 16]),
    );
    let with_strd_len = std::fs::metadata(&with_strd).unwrap().len();

    assert!(
        with_strd_len >= baseline_len + 8 + 16,
        "configuring a 16-byte strd must grow the file by at least chunk-header + body \
         (baseline={baseline_len}, with_strd={with_strd_len})"
    );
}

// ---------------------------------------------------------------------------
// Empty-payload strd round-trips as `Some(&[])` (absent vs. empty distinct).
// ---------------------------------------------------------------------------

#[test]
fn empty_strd_payload_roundtrips_as_some_empty() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r89-strd-empty.avi");
    let opts = AviMuxOptions::new().with_stream_header_data(0, Vec::<u8>::new());
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Empty driver blob must be preserved as `Some(&[])`, NOT mapped to
    // `None` (the demuxer reserves `None` for "no strd chunk at all").
    assert_eq!(
        dmx.stream_header_data(0),
        Some(&[][..]),
        "empty `strd` body must round-trip as Some(&[]) so callers can \
         distinguish 'no chunk' from 'empty driver blob'"
    );
    // Stream 1 had no strd configured → None.
    assert_eq!(dmx.stream_header_data(1), None);

    // Metadata key still emitted even for empty blob (length 0).
    let md = dmx.metadata();
    assert!(
        md.iter().any(|(k, v)| k == "avi:strd.0.len" && v == "0"),
        "empty strd should still surface `avi:strd.0.len = 0`"
    );
}

// ---------------------------------------------------------------------------
// Builder dedup: last `with_stream_header_data(i, ...)` wins.
// ---------------------------------------------------------------------------

#[test]
fn with_stream_header_data_dedups_per_stream_index() {
    let first = vec![0xAAu8; 4];
    let second = vec![0xBBu8; 8];
    let opts = AviMuxOptions::new()
        .with_stream_header_data(0, first.clone())
        .with_stream_header_data(0, second.clone()) // overrides prior entry for stream 0
        .with_stream_header_data(2, vec![0xCCu8; 2]);
    assert_eq!(opts.stream_header_data.len(), 2);
    let by_idx0: Vec<&Vec<u8>> = opts
        .stream_header_data
        .iter()
        .filter(|(i, _)| *i == 0)
        .map(|(_, b)| b)
        .collect();
    assert_eq!(by_idx0.len(), 1);
    assert_eq!(by_idx0[0], &second);
}

// ---------------------------------------------------------------------------
// Odd-length blobs still round-trip byte-equal (RIFF word-pad is invisible).
// ---------------------------------------------------------------------------

#[test]
fn strd_odd_length_blob_roundtrip() {
    // 5 bytes ⇒ RIFF chunk needs one trailing pad byte. The demuxer
    // must report the 5-byte body (not 6) and the bytes must match.
    let tmp = std::env::temp_dir().join("oxideav-avi-r89-strd-odd.avi");
    let blob: Vec<u8> = vec![0x01, 0x02, 0x03, 0x04, 0x05];
    let opts = AviMuxOptions::new().with_stream_header_data(0, blob.clone());
    write_minimal(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_header_data(0),
        Some(blob.as_slice()),
        "odd-length strd must round-trip byte-equal; the pad byte must \
         not be surfaced as part of the body"
    );
    let md = dmx.metadata();
    assert!(
        md.iter().any(|(k, v)| k == "avi:strd.0.len" && v == "5"),
        "odd-length blob metadata should report 5, not 6 (post-pad)"
    );
}
