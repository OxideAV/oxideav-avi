//! Round 394 — every stream's `strl` carries its own `indx`
//! super-index.
//!
//! Clean-room source: `docs/container/riff/opendml-avi-2.0.pdf`
//! §"Index Locations in RIFF File": *"Unlike the 'idx1' chunk, a
//! single index is stored per stream in the AVI file. An 'indx' chunk
//! follows the 'strf' chunk in the LIST 'strl' chunk of an AVI
//! header."* The pre-round-394 muxer emitted an `indx` for stream 0
//! only, so a conformant reader following the `strl`-level indexes
//! had no random access into any other stream's `ix##` chunks.
//!
//! Also covers the round-394 `write_trailer` refactor that patches
//! `strh.dwLength` / `strh.dwSuggestedBufferSize` through offsets
//! recorded at `write_header` time: the old arithmetic layout walk
//! didn't account for optional `strd` / `strn` chunks, so a
//! multi-stream file with a stream name on stream 0 got stream 1's
//! `strh` patch written into the wrong bytes.

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer as _, MediaType,
    Muxer as _, Packet, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    let info = CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG"));
    reg.register(info);
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

fn audio_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
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

fn payload(seed: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed.wrapping_mul(0x9E37_79B9).wrapping_add(7);
    for _ in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 24) as u8);
    }
    out
}

/// Mux an interleaved 2-stream (video + PCM audio) file: `n` video
/// frames of `frame_len` bytes, one 96-byte (24-sample) audio packet
/// after each frame.
fn mux_av(tag: &str, n: usize, frame_len: usize, kind: AviKind, opts: AviMuxOptions) -> Vec<u8> {
    let streams = [video_stream(0), audio_stream(1)];
    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r394-psi-{tag}.avi"));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(ws, &streams, kind, opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..n {
            let mut v = Packet::new(0, streams[0].time_base, payload(i as u32, frame_len));
            v.pts = Some(i as i64);
            v.flags.keyframe = true;
            mux.write_packet(&v).unwrap();
            let mut a = Packet::new(1, streams[1].time_base, payload(0x8000 + i as u32, 96));
            a.pts = Some(i as i64 * 24);
            a.flags.keyframe = true;
            mux.write_packet(&a).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Byte-scan chunk headers with the given FourCC: `(offset, 8 + cb)`.
fn find_chunks(bytes: &[u8], fourcc: &[u8; 4]) -> Vec<(u64, u32)> {
    let mut out = Vec::new();
    let mut k = 0usize;
    while k + 8 <= bytes.len() {
        if &bytes[k..k + 4] == fourcc {
            let cb =
                u32::from_le_bytes([bytes[k + 4], bytes[k + 5], bytes[k + 6], bytes[k + 7]]) as u64;
            if cb >= 24 && (k as u64 + 8 + cb) <= bytes.len() as u64 {
                out.push((k as u64, (8 + cb) as u32));
                k += (8 + cb) as usize;
                continue;
            }
        }
        k += 1;
    }
    out
}

fn open_dmx(bytes: &[u8]) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.to_vec()));
    oxideav_avi::demuxer::open_avi(rs, &reg).unwrap()
}

#[test]
fn every_stream_gets_its_own_indx() {
    let bytes = mux_av(
        "basic",
        16,
        512,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(8 * 1024)),
        AviMuxOptions::default(),
    );
    // Two indx chunks in the header, one per strl.
    let mut indx_count = 0;
    let mut k = 0usize;
    // Only look inside the primary RIFF's hdrl region (before movi).
    let movi = bytes
        .windows(4)
        .position(|w| w == b"movi")
        .expect("movi present");
    while k + 4 <= movi {
        if &bytes[k..k + 4] == b"indx" {
            indx_count += 1;
        }
        k += 1;
    }
    assert_eq!(indx_count, 2, "one indx per stream's strl");

    let dmx = open_dmx(&bytes);
    // Stream 0 (video): entries target ix00 chunks.
    let ix00 = find_chunks(&bytes, b"ix00");
    let e0 = dmx.super_index_entries(0).expect("stream 0 indx");
    assert_eq!(e0.len(), ix00.len());
    for (e, &(off, size)) in e0.iter().zip(ix00.iter()) {
        assert_eq!(e.qw_offset, off);
        assert_eq!(e.dw_size, size);
    }
    assert_eq!(e0.iter().map(|e| e.dw_duration as u64).sum::<u64>(), 16);

    // Stream 1 (audio): entries target ix01 chunks, durations are
    // sample ticks (24 samples per 96-byte packet).
    let ix01 = find_chunks(&bytes, b"ix01");
    assert!(!ix01.is_empty(), "audio ix01 chunks emitted");
    let e1 = dmx.super_index_entries(1).expect("stream 1 indx");
    assert_eq!(e1.len(), ix01.len());
    for (e, &(off, size)) in e1.iter().zip(ix01.iter()) {
        assert_eq!(e.qw_offset, off);
        assert_eq!(e.dw_size, size);
    }
    assert_eq!(
        e1.iter().map(|e| e.dw_duration as u64).sum::<u64>(),
        16 * 24,
        "audio dwDuration is in sample ticks"
    );

    // dwChunkId spells each stream's own packet FourCC.
    assert_eq!(dmx.super_index_chunk_id(0), Some(*b"00dc"));
    assert_eq!(dmx.super_index_chunk_id(1), Some(*b"01wb"));

    // No stale targets anywhere.
    assert!(dmx.super_index_target_violations().is_empty());
}

#[test]
fn avi10_still_carries_no_indx() {
    let bytes = mux_av("avi10", 4, 256, AviKind::Avi10, AviMuxOptions::default());
    assert!(
        !bytes.windows(4).any(|w| w == b"indx"),
        "legacy envelope must not grow an indx"
    );
    let dmx = open_dmx(&bytes);
    assert!(dmx.super_index_entries(0).is_none());
    assert!(dmx.super_index_entries(1).is_none());
}

#[test]
fn strh_patches_survive_strn_and_strd_on_earlier_stream() {
    // Regression for the round-394 patch-offset refactor: a stream
    // name + codec-driver blob on stream 0 shifts every later strl,
    // which the old arithmetic layout walk did not account for —
    // stream 1's dwLength / dwSuggestedBufferSize patches landed in
    // the wrong bytes. With recorded offsets the patches must land
    // exactly, for both the legacy and OpenDML envelopes.
    for (tag, kind) in [
        ("strn-avi10", AviKind::Avi10),
        (
            "strn-odml",
            AviKind::OpenDml(RiffSegmentLimit::Bytes(1 << 30)),
        ),
    ] {
        let bytes = mux_av(
            tag,
            5,
            300,
            kind,
            AviMuxOptions::default()
                .with_stream_name(0, "main video (odd-length name!)")
                .with_stream_header_data(0, [0xAA; 13]),
        );
        let dmx = open_dmx(&bytes);
        // Stream 0: 5 video frames.
        assert_eq!(dmx.stream_length(0), Some(5), "{tag}: video dwLength");
        // Stream 1: 5 packets × 24 samples.
        assert_eq!(
            dmx.stream_length(1),
            Some(120),
            "{tag}: audio dwLength in samples"
        );
        // Suggested buffer sizes: the largest chunk per stream.
        assert_eq!(
            dmx.stream_suggested_buffer_size(0),
            Some(300),
            "{tag}: video strh.dwSuggestedBufferSize"
        );
        assert_eq!(
            dmx.stream_suggested_buffer_size(1),
            Some(96),
            "{tag}: audio strh.dwSuggestedBufferSize"
        );
        // And the side-band chunks themselves round-trip.
        assert_eq!(dmx.stream_name(0), Some("main video (odd-length name!)"));
        assert_eq!(dmx.stream_header_data(0), Some(&[0xAA; 13][..]));

        // Every packet still demuxes byte-exact.
        let mut dmx = dmx;
        let mut v = 0;
        let mut a = 0;
        loop {
            match dmx.next_packet() {
                Ok(p) => {
                    if p.stream_index == 0 {
                        assert_eq!(p.data, payload(v as u32, 300));
                        v += 1;
                    } else {
                        assert_eq!(p.data, payload(0x8000 + a as u32, 96));
                        a += 1;
                    }
                }
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("{tag}: demux error: {e}"),
            }
        }
        assert_eq!((v, a), (5, 5), "{tag}: all packets recovered");
    }
}

#[test]
fn per_stream_2field_subtype_lands_on_that_streams_indx() {
    // Register stream 1 (audio slot used as a stand-in 2-field
    // stream) — the sub-type must land on stream 1's indx and NOT on
    // stream 0's, per the per-stream wiring.
    let bytes = mux_av(
        "2field",
        4,
        256,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(1 << 30)),
        AviMuxOptions::default().with_field2_stream(1),
    );
    let dmx = open_dmx(&bytes);
    assert!(!dmx.super_index_is_2field(0), "stream 0 stays default");
    assert!(dmx.super_index_is_2field(1), "stream 1 carries 2FIELD");
}

#[test]
fn opendml_seek_works_on_non_primary_stream_via_its_indx_ix() {
    // No idx1? OpenDML files always get idx1 for the primary segment
    // here, so instead check the ix-backed accessors for stream 1:
    // the std-index surfaces are keyed per stream and must be
    // reachable for the audio stream in a multi-segment file.
    let bytes = mux_av(
        "seek1",
        16,
        512,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(8 * 1024)),
        AviMuxOptions::default(),
    );
    let dmx = open_dmx(&bytes);
    let bases = dmx.std_index_base_offsets(1);
    assert!(bases.len() >= 2, "audio ix01 in every segment");
    assert!(dmx.std_index_base_offset_violations().is_empty());
    assert_eq!(
        dmx.super_index_entries(1).map(|e| e.len()),
        Some(bases.len()),
        "stream 1 indx entry count matches its ix01 chunks"
    );
}
