//! Round-101: OpenDML super-index `dwDuration` round-trip + cross-check
//! against the `dmlh` extended-header total-frame count.
//!
//! Per OpenDML 2.0 §"AVI Super Index Chunk", each
//! `_avisuperindex_entry.dwDuration` is the per-segment "time span in
//! stream ticks" of the chunks indexed by that segment's `ix##`. For a
//! one-tick-per-frame video stream that is the segment's frame count.
//! §5.0 ("Extended AVI Header") defines `dmlh.dwTotalFrames` as the
//! file's real total frame count across every `RIFF AVIX` segment, so
//! `sum(dwDuration) == dmlh.dwTotalFrames` must hold for the indexed
//! video stream.
//!
//! The muxer (round-101) emits `dwDuration` as the indexed stream's
//! per-segment frame count — not the all-stream packet total it used
//! before — so a video+audio file round-trips with the super-index
//! durations summing exactly to `dmlh`. The demuxer exposes the raw
//! per-segment values via `super_index_segment_durations()` and the
//! consistency check via `super_index_duration_violations()`.
//!
//! We drive the muxer in multi-segment OpenDML mode (a small
//! `RiffSegmentLimit::Bytes` ceiling rolls multiple `RIFF AVIX`
//! segments) so the super-index carries more than one entry and the
//! per-segment partition is exercised.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, MediaType, Muxer, Packet,
    Rational, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
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

fn pcm_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::audio(CodecId::new("pcm_s16le")).with_tag(CodecTag::wave_format(0x0001));
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

/// Mux `frames` interleaved video+audio packets into a multi-segment
/// OpenDML file. The 4 KiB ceiling plus bulky video keyframes roll
/// several `RIFF AVIX` segments. Returns the file path and the number
/// of video frames written.
fn mux_video_audio(name: &str, frames: usize) -> (std::path::PathBuf, u32) {
    let vid = video_stream(0);
    let aud = pcm_stream(1);
    let streams = vec![vid.clone(), aud.clone()];
    let tmp = std::env::temp_dir().join(name);
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(
        ws,
        &streams,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(4096)),
        AviMuxOptions::new(),
    )
    .unwrap();
    mux.write_header().unwrap();
    for i in 0..frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 1500]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();

        // One audio packet per video frame: block-aligned (nBlockAlign
        // = 4), exercising the all-stream-vs-indexed-stream distinction.
        let mut apkt = Packet::new(1, aud.time_base, vec![0u8; 64]);
        apkt.pts = Some(i as i64);
        apkt.flags.keyframe = true;
        mux.write_packet(&apkt).unwrap();
    }
    mux.write_trailer().unwrap();
    (tmp, frames as u32)
}

/// Mux a video-only multi-segment OpenDML file.
fn mux_video_only(name: &str, frames: usize) -> (std::path::PathBuf, u32) {
    let vid = video_stream(0);
    let streams = vec![vid.clone()];
    let tmp = std::env::temp_dir().join(name);
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(
        ws,
        &streams,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(4096)),
        AviMuxOptions::new(),
    )
    .unwrap();
    mux.write_header().unwrap();
    for i in 0..frames {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 1500]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();
    }
    mux.write_trailer().unwrap();
    (tmp, frames as u32)
}

#[test]
fn video_audio_super_index_durations_sum_to_dmlh() {
    // Round-101: a multi-stream file's super-index dwDuration must sum
    // to dmlh.dwTotalFrames (the video frame count), NOT the all-stream
    // packet total. Pre-round-101 the muxer wrote the all-stream count,
    // which would have been ~2x the video count and tripped the check.
    let (path, frames) = mux_video_audio("oxideav-avi-r101-va.avi", 8);

    let f = std::fs::File::open(&path).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    // dmlh carries the real total frame count across all segments.
    assert_eq!(
        dem.dmlh_total_frames(),
        Some(frames as u64),
        "dmlh.dwTotalFrames must equal the video frame count"
    );

    // Per-segment durations: more than one entry (multi-segment) and
    // they sum to the video frame count.
    let durations = dem.super_index_segment_durations(0);
    assert!(
        durations.len() > 1,
        "expected a multi-segment super-index, got {} entr(ies)",
        durations.len()
    );
    let sum: u64 = durations.iter().map(|&d| d as u64).sum();
    assert_eq!(
        sum, frames as u64,
        "per-segment dwDuration must sum to the video frame count, got {durations:?}"
    );

    // No violation: index agrees with the extended header.
    let violations = dem.super_index_duration_violations();
    assert!(
        violations.is_empty(),
        "consistent video+audio file must report no violations, got {violations:?}"
    );
}

#[test]
fn video_only_super_index_durations_sum_to_dmlh() {
    let (path, frames) = mux_video_only("oxideav-avi-r101-vonly.avi", 6);

    let f = std::fs::File::open(&path).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();

    let durations = dem.super_index_segment_durations(0);
    let sum: u64 = durations.iter().map(|&d| d as u64).sum();
    assert_eq!(sum, frames as u64, "video-only durations: {durations:?}");
    assert!(
        dem.super_index_duration_violations().is_empty(),
        "video-only consistent file must report no violations"
    );
}

#[test]
fn mismatched_dmlh_is_flagged() {
    // Byte-patch dmlh.dwTotalFrames to a wrong value so the super-index
    // total no longer matches; the cross-check must flag exactly the
    // video stream. We do NOT touch the super-index, only the extended
    // header, reproducing a writer whose dmlh disagrees with its index.
    let (path, frames) = mux_video_audio("oxideav-avi-r101-mismatch.avi", 8);
    let mut bytes = std::fs::read(&path).unwrap();

    // Locate the `dmlh` chunk: 4-CC `dmlh`, then 4-byte cb, then the
    // dwTotalFrames DWORD. Patch dwTotalFrames to frames + 100.
    let pos = bytes
        .windows(4)
        .position(|w| w == b"dmlh")
        .expect("dmlh chunk must be present in OpenDML output");
    let val_off = pos + 8; // skip 4-CC + cb
    let bogus = (frames + 100).to_le_bytes();
    bytes[val_off..val_off + 4].copy_from_slice(&bogus);
    std::fs::write(&path, &bytes).unwrap();

    let f = std::fs::File::open(&path).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    assert_eq!(
        dem.dmlh_total_frames(),
        Some((frames + 100) as u64),
        "patched dmlh must read back as the bogus value"
    );

    let violations = dem.super_index_duration_violations();
    assert_eq!(
        violations.len(),
        1,
        "exactly one mismatched video super-index expected, got {violations:?}"
    );
    let v = violations[0];
    assert_eq!(v.stream_index, 0, "violation must name the video stream");
    assert_eq!(
        v.super_index_duration_total, frames as u64,
        "reported super-index total must be the real frame count"
    );
    assert_eq!(
        v.dmlh_total_frames,
        (frames + 100) as u64,
        "reported dmlh value must be the bogus patched value"
    );
}

#[test]
fn avi10_without_super_index_yields_no_durations_or_violations() {
    // Pure AVI 1.0 has no indx super-index and no dmlh, so both the
    // accessor and the validator return empty.
    let vid = video_stream(0);
    let streams = vec![vid.clone()];
    let tmp = std::env::temp_dir().join("oxideav-avi-r101-avi10.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::new()).unwrap();
        mux.write_header().unwrap();
        for i in 0..3 {
            let mut vpkt = Packet::new(0, vid.time_base, vec![0u8; 96]);
            vpkt.pts = Some(i as i64);
            vpkt.flags.keyframe = true;
            mux.write_packet(&vpkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    assert!(
        dem.super_index_segment_durations(0).is_empty(),
        "AVI 1.0 (no indx) must yield no per-segment durations"
    );
    assert!(
        dem.super_index_duration_violations().is_empty(),
        "AVI 1.0 (no dmlh / no indx) must yield no violations"
    );
}
