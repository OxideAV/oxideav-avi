//! Regression tests for findings of the `mux_roundtrip` /
//! `structured_mutate` / `demux` fuzz targets.
//!
//! Each test rebuilds the minimised recipe through the public muxer API
//! and states the container rule the finding turned on.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_avi::demuxer::{AviDemuxer, open_avi, open_avi_strict};
use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};
use oxideav_core::{
    CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Demuxer, Error, Muxer, Packet,
    Rational, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for SharedBuf {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(CodecInfo::new(CodecId::new("magicyuv")).tag(CodecTag::fourcc(b"M8RG")));
    reg
}

fn video_stream() -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
    params.width = Some(112);
    params.height = Some(112);
    params.frame_rate = Some(Rational::new(15, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 15),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// What to write after frame `i`.
#[derive(Clone, Copy, PartialEq)]
enum Side {
    None,
    PaletteAndText,
}

/// Mux `(payload length, keyframe)` frames on one video stream, frame
/// `i` at `pts` `i`, optionally followed by a palette change and a text
/// chunk each.
fn mux(frames: &[(usize, bool)], kind: AviKind, side: Side) -> Vec<u8> {
    let stream = video_stream();
    let buf = SharedBuf::default();
    let ws: Box<dyn WriteSeek> = Box::new(buf.clone());
    let mut m = oxideav_avi::muxer::open_avi(
        ws,
        std::slice::from_ref(&stream),
        kind,
        AviMuxOptions::new(),
    )
    .unwrap();
    m.write_header().unwrap();
    for (i, &(len, key)) in frames.iter().enumerate() {
        let data: Vec<u8> = (0..len).map(|b| (b as u8) ^ (i as u8)).collect();
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(i as i64);
        pkt.flags.keyframe = key;
        m.write_packet(&pkt).unwrap();
        if side == Side::PaletteAndText {
            m.write_palette_change(0, &[0, 2, 0, 0, 10, 20, 30, 0, 40, 50, 60, 0])
                .unwrap();
            m.write_text_chunk(0, b"overlay").unwrap();
        }
    }
    m.write_trailer().unwrap();
    drop(m);
    Arc::try_unwrap(buf.0)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap()
        .into_inner()
}

fn open(bytes: &[u8]) -> AviDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    open_avi_strict(rs, &registry()).expect("strict open of muxer output");
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    open_avi(rs, &registry()).unwrap()
}

/// `(pts, size, keyframe)` of every packet until end of file.
fn drain(dmx: &mut AviDemuxer) -> Vec<(i64, usize, bool)> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push((p.pts.unwrap(), p.data.len(), p.flags.keyframe)),
            Err(Error::Eof) => return out,
            Err(e) => panic!("next_packet: {e}"),
        }
    }
}

const KINDS: [AviKind; 3] = [
    AviKind::Avi10,
    AviKind::OpenDml(RiffSegmentLimit::OneGiB),
    // 4 KiB segments: every few frames roll a `RIFF AVIX`.
    AviKind::OpenDml(RiffSegmentLimit::Bytes(4096)),
];

/// Scheduled fuzz run 43601510806, `mux_roundtrip`, input
/// `00 d8 d7 d8 d7 0e f5`: one video stream, twenty zero-length packets.
///
/// With `dwSampleSize` 0 a chunk is one frame-time, and a zero-length
/// chunk is a frame-time with no frame (AVI 1.0 §AVISTREAMHEADER; only
/// an `AVIIF_NO_TIME` chunk "does not affect timing", §AVIOLDINDEX). The
/// file holds twenty time slots and no packet: it must open (strictly),
/// read as immediately at end of file, and refuse a seek without
/// panicking. The harness used to demand twenty empty packets back.
#[test]
fn all_zero_length_video_is_twenty_slots_and_no_packets() {
    for kind in KINDS {
        let bytes = mux(&[(0, true); 20], kind, Side::None);
        let mut dmx = open(&bytes);
        assert_eq!(dmx.streams().len(), 1);
        assert_eq!(drain(&mut dmx), []);
        assert_eq!(dmx.keyframe_indexed_packet_count(0), 0);
        assert!(dmx.seek_to(0, 0).is_err(), "no frame to land on");
        assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    }
}

/// Zero-length packets between real frames: every returned packet keeps
/// the frame slot it was written at, and a seek lands on real frames.
#[test]
fn zero_length_slots_keep_frame_timestamps() {
    let frames = [
        (0, true),
        (40, true),
        (0, false),
        (0, true),
        (9, false),
        (33, true),
        (0, false),
        (5, false),
    ];
    for kind in KINDS {
        let bytes = mux(&frames, kind, Side::None);
        let mut dmx = open(&bytes);
        let want = [(1, 40, true), (4, 9, false), (5, 33, true), (7, 5, false)];
        assert_eq!(drain(&mut dmx), want);
        for (seq, w) in want.iter().enumerate() {
            assert_eq!(dmx.packet_is_keyframe(0, seq), Some(w.2));
        }
        assert_eq!(dmx.keyframe_indexed_packet_count(0), want.len());
        // Slot 0 is empty: the first key frame is the landing point.
        assert_eq!(dmx.seek_to(0, 0).unwrap(), 1);
        assert_eq!(dmx.next_packet().unwrap().pts, Some(1));
        // Slot 3 is an empty "key" slot: land on the frame at 1.
        assert_eq!(dmx.seek_to(0, 4).unwrap(), 1);
        assert_eq!(dmx.seek_to(0, 6).unwrap(), 5);
        assert_eq!(drain(&mut dmx), [(5, 33, true), (7, 5, false)]);
    }
}

/// `mux_roundtrip` neighbour (recipe: palette change + text chunk after
/// every frame): the side-band chunks' `idx1` entries were counted as
/// frames — `packet_is_keyframe` went out of step with the packets
/// returned, and a seek resumed with timestamps advanced by one tick per
/// side-band chunk passed.
///
/// A palette change "does not affect timing" (AVI 1.0 §AVIOLDINDEX,
/// `AVIIF_NO_TIME`), and the OpenDML standard index lists frames only
/// ("Palette changes should not appear in the same Index List as ##dc
/// chunks", OpenDML "Index Locations in RIFF File").
#[test]
fn side_band_chunks_take_no_frame_slot_in_any_index() {
    let frames: Vec<(usize, bool)> = (0..40).map(|i| (300 + i, i % 8 == 0)).collect();
    for kind in KINDS {
        let bytes = mux(&frames, kind, Side::PaletteAndText);
        let mut dmx = open(&bytes);
        let want: Vec<(i64, usize, bool)> = frames
            .iter()
            .enumerate()
            .map(|(i, &(len, key))| (i as i64, len, key))
            .collect();
        assert_eq!(drain(&mut dmx), want);
        assert_eq!(dmx.keyframe_indexed_packet_count(0), frames.len());
        for (seq, w) in want.iter().enumerate() {
            assert_eq!(dmx.packet_is_keyframe(0, seq), Some(w.2), "packet {seq}");
        }
        for target in [0i64, 7, 8, 21, 39, 1000] {
            let key = (target.min(39) / 8) * 8;
            assert_eq!(dmx.seek_to(0, target).unwrap(), key, "seek {target}");
            let rest = drain(&mut dmx);
            assert_eq!(rest, want[key as usize..], "packets after seek {target}");
        }
        // The strict standard-index seek agrees wherever the file has one.
        if !matches!(kind, AviKind::Avi10) {
            let r = dmx.seek_to_keyframe_strict_via_std_index(0, 21).unwrap();
            assert_eq!(r.landed_pts, 16);
            assert_eq!(drain(&mut dmx), want[16..]);
        }
    }
}

/// The idx1-only helper never lands on a side-band chunk or an empty slot.
#[test]
fn first_video_keyframe_after_skips_side_band_and_empty_slots() {
    let frames = [(20, true), (21, false), (0, true), (23, false), (24, true)];
    let bytes = mux(&frames, AviKind::Avi10, Side::PaletteAndText);
    let mut dmx = open(&bytes);
    let r = dmx.seek_to_first_video_keyframe_after(0, 1).unwrap();
    assert_eq!(r.landed_pts, 4);
    assert_eq!(drain(&mut dmx), [(4, 24, true)]);
    // A palette change's position is the count of packets returned
    // before it: the empty slot is not one of them.
    assert_eq!(dmx.palette_change_packet_positions(0), [1, 2, 2, 3, 4]);
}

// ───────────── hostile index shapes (audit findings) ─────────────

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len() + 1);
    v.extend_from_slice(id);
    v.extend_from_slice(&(body.len() as u32).to_le_bytes());
    v.extend_from_slice(body);
    if body.len() % 2 == 1 {
        v.push(0);
    }
    v
}

fn list(id: &[u8; 4], form: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + body.len());
    v.extend_from_slice(id);
    v.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
    v.extend_from_slice(form);
    v.extend_from_slice(body);
    v
}

fn dwords(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// An empty `ix00` standard index (AVISTDINDEX header, no entries)
/// whose `qwBaseOffset` is `base`.
fn empty_ix00(base: u64) -> Vec<u8> {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&2u16.to_le_bytes()); // wLongsPerEntry
    body.extend_from_slice(&[0, 1]); // bIndexSubType, AVI_INDEX_OF_CHUNKS
    body.extend_from_slice(&0u32.to_le_bytes()); // nEntriesInUse
    body.extend_from_slice(b"00dc");
    body.extend_from_slice(&base.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    chunk(b"ix00", &body)
}

/// `hdrl` for one `M8RG` video stream whose `strl` carries a
/// super-index with the given `qwOffset`s.
fn hdrl_with_super_index(targets: &[u64]) -> Vec<u8> {
    // AVIMAINHEADER: 25 fps, one stream, 32x16.
    let avih = dwords(&[40_000, 0, 0, 0, 0, 0, 1, 0, 32, 16, 0, 0, 0, 0]);
    // AVISTREAMHEADER: vids / M8RG, dwScale 1, dwRate 25.
    let mut strh = Vec::new();
    strh.extend_from_slice(b"vidsM8RG");
    strh.extend_from_slice(&dwords(&[0, 0, 0, 1, 25, 0, 0, 0, 0, 0]));
    strh.extend_from_slice(&[0; 8]); // rcFrame
    // BITMAPINFOHEADER.
    let mut strf = dwords(&[40, 32, 16]);
    strf.extend_from_slice(&1u16.to_le_bytes());
    strf.extend_from_slice(&24u16.to_le_bytes());
    strf.extend_from_slice(b"M8RG");
    strf.extend_from_slice(&dwords(&[0, 0, 0, 0, 0]));
    // AVISUPERINDEX.
    let mut indx = Vec::with_capacity(24 + 16 * targets.len());
    indx.extend_from_slice(&4u16.to_le_bytes());
    indx.extend_from_slice(&[0, 0]); // AVI_INDEX_OF_INDEXES
    indx.extend_from_slice(&(targets.len() as u32).to_le_bytes());
    indx.extend_from_slice(b"00dc");
    indx.extend_from_slice(&[0; 12]);
    for &t in targets {
        indx.extend_from_slice(&t.to_le_bytes());
        indx.extend_from_slice(&dwords(&[32, 0]));
    }
    let strl = [
        chunk(b"strh", &strh),
        chunk(b"strf", &strf),
        chunk(b"indx", &indx),
    ]
    .concat();
    list(
        b"LIST",
        b"hdrl",
        &[chunk(b"avih", &avih), list(b"LIST", b"strl", &strl)].concat(),
    )
}

/// A super-index of 40 000 entries against 20 000 `ix00` chunks, each
/// in its own `RIFF AVIX` segment — a 2 MiB file. Every lookup the
/// validators and the open-time metadata pass make (super-index target
/// → `ix##` chunk, `qwBaseOffset` → `movi` segment) used to scan a
/// whole table per item; they are hashed / binary-searched now, and
/// must still report exactly the entries that are wrong.
#[test]
fn many_segments_and_a_long_super_index_validate_exactly() {
    const SEGMENTS: usize = 20_000;
    const ENTRIES: usize = 2 * SEGMENTS;
    // Layout is fixed-size, so every offset is known up front.
    let hdrl_len = hdrl_with_super_index(&vec![0; ENTRIES]).len() as u64;
    let primary_len = 12 + hdrl_len + 12 + 8; // RIFF, hdrl, movi, one frame chunk
    let segment_len = 12 + 12 + 32; // RIFF AVIX, LIST movi, ix00
    let ix_offset = |k: usize| primary_len + (k * segment_len) as u64 + 24;
    // Even entries point at the k-th ix00 chunk; odd ones 4 bytes off.
    let targets: Vec<u64> = (0..ENTRIES)
        .map(|e| ix_offset(e / 2) + if e % 2 == 0 { 0 } else { 4 })
        .collect();
    let hdrl = hdrl_with_super_index(&targets);
    let movi = list(b"LIST", b"movi", &chunk(b"00dc", &[]));
    let mut file = list(b"RIFF", b"AVI ", &[hdrl, movi].concat());
    assert_eq!(file.len() as u64, primary_len);
    for k in 0..SEGMENTS {
        // Every third index anchors outside any movi list.
        let base = if k % 3 == 0 { 4 } else { ix_offset(k) };
        file.extend(list(
            b"RIFF",
            b"AVIX",
            &list(b"LIST", b"movi", &empty_ix00(base)),
        ));
    }

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = open_avi(rs, &registry()).unwrap();
    assert_eq!(dmx.movi_segment_count(), SEGMENTS + 1);

    let stale = dmx.super_index_target_violations();
    assert_eq!(stale.len(), SEGMENTS);
    assert!(stale.iter().all(|v| v.entry_index % 2 == 1));

    let outside = dmx.std_index_base_offset_violations();
    assert_eq!(outside.len(), SEGMENTS.div_ceil(3));
    assert!(outside.iter().all(|v| v.segment_index % 3 == 0));

    // Nothing to read and nothing to land on, but both terminate.
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    assert!(dmx.seek_to(0, 0).is_err());
}

/// 262 144 standard-index entries whose chunk offsets all agree in
/// their low 21 bits (`dwOffset` a multiple of 2^21, `qwBaseOffset` a
/// multiple of 2^32) — a 2 MiB file. The per-chunk keyframe map is
/// keyed by those offsets; with a hash whose bucket bits came from the
/// key's low bits alone, every insert walked one ever-longer probe
/// chain. The map's hash now mixes all key bits with a per-map seed.
#[test]
fn index_offsets_agreeing_in_their_low_bits_open_in_linear_time() {
    const CHUNKS: usize = 128;
    const ENTRIES: usize = 2048;
    let mut movi = Vec::new();
    for k in 0..CHUNKS {
        let mut body = Vec::with_capacity(24 + 8 * ENTRIES);
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&[0, 1]);
        body.extend_from_slice(&(ENTRIES as u32).to_le_bytes());
        body.extend_from_slice(b"00dc");
        body.extend_from_slice(&((k as u64 + 1) << 32).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        for j in 0..ENTRIES as u32 {
            body.extend_from_slice(&dwords(&[j << 21, 1]));
        }
        movi.extend(chunk(b"ix00", &body));
    }
    let hdrl = hdrl_with_super_index(&[1]);
    let file = list(
        b"RIFF",
        b"AVI ",
        &[hdrl, list(b"LIST", b"movi", &movi)].concat(),
    );
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = open_avi(rs, &registry()).unwrap();
    assert_eq!(dmx.keyframe_indexed_packet_count(0), CHUNKS * ENTRIES);
    // The entries point nowhere: a seek is refused, not followed.
    assert!(dmx.seek_to(0, 1000).is_err());
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

/// Side-band chunks are counted and buffered once, whichever pass meets
/// them: the `idx1` scan at `open()` (primary segment), or the `movi`
/// walk (`RIFF AVIX` segments, which `idx1` cannot reach). The walk
/// used to count the `idx1`-listed chunks a second time, and again on
/// every re-read after a seek.
#[test]
fn side_band_chunks_are_recorded_once_however_often_they_are_read() {
    let frames: Vec<(usize, bool)> = (0..40).map(|i| (300 + i, i % 8 == 0)).collect();
    for kind in KINDS {
        let bytes = mux(&frames, kind, Side::PaletteAndText);
        let mut dmx = open(&bytes);
        let multi_segment = dmx.movi_segment_count() > 1;
        if !multi_segment {
            // Everything is in idx1: known before the first packet.
            assert_eq!(dmx.palette_change_count(0), 40);
            assert_eq!(dmx.text_chunk_count(0), 40);
        }
        for round in 0..3 {
            assert_eq!(drain(&mut dmx).len(), 40 - 8 * round.min(1) * 2);
            assert_eq!(dmx.palette_change_count(0), 40, "round {round}");
            assert_eq!(dmx.text_chunk_count(0), 40, "round {round}");
            assert_eq!(dmx.palette_change_data(0).len(), 40);
            assert_eq!(dmx.text_chunk_data(0).len(), 40);
            assert!(dmx.text_chunk_data(0).iter().all(|t| t == b"overlay"));
            assert_eq!(dmx.seek_to(0, 16).unwrap(), 16);
        }
    }
}

/// An `idx1` of 4 000 `00pc` entries that all name the same 256 KiB
/// region: the eager side-band load must not buffer it 4 000 times
/// (1 GiB from a 330 KiB file). Genuine side-band chunks are disjoint,
/// so the bodies kept can never add up to more than the file.
#[test]
fn overlapping_side_band_index_entries_do_not_multiply_memory() {
    const BODY: usize = 256 * 1024;
    const ENTRIES: usize = 4000;
    let hdrl = hdrl_with_super_index(&[]);
    let movi = list(b"LIST", b"movi", &chunk(b"00pc", &vec![7u8; BODY]));
    let mut idx1 = Vec::with_capacity(16 * ENTRIES);
    for _ in 0..ENTRIES {
        idx1.extend_from_slice(b"00pc");
        idx1.extend_from_slice(&dwords(&[0x100, 4, BODY as u32]));
    }
    let file = list(
        b"RIFF",
        b"AVI ",
        &[hdrl, movi, chunk(b"idx1", &idx1)].concat(),
    );
    let file_len = file.len();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = open_avi(rs, &registry()).unwrap();
    let held: usize = dmx.palette_change_data(0).iter().map(Vec::len).sum();
    assert!(
        held <= file_len,
        "{held} bytes buffered from a {file_len}-byte file"
    );
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    let held: usize = dmx.palette_change_data(0).iter().map(Vec::len).sum();
    assert!(held <= file_len);
}

/// `demux` target, 431 bytes: an uncompressed `BI_RGB` stream whose
/// `BITMAPINFOHEADER` states `biWidth` 0 and `biHeight` 671 416 320.
/// The bottom-up row unpacker ran once per stated row — of zero bytes
/// each — for every packet: 15 s to read two 48-byte chunks. A
/// picture with no columns has no row layout; the chunks pass through
/// as stored.
#[test]
fn zero_width_raw_video_with_a_huge_height_reads_at_once() {
    let bytes = include_bytes!("fixtures/fuzz/raw_dib_zero_width_rows.avi");
    let started = std::time::Instant::now();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    let mut dmx = open_avi(rs, &CodecRegistry::new()).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "rawvideo");
    assert_eq!(dmx.streams()[0].params.width, Some(0));
    let mut sizes = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        sizes.push(p.data.len());
    }
    // Two `00db` chunks; the third chunk names a stream that is not there.
    assert_eq!(sizes, [48, 48]);
    // 15 s before; milliseconds now. The bound is loose on purpose.
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}
