//! Pins for file shapes other writers produce and this crate's own
//! muxer never did — each one a mismatch the live black-box matrix
//! (`tests/blackbox_matrix.rs`) surfaced. The files are assembled by
//! hand from the AVI 1.0 structure tables.

#[path = "support/build.rs"]
mod build;
#[path = "support/claims.rs"]
mod claims;

use std::io::Cursor;

use build::{Item, Strh, Track, avi, bmih, wfx};
use oxideav_avi::demuxer::{AviDemuxer, open_avi, open_avi_strict};
use oxideav_core::{Demuxer, Error, Packet, ReadSeek};

const KEY: u32 = 0x10; // AVIIF_KEYFRAME

fn open(bytes: Vec<u8>) -> AviDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    open_avi(rs, &claims::registry()).expect("file opens")
}

fn packets(dmx: &mut AviDemuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux error: {e}"),
        }
    }
}

fn video_track(fourcc: &[u8; 4], frames: u32) -> Track {
    Track {
        strh: Strh::video(fourcc, 1, 25, frames).bytes(),
        strf: bmih(16, 16, fourcc, 24, &[]),
        extra: Vec::new(),
    }
}

/// An index that flags none of a stream's frames key says its writer
/// did not maintain the bit: every frame reads key. One flagged frame
/// is enough for the index to be taken at its word.
#[test]
fn an_index_with_no_key_frame_at_all_reads_all_key() {
    let items = |flags: [u32; 3]| -> Vec<Item<'static>> {
        flags
            .into_iter()
            .map(|f| Item {
                stream: 0,
                kind: b"dc",
                data: &[0xC5u8; 9],
                flags: Some(f),
            })
            .collect()
    };
    let mut none_key = open(avi(&[video_track(b"CVID", 3)], &items([0, 0, 0]), true));
    let keys: Vec<bool> = packets(&mut none_key)
        .iter()
        .map(|p| p.flags.keyframe)
        .collect();
    assert_eq!(keys, [true, true, true]);
    assert_eq!(none_key.packet_is_keyframe(0, 1), Some(true));
    assert_eq!(none_key.seek_to(0, 2).expect("a keyless index seeks"), 2);

    let mut one_key = open(avi(&[video_track(b"CVID", 3)], &items([KEY, 0, 0]), true));
    let keys: Vec<bool> = packets(&mut one_key)
        .iter()
        .map(|p| p.flags.keyframe)
        .collect();
    assert_eq!(keys, [true, false, false]);
    assert_eq!(one_key.seek_to(0, 2).expect("seek"), 0);
}

/// Every packet states how many `dwScale / dwRate` ticks its chunk
/// spans: one for a frame-per-chunk stream, the sample blocks it holds
/// when `dwSampleSize` is set.
#[test]
fn packets_carry_their_duration_in_stream_ticks() {
    let pcm = Track {
        strh: Strh::audio(1, 8000, 6, 4).bytes(),
        strf: wfx(1, 2, 8000, 32_000, 4, 16, &[]),
        extra: Vec::new(),
    };
    let items = [
        Item {
            stream: 0,
            kind: b"dc",
            data: &[1; 5],
            flags: Some(KEY),
        },
        Item {
            stream: 1,
            kind: b"wb",
            data: &[0; 16],
            flags: Some(KEY),
        },
        Item {
            stream: 0,
            kind: b"dc",
            data: &[2; 5],
            flags: Some(0),
        },
        Item {
            stream: 1,
            kind: b"wb",
            data: &[0; 8],
            flags: Some(KEY),
        },
    ];
    let mut dmx = open(avi(&[video_track(b"CVID", 2), pcm], &items, true));
    let got: Vec<(u32, Option<i64>, Option<i64>)> = packets(&mut dmx)
        .iter()
        .map(|p| (p.stream_index, p.pts, p.duration))
        .collect();
    assert_eq!(
        got,
        [
            (0, Some(0), Some(1)),
            (1, Some(0), Some(4)),
            (0, Some(1), Some(1)),
            (1, Some(4), Some(2)),
        ]
    );
}

/// A block-framed compressed stream written with `dwSampleSize` =
/// `nBlockAlign` ("for audio streams, this number should be the same
/// as the nBlockAlign member", AVI 1.0 §AVISTREAMHEADER) opens, and its
/// chunks span `size / dwSampleSize` ticks. Strict mode still refuses.
#[test]
fn block_framed_compressed_audio_with_a_sample_size_opens() {
    let build = || {
        let wma = Track {
            // 2 blocks of 743 bytes; one tick = one block.
            strh: Strh::audio(743, 16_000, 3, 743).bytes(),
            strf: wfx(0x0161, 2, 44_100, 16_000, 743, 16, &[0u8; 10]),
            extra: Vec::new(),
        };
        let one = vec![0x5Au8; 743];
        let two = vec![0xA5u8; 1486];
        let items = [
            Item {
                stream: 0,
                kind: b"wb",
                data: &one,
                flags: Some(KEY),
            },
            Item {
                stream: 0,
                kind: b"wb",
                data: &two,
                flags: Some(KEY),
            },
        ];
        avi(&[wma], &items, true)
    };
    let mut dmx = open(build());
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "wmav2");
    assert_eq!(dmx.audio_sample_size_mismatches(), [(0, 743)]);
    let got: Vec<(Option<i64>, Option<i64>)> = packets(&mut dmx)
        .iter()
        .map(|p| (p.pts, p.duration))
        .collect();
    assert_eq!(got, [(Some(0), Some(1)), (Some(1), Some(2))]);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(build()));
    assert!(matches!(
        open_avi_strict(rs, &claims::registry()),
        Err(Error::InvalidData(_))
    ));
}
