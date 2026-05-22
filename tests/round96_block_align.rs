//! Round-96: reader-side `ix##` standard-index block-alignment
//! validation for CBR audio.
//!
//! Per OpenDML 2.0 §3.0 ("AVI Standard Index Chunk"), each
//! `AVISTDINDEX_ENTRY.dwSize` is the byte length of the indexed data
//! chunk. A constant-bit-rate audio stream (PCM / A-law / µ-law /
//! IMA-ADPCM) stores a whole number of `WAVEFORMATEX.nBlockAlign`
//! sample blocks per chunk, so a conformant index satisfies
//! `dwSize % nBlockAlign == 0` for every entry. The demuxer's
//! `cbr_audio_block_alignment_violations()` cross-checks this and
//! returns one `BlockAlignViolation` per offending entry.
//!
//! We drive the muxer in multi-segment OpenDML mode (a small
//! `RiffSegmentLimit::Bytes` ceiling rolls a second `RIFF AVIX`
//! segment), which makes the demuxer scan the per-segment `ix##`
//! standard indexes (the multi-segment file genuinely needs them for
//! random access). Every stream gets a segment-tail `ix##` flush, so
//! the audio stream's entries are present and scanned. The muxer
//! copies each packet's payload length verbatim into the `ix##` entry
//! `dwSize`, so feeding an audio payload whose length is not a
//! multiple of `nBlockAlign` reproduces the malformed-encoder case
//! without byte-patching.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, MediaType, Muxer, Packet,
    Rational, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

/// Registry resolving the video FourCC + PCM and MP3 wave-format tags
/// so the demuxer routes both audio carriages. No real codec crates
/// are pulled.
fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    // WAVE_FORMAT_MPEGLAYER3 == 0x0055 (VBR — sample_size == 0).
    reg.register(CodecInfo::new(CodecId::new("mp3")).tag(CodecTag::wave_format(0x0055)));
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

/// Stereo s16 PCM stream: nBlockAlign = 2ch * 2B = 4.
fn pcm_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::audio(CodecId::new("pcm_s16le")).with_tag(CodecTag::wave_format(0x0001));
    params.media_type = MediaType::Audio;
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(oxideav_core::SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// VBR MP3 stream (sample_size == 0; classified as VBR — not checked).
fn mp3_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::audio(CodecId::new("mp3")).with_tag(CodecTag::wave_format(0x0055));
    params.media_type = MediaType::Audio;
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Mux `video_stream(0)` + `audio` (index 1) into a multi-segment
/// OpenDML file. The small `RiffSegmentLimit::Bytes(4096)` ceiling
/// plus the bulky video keyframes roll a second `RIFF AVIX` segment,
/// so the demuxer scans the per-segment `ix##` indexes. Each
/// `audio_payloads` entry is written verbatim as a `01wb` chunk and
/// recorded in a segment-tail `ix01` standard index.
fn mux_file(name: &str, audio: StreamInfo, audio_payloads: &[Vec<u8>]) -> std::path::PathBuf {
    let vid = video_stream(0);
    let streams = vec![vid.clone(), audio];
    let tmp = std::env::temp_dir().join(name);
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let opts = AviMuxOptions::new();
    let mut mux = open_avi(
        ws,
        &streams,
        // 4 KiB segment ceiling: forces a second RIFF AVIX segment so
        // `movi_segments.len() > 1` triggers the demuxer's ix## scan.
        AviKind::OpenDml(RiffSegmentLimit::Bytes(4096)),
        opts,
    )
    .unwrap();
    mux.write_header().unwrap();
    // Interleave: each audio packet preceded by a bulky video keyframe
    // so the running segment byte count crosses the 4 KiB ceiling and
    // the muxer rolls a fresh AVIX segment partway through.
    for (i, payload) in audio_payloads.iter().enumerate() {
        let mut vpkt = Packet::new(0, vid.time_base, vec![(i as u8).wrapping_add(1); 1500]);
        vpkt.pts = Some(i as i64);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();

        let mut pkt = Packet::new(1, streams[1].time_base, payload.clone());
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    tmp
}

#[test]
fn aligned_cbr_audio_has_no_block_align_violations() {
    // Every audio payload is a multiple of nBlockAlign (4): 16, 32, 64
    // bytes. The mid-movi ix## entries therefore all satisfy
    // dwSize % 4 == 0.
    let payloads: Vec<Vec<u8>> = vec![vec![0u8; 16], vec![0u8; 32], vec![0u8; 64], vec![0u8; 16]];
    let path = mux_file("oxideav-avi-r96-aligned.avi", pcm_stream(1), &payloads);

    let f = std::fs::File::open(&path).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    let violations = dem.cbr_audio_block_alignment_violations();
    assert!(
        violations.is_empty(),
        "block-aligned CBR audio must report no violations, got {violations:?}"
    );
}

#[test]
fn misaligned_cbr_audio_entry_is_flagged() {
    // Payload of 18 bytes is NOT a multiple of nBlockAlign (4): the
    // ix## entry's dwSize lands at 18 and 18 % 4 == 2. Surround it with
    // aligned payloads so exactly one entry trips.
    let payloads: Vec<Vec<u8>> = vec![vec![0u8; 16], vec![0u8; 18], vec![0u8; 32]];
    let path = mux_file("oxideav-avi-r96-misaligned.avi", pcm_stream(1), &payloads);

    let f = std::fs::File::open(&path).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    let violations = dem.cbr_audio_block_alignment_violations();
    assert_eq!(
        violations.len(),
        1,
        "exactly one misaligned CBR-audio ix## entry expected, got {violations:?}"
    );
    let v = violations[0];
    assert_eq!(v.stream_index, 1, "violation must name the audio stream");
    assert_eq!(
        v.dw_size, 18,
        "dwSize must be the misaligned payload length"
    );
    assert_eq!(v.block_align, 4, "block_align must be 2ch * 2B = 4");
    // entry_index is the per-stream ordinal of the offending entry; the
    // 18-byte payload is the audio stream's second packet (ordinal 1).
    assert_eq!(
        v.entry_index, 1,
        "entry_index must be the per-stream ordinal"
    );
}

#[test]
fn vbr_audio_is_never_flagged() {
    // VBR MP3 (sample_size == 0) carries variable-length packets by
    // design; the validator must NOT check it against nBlockAlign even
    // when chunk sizes are odd.
    let payloads: Vec<Vec<u8>> = vec![vec![0u8; 17], vec![0u8; 23], vec![0u8; 19]];
    let path = mux_file("oxideav-avi-r96-vbr.avi", mp3_stream(1), &payloads);

    let f = std::fs::File::open(&path).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    let violations = dem.cbr_audio_block_alignment_violations();
    assert!(
        violations.is_empty(),
        "VBR audio must never be block-align-checked, got {violations:?}"
    );
}

#[test]
fn avi10_without_ix_chunks_has_no_violations() {
    // Pure AVI 1.0 has no ix## standard indexes, so the validator has
    // nothing to walk and returns an empty Vec regardless of payloads.
    let payloads: Vec<Vec<u8>> = vec![vec![0u8; 18], vec![0u8; 22]];
    let vid = video_stream(0);
    let aud = pcm_stream(1);
    let streams = vec![vid.clone(), aud];
    let tmp = std::env::temp_dir().join("oxideav-avi-r96-avi10.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(ws, &streams, AviKind::Avi10, AviMuxOptions::new()).unwrap();
        mux.write_header().unwrap();
        let mut vpkt = Packet::new(0, vid.time_base, vec![0u8; 96]);
        vpkt.pts = Some(0);
        vpkt.flags.keyframe = true;
        mux.write_packet(&vpkt).unwrap();
        for (i, payload) in payloads.iter().enumerate() {
            let mut pkt = Packet::new(1, streams[1].time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let f = std::fs::File::open(&tmp).unwrap();
    let dem = demuxer_open_avi(Box::new(f), &registry()).unwrap();
    assert!(
        dem.cbr_audio_block_alignment_violations().is_empty(),
        "AVI 1.0 (no ix## chunks) must yield no violations"
    );
}
