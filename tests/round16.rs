//! Round-16 AVI feature tests.
//!
//! Covers:
//! - **C1** `AviMuxOptions::synthesise_idx1_from_ix(true)` —
//!   rebuilds the primary segment's `idx1` body from each stream's
//!   `ix##` standard-index records instead of the muxer's running
//!   per-packet `IndexEntry` collection. Per AVI 1.0 + OpenDML 2.0
//!   §"Index Locations": AVI 1.0-only readers honour `idx1` alone
//!   (they don't walk OpenDML `ix##` super-indexes), so an
//!   OpenDML-muxed file without `idx1` can't be seeked by them.
//!   The synthesiser closes that compat gap and serves as a
//!   round-trip self-consistency check between the two index views.
//! - **C4** Wider `WAVE_FORMAT_*` constants (AC-3 / DTS / WMA1 /
//!   WMA2 / WMA Pro / WMA Lossless / Opus / AAC-ADTS) plus the
//!   round-14 C2 VBR/CBR validator extension that classifies all
//!   eight tags as VBR (require `strh.dwSampleSize == 0`).

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, MediaType, Muxer,
    Packet, Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::{
    open_avi as demuxer_open_avi, open_avi_lenient, WAVE_FORMAT_AAC_ADTS, WAVE_FORMAT_AC3,
    WAVE_FORMAT_DTS, WAVE_FORMAT_OPUS, WAVE_FORMAT_WMA1, WAVE_FORMAT_WMA2,
    WAVE_FORMAT_WMA_LOSSLESS, WAVE_FORMAT_WMA_PRO,
};
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions, RiffSegmentLimit};

// ---------------------------------------------------------------------------
// Test fixtures.
// ---------------------------------------------------------------------------

fn registry_with_video_and_audio() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg.register(CodecInfo::new(CodecId::new("pcm_s16le")).tag(CodecTag::wave_format(0x0001)));
    reg.register(CodecInfo::new(CodecId::new("ac3")).tag(CodecTag::wave_format(WAVE_FORMAT_AC3)));
    reg.register(CodecInfo::new(CodecId::new("dts")).tag(CodecTag::wave_format(WAVE_FORMAT_DTS)));
    reg.register(CodecInfo::new(CodecId::new("wma1")).tag(CodecTag::wave_format(WAVE_FORMAT_WMA1)));
    reg.register(CodecInfo::new(CodecId::new("wma2")).tag(CodecTag::wave_format(WAVE_FORMAT_WMA2)));
    reg.register(
        CodecInfo::new(CodecId::new("wma_pro")).tag(CodecTag::wave_format(WAVE_FORMAT_WMA_PRO)),
    );
    reg.register(
        CodecInfo::new(CodecId::new("wma_lossless"))
            .tag(CodecTag::wave_format(WAVE_FORMAT_WMA_LOSSLESS)),
    );
    reg.register(CodecInfo::new(CodecId::new("opus")).tag(CodecTag::wave_format(WAVE_FORMAT_OPUS)));
    reg.register(
        CodecInfo::new(CodecId::new("aac_adts")).tag(CodecTag::wave_format(WAVE_FORMAT_AAC_ADTS)),
    );
    reg
}

fn magicyuv_stream(index: u32, width: u32, height: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
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
    params.sample_format = Some(oxideav_core::SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn synth_payload(seed: u32, len: usize) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        out.push((s >> 24) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// C1: idx1-from-ix synthesis.
// ---------------------------------------------------------------------------

/// Locate every `idx1` chunk in `bytes` and return its body bytes.
/// idx1 is at the top level (sibling of `LIST hdrl` / `LIST movi`)
/// inside the primary RIFF, so a flat 4-byte FourCC scan is enough.
fn find_idx1_bodies(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= bytes.len() {
        if &bytes[i..i + 4] == b"idx1" {
            let size = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]])
                as usize;
            let start = i + 8;
            if start + size <= bytes.len() {
                out.push(bytes[start..start + size].to_vec());
            }
            i = start + size;
        } else {
            i += 1;
        }
    }
    out
}

#[test]
fn synthesise_idx1_from_ix_emits_idx1_with_per_packet_entries() {
    // Round-16 C1: a 4-frame OpenDML file with synthesise_idx1_from_ix
    // on emits an idx1 with one 16-B entry per packet (= 4 entries =
    // 64 bytes). The flag DWORD on each entry is AVIIF_KEYFRAME
    // (= 0x10) since every frame is marked keyframe.
    let stream = magicyuv_stream(0, 64, 64);
    let frames: Vec<Vec<u8>> = (0..4).map(|i| synth_payload(i + 16100, 128)).collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r16-synth-idx1.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().synthesise_idx1_from_ix(true);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    let bodies = find_idx1_bodies(&bytes);
    assert_eq!(
        bodies.len(),
        1,
        "synthesise_idx1_from_ix must still emit exactly one idx1 chunk"
    );
    let body = &bodies[0];
    assert_eq!(
        body.len(),
        4 * 16,
        "idx1 body must hold 4 × 16-B entries (one per packet)"
    );
    // Walk the entries: each is (ckid[4], flags[4], offset[4], size[4]).
    for (i, e) in body.chunks(16).enumerate() {
        assert_eq!(&e[0..4], b"00dc", "entry {i} ckid must be 00dc");
        let flags = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
        let size = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
        assert_eq!(flags & 0x10, 0x10, "entry {i} must carry AVIIF_KEYFRAME");
        assert_eq!(size, 128, "entry {i} size must equal payload length");
    }

    // Round-trip verification: the demuxer reads the synthesised idx1
    // and surfaces 4 packets via next_packet (= 4 idx-driven calls).
    let reg = registry_with_video_and_audio();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    let mut total = 0usize;
    loop {
        match dmx.next_packet() {
            Ok(pkt) => {
                assert_eq!(pkt.data.len(), 128);
                total += 1;
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e:?}"),
        }
    }
    assert_eq!(
        total, 4,
        "demuxer must surface every packet via the synthesised idx1"
    );
}

#[test]
fn synthesise_idx1_offsets_match_default_path() {
    // Round-16 C1: with both options producing idx1 from the same
    // packet stream, the resulting per-packet offsets MUST match
    // (the synthesiser converts ix##.dwOffset back to movi-relative
    // form via `dw_offset - 4`, which is the inverse of
    // write_packet's recording math). Compare default vs synthesised
    // for an OpenDML-mode mux of the same packet stream.
    let stream = magicyuv_stream(0, 64, 64);
    let frames: Vec<Vec<u8>> = (0..6).map(|i| synth_payload(i + 16200, 200)).collect();

    fn run(opts: AviMuxOptions, name: &str, stream: &StreamInfo, frames: &[Vec<u8>]) -> Vec<u8> {
        let tmp = std::env::temp_dir().join(format!("oxideav-avi-r16-cmp-{name}.avi"));
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
        std::fs::read(&tmp).unwrap()
    }
    let default_bytes = run(AviMuxOptions::new(), "default", &stream, &frames);
    let synth_bytes = run(
        AviMuxOptions::new().synthesise_idx1_from_ix(true),
        "synth",
        &stream,
        &frames,
    );

    let default_idx1 = find_idx1_bodies(&default_bytes);
    let synth_idx1 = find_idx1_bodies(&synth_bytes);
    assert_eq!(default_idx1.len(), 1);
    assert_eq!(synth_idx1.len(), 1);
    assert_eq!(
        default_idx1[0], synth_idx1[0],
        "synthesised idx1 must byte-equal the default path's idx1 \
         (same primary segment, same packets, same flags)"
    );
}

#[test]
fn synthesise_idx1_default_off_keeps_round3_behaviour() {
    // Round-16 C1: default `synthesise_idx1_from_ix == false` keeps
    // the round-3 path. With the option off the muxer never touches
    // the snapshot vector (so OpenDML-mode files don't pay the per-
    // packet bookkeeping cost). We assert the produced file is
    // identical to the same mux without ever calling the builder.
    let stream = magicyuv_stream(0, 64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synth_payload(i + 16300, 64)).collect();

    fn run(opts: AviMuxOptions, name: &str, stream: &StreamInfo, frames: &[Vec<u8>]) -> Vec<u8> {
        let tmp = std::env::temp_dir().join(format!("oxideav-avi-r16-default-{name}.avi"));
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(stream),
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
        std::fs::read(&tmp).unwrap()
    }
    let baseline = run(AviMuxOptions::new(), "baseline", &stream, &frames);
    let off = run(
        AviMuxOptions::new().synthesise_idx1_from_ix(false),
        "off",
        &stream,
        &frames,
    );
    assert_eq!(
        baseline, off,
        "synthesise_idx1_from_ix(false) must be a no-op"
    );
}

#[test]
fn synthesise_idx1_avi10_mode_is_no_op() {
    // Round-16 C1: synthesise_idx1_from_ix only fires in OpenDml mode
    // (Avi10 has no ix## chunks to walk). With the option set on an
    // Avi10 mux the produced file must byte-equal the default path.
    let stream = magicyuv_stream(0, 64, 64);
    let frames: Vec<Vec<u8>> = (0..3).map(|i| synth_payload(i + 16400, 64)).collect();

    fn run(opts: AviMuxOptions, name: &str, stream: &StreamInfo, frames: &[Vec<u8>]) -> Vec<u8> {
        let tmp = std::env::temp_dir().join(format!("oxideav-avi-r16-avi10-{name}.avi"));
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(ws, std::slice::from_ref(stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
        std::fs::read(&tmp).unwrap()
    }
    let baseline = run(AviMuxOptions::new(), "default", &stream, &frames);
    let with_opt = run(
        AviMuxOptions::new().synthesise_idx1_from_ix(true),
        "synth",
        &stream,
        &frames,
    );
    assert_eq!(
        baseline, with_opt,
        "synthesise_idx1_from_ix must be a no-op for AviKind::Avi10"
    );
}

#[test]
fn synthesise_idx1_with_audio_video_mux() {
    // Round-16 C1: a multi-stream OpenDML mux (video + audio) with
    // synthesise_idx1_from_ix on. The synthesised idx1 entries must
    // include both stream FourCCs (`00dc` for vid0, `01wb` for aud1)
    // and round-trip through the demuxer.
    let video = magicyuv_stream(0, 64, 64);
    let audio = pcm_stream(1);
    let streams = [video.clone(), audio.clone()];
    let v_payload = synth_payload(16500, 256);
    let a_payload = synth_payload(16501, 1024);

    let tmp = std::env::temp_dir().join("oxideav-avi-r16-synth-av.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().synthesise_idx1_from_ix(true);
        let mut mux = open_avi(
            ws,
            &streams,
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            opts,
        )
        .unwrap();
        mux.write_header().unwrap();
        for i in 0..3 {
            let mut p = Packet::new(0, video.time_base, v_payload.clone());
            p.pts = Some(i);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
            let mut p = Packet::new(1, audio.time_base, a_payload.clone());
            p.pts = Some(i);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    let bodies = find_idx1_bodies(&bytes);
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    // 6 packets × 16 = 96 bytes.
    assert_eq!(body.len(), 96);
    // Count each stream FourCC.
    let mut n_vid = 0;
    let mut n_aud = 0;
    for e in body.chunks(16) {
        if &e[0..4] == b"00dc" {
            n_vid += 1;
        } else if &e[0..4] == b"01wb" {
            n_aud += 1;
        }
    }
    assert_eq!(n_vid, 3, "must hold one entry per video packet");
    assert_eq!(n_aud, 3, "must hold one entry per audio packet");

    // Round-trip via the demuxer: 6 packets total.
    let reg = registry_with_video_and_audio();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();
    let mut total = 0usize;
    loop {
        match dmx.next_packet() {
            Ok(_) => total += 1,
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e:?}"),
        }
    }
    assert_eq!(total, 6);
}

// ---------------------------------------------------------------------------
// C4: WAVE_FORMAT_* constants + VBR validator extension.
// ---------------------------------------------------------------------------

#[test]
fn wave_format_constants_have_canonical_values() {
    // Round-16 C4: per Microsoft mmreg.h. Pin the literal bytes so
    // a future refactor of the public constants can't silently drift.
    assert_eq!(WAVE_FORMAT_AC3, 0x2000);
    assert_eq!(WAVE_FORMAT_DTS, 0x2001);
    assert_eq!(WAVE_FORMAT_WMA1, 0x0160);
    assert_eq!(WAVE_FORMAT_WMA2, 0x0161);
    assert_eq!(WAVE_FORMAT_WMA_PRO, 0x0162);
    assert_eq!(WAVE_FORMAT_WMA_LOSSLESS, 0x0163);
    assert_eq!(WAVE_FORMAT_OPUS, 0x704F);
    assert_eq!(WAVE_FORMAT_AAC_ADTS, 0x1601);
}

/// Build a minimal AVI byte buffer with one audio stream whose
/// WAVEFORMATEX `wFormatTag` is `tag` and whose `strh.dwSampleSize`
/// is `sample_size`. Used to drive the round-14 C2 validator from
/// the demuxer side.
fn build_minimal_avi_audio(tag: u16, sample_size: u32) -> Vec<u8> {
    // Layout (matching what `build_strf` / muxer would emit):
    //   RIFF + size + AVI
    //   LIST hdrl
    //     avih (56 B body)
    //     LIST strl
    //       strh ('auds' 56 B body)
    //       strf (WAVEFORMATEX, 18 B)
    //   LIST movi (empty body)
    let mut body = Vec::new();
    // hdrl
    let mut hdrl = Vec::new();
    hdrl.extend_from_slice(b"hdrl");
    // avih
    let mut avih_body = vec![0u8; 56];
    // dwMicroSecPerFrame=0, dwMaxBytesPerSec=0, dwPaddingGranularity=0,
    // dwFlags=0x810, dwTotalFrames=0, dwInitialFrames=0, dwStreams=1, ...
    avih_body[12..16].copy_from_slice(&0x810u32.to_le_bytes());
    avih_body[24..28].copy_from_slice(&1u32.to_le_bytes()); // dwStreams
    hdrl.extend_from_slice(b"avih");
    hdrl.extend_from_slice(&(avih_body.len() as u32).to_le_bytes());
    hdrl.extend_from_slice(&avih_body);
    // strl
    let mut strl = Vec::new();
    strl.extend_from_slice(b"strl");
    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"auds");
    // dwScale at 20, dwRate at 24, dwLength at 32, dwSampleSize at 44
    strh[20..24].copy_from_slice(&1u32.to_le_bytes()); // scale
    strh[24..28].copy_from_slice(&48000u32.to_le_bytes()); // rate
    strh[44..48].copy_from_slice(&sample_size.to_le_bytes());
    strl.extend_from_slice(b"strh");
    strl.extend_from_slice(&(strh.len() as u32).to_le_bytes());
    strl.extend_from_slice(&strh);
    // strf — WAVEFORMATEX, 18 bytes
    let mut wfx = Vec::new();
    wfx.extend_from_slice(&tag.to_le_bytes());
    wfx.extend_from_slice(&2u16.to_le_bytes()); // channels
    wfx.extend_from_slice(&48_000u32.to_le_bytes()); // samples_per_sec
    wfx.extend_from_slice(&192_000u32.to_le_bytes()); // avg_bytes_per_sec
    wfx.extend_from_slice(&4u16.to_le_bytes()); // block_align
    wfx.extend_from_slice(&16u16.to_le_bytes()); // bits_per_sample
    wfx.extend_from_slice(&0u16.to_le_bytes()); // cbSize
    strl.extend_from_slice(b"strf");
    strl.extend_from_slice(&(wfx.len() as u32).to_le_bytes());
    strl.extend_from_slice(&wfx);
    // wrap strl in LIST
    hdrl.extend_from_slice(b"LIST");
    hdrl.extend_from_slice(&(strl.len() as u32).to_le_bytes());
    hdrl.extend_from_slice(&strl);
    body.extend_from_slice(b"LIST");
    body.extend_from_slice(&(hdrl.len() as u32).to_le_bytes());
    body.extend_from_slice(&hdrl);
    // movi (empty)
    let movi: &[u8] = b"movi";
    body.extend_from_slice(b"LIST");
    body.extend_from_slice(&(movi.len() as u32).to_le_bytes());
    body.extend_from_slice(movi);

    // RIFF wrapper: "AVI " + body.
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    let total_size = (4 + body.len()) as u32;
    out.extend_from_slice(&total_size.to_le_bytes());
    out.extend_from_slice(b"AVI ");
    out.extend_from_slice(&body);
    out
}

#[test]
fn round16_c4_vbr_codecs_require_zero_sample_size() {
    let reg = registry_with_video_and_audio();
    for &tag in &[
        WAVE_FORMAT_AC3,
        WAVE_FORMAT_DTS,
        WAVE_FORMAT_WMA1,
        WAVE_FORMAT_WMA2,
        WAVE_FORMAT_WMA_PRO,
        WAVE_FORMAT_WMA_LOSSLESS,
        WAVE_FORMAT_OPUS,
        WAVE_FORMAT_AAC_ADTS,
    ] {
        // Non-zero sample_size for a VBR codec must be rejected.
        let bytes = build_minimal_avi_audio(tag, 4);
        let cur = std::io::Cursor::new(bytes);
        let rs: Box<dyn ReadSeek> = Box::new(cur);
        let res = demuxer_open_avi(rs, &reg);
        match res {
            Err(Error::InvalidData(msg)) => {
                assert!(
                    msg.contains(&format!("0x{tag:04X}")),
                    "validator error must name the offending tag (got: {msg})"
                );
            }
            Ok(_) => {
                panic!("VBR codec 0x{tag:04X} with sample_size=4 should fail validation")
            }
            Err(e) => panic!("expected InvalidData for VBR codec 0x{tag:04X}, got: {e:?}"),
        }

        // sample_size == 0 is the valid VBR carriage and must parse.
        let bytes = build_minimal_avi_audio(tag, 0);
        let cur = std::io::Cursor::new(bytes);
        let rs: Box<dyn ReadSeek> = Box::new(cur);
        let dmx = demuxer_open_avi(rs, &reg)
            .unwrap_or_else(|e| panic!("VBR codec 0x{tag:04X} sample_size=0 must parse: {e:?}"));
        assert_eq!(dmx.streams().len(), 1);
    }
}

#[test]
fn round16_c4_lenient_accepts_vbr_codec_with_nonzero_sample_size() {
    // The lenient demuxer entry-point must still let the file
    // through (re-mux / inspection use case for malformed files).
    let reg = registry_with_video_and_audio();
    let bytes = build_minimal_avi_audio(WAVE_FORMAT_AC3, 8);
    let cur = std::io::Cursor::new(bytes);
    let rs: Box<dyn ReadSeek> = Box::new(cur);
    open_avi_lenient(rs, &reg).expect("lenient open must skip the VBR sample-size validator");
}
