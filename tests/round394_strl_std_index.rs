//! Round 394 — in-`strl` standard index (the OpenDML compact
//! single-index layout), read + write.
//!
//! Clean-room source: `docs/container/riff/opendml-avi-2.0.pdf`
//! §"Index Locations in RIFF File": the strl `indx` *"may either be
//! an index of indexes (super index), or may be an index to the
//! chunks directly. … If the 'indx' chunk is a standard or field
//! index chunk (i.e., not an index of indexes) then the stream has
//! only one index chunk and there is none in the 'movi' data."* And
//! the growth model: *"A file can be easily grown if it has a
//! standard index in the 'indx' chunk position. The chunk can be
//! moved to a new 'ix##' chunk, and a new super index can be
//! inserted into the stream header ('indx' position)."*
//!
//! Write side: `AviMuxOptions::with_strl_std_index(cap)` reserves the
//! compact layout and falls back transparently to the two-tier
//! super-index + `ix##` layout on a `RIFF AVIX` roll or a capacity
//! overflow. Read side: `parse_strl` folds an `AVI_INDEX_OF_CHUNKS`
//! `indx` into the same per-stream std-index machinery that backs
//! seek, per-packet keyframe flags, and the `std_index_*` accessors
//! (pre-round-394 such an `indx` was dropped entry-less — and one
//! with enough entries even failed `open()` on the 16-byte-stride
//! truncation check that ran before the type check).

use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer as _, MediaType,
    Muxer as _, Packet, Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::AviIndexType;
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
    let mut state = seed.wrapping_mul(0x9E37_79B9).wrapping_add(11);
    for _ in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((state >> 24) as u8);
    }
    out
}

/// Mux `n` video frames + `n` audio packets, every 4th video frame a
/// keyframe (frame 0, 4, 8, ...), the rest deltas.
fn mux_av(tag: &str, n: usize, frame_len: usize, kind: AviKind, opts: AviMuxOptions) -> Vec<u8> {
    let streams = [video_stream(0), audio_stream(1)];
    let tmp = std::env::temp_dir().join(format!("oxideav-avi-r394-sstd-{tag}.avi"));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(ws, &streams, kind, opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..n {
            let mut v = Packet::new(0, streams[0].time_base, payload(i as u32, frame_len));
            v.pts = Some(i as i64);
            v.flags.keyframe = i % 4 == 0;
            mux.write_packet(&v).unwrap();
            let mut a = Packet::new(1, streams[1].time_base, payload(0x9000 + i as u32, 96));
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

fn open_dmx(bytes: &[u8]) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.to_vec()));
    oxideav_avi::demuxer::open_avi(rs, &reg).unwrap()
}

fn contains_fourcc(bytes: &[u8], fourcc: &[u8; 4]) -> bool {
    bytes.windows(4).any(|w| w == fourcc)
}

#[test]
fn strl_std_index_roundtrip_single_segment() {
    let bytes = mux_av(
        "roundtrip",
        12,
        400,
        AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        AviMuxOptions::default().with_strl_std_index(64),
    );

    // Per the spec: "there is none in the 'movi' data".
    assert!(!contains_fourcc(&bytes, b"ix00"), "no ix00 in movi");
    assert!(!contains_fourcc(&bytes, b"ix01"), "no ix01 in movi");

    let dmx = open_dmx(&bytes);
    // The indx chunks declare the standard-index layout...
    assert_eq!(dmx.super_index_index_type(0), Some(AviIndexType::OfChunks));
    assert_eq!(dmx.super_index_index_type(1), Some(AviIndexType::OfChunks));
    // ...and the divergence-only metadata key labels it.
    let meta = dmx.metadata().to_vec();
    for n in 0..2 {
        assert!(
            meta.iter()
                .any(|(k, v)| k == &format!("avi:indx.{n}.index_type") && v == "of_chunks"),
            "index_type key for stream {n}"
        );
    }
    // The super-entry surface stays empty (present but no super
    // entries) while the std-index machinery is fully populated.
    assert_eq!(dmx.super_index_entries(0), Some(vec![]));
    assert_eq!(dmx.std_index_index_types(0), vec![AviIndexType::OfChunks]);
    assert_eq!(dmx.std_index_index_types(1), vec![AviIndexType::OfChunks]);
    assert_eq!(dmx.std_index_base_offsets(0).len(), 1);
    assert_eq!(dmx.std_index_base_offsets(1).len(), 1);
    assert!(
        dmx.std_index_base_offset_violations().is_empty(),
        "qwBaseOffset anchors inside movi"
    );
    assert!(dmx.std_index_entry_count_violations().is_empty());

    // Per-packet keyframe flags come from the in-strl index's dwSize
    // delta bit: frames 0,4,8 key; the rest delta.
    for seq in 0..12 {
        assert_eq!(
            dmx.packet_is_keyframe(0, seq),
            Some(seq % 4 == 0),
            "video keyframe flag at seq {seq}"
        );
    }

    // Strict std-index seek resolves through the in-strl index.
    let mut dmx = dmx;
    let res = dmx.seek_to_keyframe_strict_via_std_index(0, 9).unwrap();
    let _ = res; // landing details covered by the seek suites
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.stream_index, 0);
    assert_eq!(pkt.data, payload(8, 400), "lands on keyframe 8");

    // Full linear demux stays byte-exact.
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.clone()));
    let mut dmx2 = oxideav_avi::demuxer::open_avi(rs, &registry()).unwrap();
    let (mut v, mut a) = (0u32, 0u32);
    loop {
        match dmx2.next_packet() {
            Ok(p) => {
                if p.stream_index == 0 {
                    assert_eq!(p.data, payload(v, 400));
                    v += 1;
                } else {
                    assert_eq!(p.data, payload(0x9000 + a, 96));
                    a += 1;
                }
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!((v, a), (12, 12));
}

#[test]
fn strl_std_index_falls_back_on_segment_roll() {
    // 4 KiB ceiling forces RIFF AVIX continuations → the compact
    // layout must migrate to the two-tier super-index + ix## shape.
    let bytes = mux_av(
        "roll",
        16,
        512,
        AviKind::OpenDml(RiffSegmentLimit::Bytes(4 * 1024)),
        AviMuxOptions::default().with_strl_std_index(64),
    );
    assert!(contains_fourcc(&bytes, b"ix00"), "fallback emits ix00");
    assert!(contains_fourcc(&bytes, b"AVIX"), "multi-segment fixture");

    let dmx = open_dmx(&bytes);
    assert_eq!(
        dmx.super_index_index_type(0),
        Some(AviIndexType::OfIndexes),
        "indx re-patched as a super-index"
    );
    let entries = dmx.super_index_entries(0).expect("super entries");
    assert!(!entries.is_empty());
    assert!(
        dmx.super_index_target_violations().is_empty(),
        "fallback entries point at real ix chunks"
    );

    let mut dmx = dmx;
    let mut count = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => count += 1,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(count, 32, "all packets across segments");
}

#[test]
fn strl_std_index_falls_back_on_capacity_overflow() {
    // Capacity 4 but 10 packets per stream → overflow deactivates the
    // compact layout; the reserved bytes (24 + 4*8 = 56) hold
    // (56-24)/16 = 2 super entries — enough for the single tail ix##.
    let bytes = mux_av(
        "overflow",
        10,
        300,
        AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        AviMuxOptions::default().with_strl_std_index(4),
    );
    assert!(contains_fourcc(&bytes, b"ix00"), "fallback emits ix00");

    let dmx = open_dmx(&bytes);
    assert_eq!(dmx.super_index_index_type(0), Some(AviIndexType::OfIndexes));
    let entries = dmx.super_index_entries(0).expect("super entries");
    assert_eq!(entries.len(), 1, "one tail ix00 fits the shrunk table");
    assert!(dmx.super_index_target_violations().is_empty());
    assert_eq!(
        entries[0].dw_duration, 10,
        "tail ix00 spans every video frame"
    );

    let mut dmx = dmx;
    let mut count = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => count += 1,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(count, 20);
}

#[test]
fn strl_std_index_2field_field_index() {
    // A 2-field stream's in-strl index is an AVI FIELD index: 12-byte
    // entries carrying dwOffsetField2, wLongsPerEntry = 3.
    let streams = [video_stream(0)];
    let tmp = std::env::temp_dir().join("oxideav-avi-r394-sstd-2field.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open_avi(
            ws,
            &streams,
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::default()
                .with_strl_std_index(16)
                .with_field2_stream(0),
        )
        .unwrap();
        mux.write_header().unwrap();
        for i in 0..6 {
            // Field 2 starts halfway into each 400-byte payload.
            mux.set_field2_offset(200);
            let mut v = Packet::new(0, streams[0].time_base, payload(i, 400));
            v.pts = Some(i as i64);
            v.flags.keyframe = true;
            mux.write_packet(&v).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);

    assert!(!contains_fourcc(&bytes, b"ix00"), "no ix00 in movi");
    let dmx = open_dmx(&bytes);
    assert_eq!(dmx.super_index_index_type(0), Some(AviIndexType::OfChunks));
    // The field-2 offsets surface per packet through the same
    // accessor the in-movi 2-field ix## path feeds.
    for seq in 0..6 {
        let f2 = dmx
            .field2_offset_for_packet(0, seq)
            .expect("field2 offset recorded");
        assert!(f2 > 0, "non-zero qwBaseOffset-relative field-2 offset");
    }
    // 2-field metadata key fires from the in-strl index.
    assert!(dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:ix.0.is_2field" && v == "true"));
}

#[test]
fn foreign_strl_std_index_no_longer_fails_open() {
    // Regression: a strl indx with AVI_INDEX_OF_CHUNKS and an entry
    // count whose 16-byte-stride bound exceeds the body used to fail
    // open() with "indx super-index entry table truncated" (the
    // stride check ran before the type check). Rebuild that shape by
    // writing a compact file with > (payload/16) entries and checking
    // it opens and seeks.
    let bytes = mux_av(
        "stride",
        20, // 20 entries * 8 B = 160 B table; 20*16 = 320 > 160+24
        300,
        AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        AviMuxOptions::default().with_strl_std_index(20),
    );
    let dmx = open_dmx(&bytes);
    assert_eq!(dmx.std_index_base_offsets(0).len(), 1);
    assert_eq!(
        dmx.std_index_declared_entry_counts(0),
        vec![20],
        "all 20 std entries declared and parsed"
    );
    assert!(dmx.std_index_entry_count_violations().is_empty());
}
