//! Pins for file shapes other writers produce and this crate's own
//! muxer never did — each one a mismatch the live black-box matrix
//! (`tests/blackbox_matrix.rs`) surfaced. The files are assembled by
//! hand from the AVI 1.0 structure tables.

#[path = "support/build.rs"]
mod build;
#[path = "support/claims.rs"]
mod claims;
#[path = "support/remux.rs"]
mod remux;

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

// ───────── WAVE_FORMAT_EXTENSIBLE ─────────

/// The 22-byte extension: valid bits, channel mask, SubFormat GUID of
/// the family that wraps `inner_tag`.
fn extension(valid_bits: u16, mask: u32, inner_tag: u16) -> Vec<u8> {
    let mut e = Vec::with_capacity(22);
    e.extend_from_slice(&valid_bits.to_le_bytes());
    e.extend_from_slice(&mask.to_le_bytes());
    e.extend_from_slice(&u32::from(inner_tag).to_le_bytes());
    e.extend_from_slice(&[0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
    e
}

/// An extensible header names the codec by GUID; a GUID wrapping a
/// format tag is that codec, and only the bytes after the extension are
/// codec configuration.
#[test]
fn an_extensible_header_resolves_to_the_tag_its_guid_wraps() {
    let config = [0x11, 0xB0, 0x56, 0xE5, 0x00];
    let mut ext = extension(16, 0x3F, 0x00FF);
    ext.extend_from_slice(&config);
    let aac = Track {
        strh: Strh::audio(1024, 48_000, 2, 0).bytes(),
        strf: wfx(0xFFFE, 6, 48_000, 42_625, 4, 16, &ext),
        extra: Vec::new(),
    };
    let items = [
        Item {
            stream: 0,
            kind: b"wb",
            data: &[1; 30],
            flags: Some(KEY),
        },
        Item {
            stream: 0,
            kind: b"wb",
            data: &[2; 31],
            flags: Some(KEY),
        },
    ];
    let dmx = open(avi(&[aac], &items, true));
    let p = &dmx.streams()[0].params;
    assert_eq!(p.codec_id.as_str(), "aac");
    assert_eq!(p.tag, Some(oxideav_core::CodecTag::wave_format(0x00FF)));
    assert_eq!(p.extradata, config);
    assert_eq!(dmx.stream_channel_mask(0), Some(0x3F));
    // Classified by the wrapped tag: a per-frame codec with
    // dwSampleSize 0 is no mismatch.
    assert!(dmx.audio_sample_size_mismatches().is_empty());
}

/// The wire sample is the container (`wBitsPerSample`) wide; the valid
/// bits say how much of it is signal.
#[test]
fn extensible_pcm_is_framed_by_its_container_size() {
    let pcm = Track {
        strh: Strh::audio(1, 48_000, 2, 8).bytes(),
        strf: wfx(0xFFFE, 2, 48_000, 384_000, 8, 32, &extension(24, 3, 1)),
        extra: Vec::new(),
    };
    let items = [Item {
        stream: 0,
        kind: b"wb",
        data: &[0; 16],
        flags: Some(KEY),
    }];
    let mut dmx = open(avi(&[pcm], &items, true));
    let p = dmx.streams()[0].params.clone();
    assert_eq!(p.codec_id.as_str(), "pcm_s32le");
    assert_eq!(p.sample_format, Some(oxideav_core::SampleFormat::S32));
    assert_eq!(dmx.stream_valid_bits_per_sample(0), Some(24));
    assert_eq!(packets(&mut dmx)[0].duration, Some(2));
}

// ───────── dwStart, lengths, cut-short files ─────────

/// `dwStart` delays a stream: its first packet is stamped with it, and
/// seeks take and return the same offset timestamps.
#[test]
fn a_stream_start_offsets_its_timestamps() {
    let mut late = Strh::video(b"CVID", 1, 25, 3);
    late.start = 10;
    let track = Track {
        strh: late.bytes(),
        strf: bmih(16, 16, b"CVID", 24, &[]),
        extra: Vec::new(),
    };
    let items: Vec<Item<'_>> = (0..3)
        .map(|_| Item {
            stream: 0,
            kind: b"dc",
            data: &[7; 6],
            flags: Some(KEY),
        })
        .collect();
    let mut dmx = open(avi(&[track], &items, true));
    assert_eq!(dmx.streams()[0].start_time, Some(10));
    assert_eq!(dmx.streams()[0].duration, Some(3));
    let pts: Vec<_> = packets(&mut dmx).iter().map(|p| p.pts).collect();
    assert_eq!(pts, [Some(10), Some(11), Some(12)]);
    assert_eq!(dmx.seek_to(0, 11).expect("seek"), 11);
    assert_eq!(dmx.next_packet().expect("packet after seek").pts, Some(11));
    assert_eq!(dmx.seek_to(0, 0).expect("seek before the start"), 10);
}

/// A length the file has no room for (the placeholder of a writer that
/// could not seek back) reads as unknown, not as a billion frames.
#[test]
fn a_length_past_the_end_of_the_file_is_unknown() {
    let track = Track {
        strh: Strh::video(b"CVID", 1, 25, 0x4000_0000).bytes(),
        strf: bmih(16, 16, b"CVID", 24, &[]),
        extra: Vec::new(),
    };
    let items = [Item {
        stream: 0,
        kind: b"dc",
        data: &[7; 6],
        flags: None,
    }];
    let dmx = open(avi(&[track], &items, false));
    assert_eq!(dmx.streams()[0].duration, None);
}

/// A file cut inside its last chunk: what is there of the chunk comes
/// out, flagged corrupt, and then the stream ends.
#[test]
fn a_chunk_cut_short_is_delivered_flagged_corrupt() {
    let items = [
        Item {
            stream: 0,
            kind: b"dc",
            data: &[1; 40],
            flags: None,
        },
        Item {
            stream: 0,
            kind: b"dc",
            data: &[2; 40],
            flags: None,
        },
    ];
    let mut bytes = avi(&[video_track(b"CVID", 2)], &items, false);
    bytes.truncate(bytes.len() - 25);
    let mut dmx = open(bytes);
    let got = packets(&mut dmx);
    assert_eq!(got.len(), 2);
    assert!(!got[0].flags.corrupt);
    assert_eq!(got[1].data, [2; 15]);
    assert!(got[1].flags.corrupt);
}

// ───────── remux: header chunks go back as they came ─────────

#[derive(Clone, Default)]
struct Sink(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

fn remuxed(bytes: Vec<u8>, kind: oxideav_avi::muxer::AviKind) -> Vec<u8> {
    let sink = Sink::default();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let ws: Box<dyn oxideav_core::WriteSeek> = Box::new(sink.clone());
    remux::remux(rs, ws, kind, Default::default()).expect("remux");
    sink.0.lock().unwrap().get_ref().clone()
}

/// Body of the `n`-th chunk named `id`.
fn nth_chunk<'a>(file: &'a [u8], id: &[u8; 4], n: usize) -> &'a [u8] {
    let at = file
        .windows(4)
        .enumerate()
        .filter(|(_, w)| w == id)
        .map(|(i, _)| i)
        .nth(n)
        .unwrap_or_else(|| panic!("chunk {id:?} #{n} present"));
    let size = u32::from_le_bytes(file[at + 4..at + 8].try_into().unwrap()) as usize;
    &file[at + 8..at + 8 + size]
}

/// A demuxed stream written back keeps its `strf` byte for byte — the
/// FourCC as spelled, `nBlockAlign`, `wBitsPerSample`, the codec's own
/// header fields — and the `strh` fields a packet walk cannot rebuild,
/// plus `vprp` and `strn`. An empty audio chunk (a gap of one frame
/// time) is written back, so every packet keeps its timestamp.
#[test]
fn a_remux_writes_the_recorded_headers_back() {
    let mut vprp = vec![0u8; 68];
    vprp[20..24].copy_from_slice(&0x0010_0009u32.to_le_bytes()); // 16:9
    let mut video_strh = Strh::video(b"dvsd", 1, 25, 2).bytes();
    video_strh[14..16].copy_from_slice(&0x040Cu16.to_le_bytes()); // wLanguage
    let video = Track {
        strh: video_strh,
        strf: bmih(16, 16, b"dvsd", 24, &[]),
        extra: [
            build::chunk(b"vprp", &vprp),
            build::chunk(b"strn", b"Camera A\0"),
        ]
        .concat(),
    };
    // MPEG Layer III: nBlockAlign 1152, the 12 MPEGLAYER3WAVEFORMAT bytes.
    let mp3_fields = [1, 0, 2, 0, 0, 0, 0x80, 4, 1, 0, 0x71, 5];
    let audio = Track {
        strh: Strh::audio(1152, 44_100, 3, 0).bytes(),
        strf: wfx(0x0055, 1, 44_100, 8000, 1152, 0, &mp3_fields),
        extra: Vec::new(),
    };
    let frame = |stream, kind, data, flags| Item {
        stream,
        kind,
        data,
        flags: Some(flags),
    };
    let items = [
        frame(0, b"dc", &[1u8; 12][..], KEY),
        frame(1, b"wb", &[9u8; 209][..], KEY),
        frame(1, b"wb", &[][..], KEY), // a gap
        frame(1, b"wb", &[8u8; 209][..], KEY),
        frame(0, b"dc", &[2u8; 12][..], 0),
    ];
    let original = avi(&[video, audio], &items, true);
    for kind in [
        oxideav_avi::muxer::AviKind::Avi10,
        oxideav_avi::muxer::AviKind::OpenDml(oxideav_avi::muxer::RiffSegmentLimit::OneGiB),
    ] {
        let out = remuxed(original.clone(), kind);
        for n in 0..2 {
            assert_eq!(
                nth_chunk(&out, b"strf", n),
                nth_chunk(&original, b"strf", n)
            );
            let (a, b) = (
                nth_chunk(&original, b"strh", n),
                nth_chunk(&out, b"strh", n),
            );
            // All but dwLength (recounted) and dwSuggestedBufferSize.
            assert_eq!(a[..32], b[..32]);
            assert_eq!(a[40..], b[40..]);
        }
        assert_eq!(nth_chunk(&out, b"vprp", 0), &vprp[..]);
        assert_eq!(nth_chunk(&out, b"strn", 0), b"Camera A\0");
        let (mut a, mut b) = (open(original.clone()), open(out));
        let line = |p: &Packet| claims::packet_line(p);
        let (pa, pb): (Vec<_>, Vec<_>) = (
            packets(&mut a).iter().map(line).collect(),
            packets(&mut b).iter().map(line).collect(),
        );
        assert_eq!(pa.len(), 4);
        for s in 0..2u32 {
            let of = |v: &[String]| -> Vec<String> {
                v.iter()
                    .filter(|l| l.starts_with(&format!("P {s} ")))
                    .cloned()
                    .collect()
            };
            assert_eq!(of(&pa), of(&pb), "stream {s}");
        }
    }
}

/// Recorded headers that no longer describe the stream — here a
/// picture size changed by the caller — are not written; the header is
/// built from the parameters.
#[test]
fn recorded_headers_are_dropped_when_the_stream_changed() {
    let items = [Item {
        stream: 0,
        kind: b"dc",
        data: &[1; 12],
        flags: Some(KEY),
    }];
    let dmx = open(avi(&[video_track(b"cvid", 1)], &items, true));
    let mut stream = dmx.streams()[0].clone();
    assert!(stream.params.options.get("avi.strf").is_some());
    stream.params.width = Some(32);
    let sink = Sink::default();
    let ws: Box<dyn oxideav_core::WriteSeek> = Box::new(sink.clone());
    let mut mux = oxideav_avi::muxer::open(ws, &[stream]).expect("mux opens");
    mux.write_header().expect("header");
    mux.write_trailer().expect("trailer");
    let out = sink.0.lock().unwrap().get_ref().clone();
    let strf = nth_chunk(&out, b"strf", 0);
    assert_eq!(strf[4..8], 32u32.to_le_bytes());
    assert_eq!(&strf[16..20], b"CVID");
}

/// `set_metadata` takes what the demuxer's `metadata()` reports: the
/// `LIST INFO` text of a file survives a remux through the trait.
#[test]
fn text_metadata_crosses_a_remux() {
    let info = build::list(
        b"LIST",
        b"INFO",
        &[
            build::chunk(b"INAM", b"A title\0"),
            build::chunk(b"IART", b"Somebody\0"),
            build::chunk(b"IKEY", b"k1; k2\0"),
        ]
        .concat(),
    );
    let items = [Item {
        stream: 0,
        kind: b"dc",
        data: &[1; 12],
        flags: Some(KEY),
    }];
    // Splice the INFO list in front of `movi`.
    let mut file = avi(&[video_track(b"CVID", 1)], &items, true);
    let movi = file.windows(4).position(|w| w == b"movi").unwrap() - 8;
    file.splice(movi..movi, info);
    let riff = (file.len() - 8) as u32;
    file[4..8].copy_from_slice(&riff.to_le_bytes());

    let want = [
        ("title", "A title"),
        ("artist", "Somebody"),
        ("avi:info.IKEY", "k1; k2"),
    ];
    let has = |d: &AviDemuxer| {
        want.iter()
            .all(|(k, v)| d.metadata().iter().any(|(mk, mv)| mk == k && mv == v))
    };
    assert!(has(&open(file.clone())));
    let out = remuxed(file, oxideav_avi::muxer::AviKind::Avi10);
    assert!(has(&open(out)));
}

// ───────── headers built from the parameters alone ─────────

/// A compressed stream whose parameters name a sample format (a
/// decoder's output layout) is not PCM: it keeps its tag and its
/// configuration bytes, counts one tick per chunk, and states the
/// largest chunk as `nBlockAlign`. 64-bit integer PCM is `pcm_s64le`.
#[test]
fn a_compressed_stream_with_a_sample_format_is_not_written_as_pcm() {
    use oxideav_core::{CodecId, CodecParameters, CodecTag, SampleFormat, StreamInfo, TimeBase};
    let mut params = CodecParameters::audio(CodecId::new("aac"));
    params.tag = Some(CodecTag::wave_format(0x00FF));
    params.channels = Some(2);
    params.sample_rate = Some(44_100);
    params.sample_format = Some(SampleFormat::S16);
    params.extradata = vec![0x12, 0x10];
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 44_100),
        duration: None,
        start_time: Some(0),
        params,
    };
    let sink = Sink::default();
    let ws: Box<dyn oxideav_core::WriteSeek> = Box::new(sink.clone());
    let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).expect("mux opens");
    mux.write_header().expect("header");
    for (n, len) in [300usize, 412, 7].into_iter().enumerate() {
        let mut p = Packet::new(0, stream.time_base, vec![0xD0; len]);
        p.pts = Some(n as i64 * 1024);
        p.duration = Some(1024);
        p.flags.keyframe = true;
        mux.write_packet(&p).expect("packet");
    }
    mux.write_trailer().expect("trailer");
    let out = sink.0.lock().unwrap().get_ref().clone();

    let strf = nth_chunk(&out, b"strf", 0);
    assert_eq!(strf[0..2], 0x00FFu16.to_le_bytes(), "wFormatTag");
    assert_eq!(
        strf[12..14],
        412u16.to_le_bytes(),
        "nBlockAlign = largest chunk"
    );
    assert_eq!(strf[18..], [0x12, 0x10], "codec configuration");
    let strh = nth_chunk(&out, b"strh", 0);
    assert_eq!(strh[44..48], [0; 4], "dwSampleSize 0: one frame per chunk");
    assert_eq!(strh[32..36], 3u32.to_le_bytes(), "dwLength counts frames");

    let mut dmx = open(out);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "aac");
    let pts: Vec<_> = packets(&mut dmx).iter().map(|p| p.pts).collect();
    assert_eq!(pts, [Some(0), Some(1), Some(2)]);

    let wide = Track {
        strh: Strh::audio(1, 8000, 2, 16).bytes(),
        strf: wfx(1, 2, 8000, 128_000, 16, 64, &[]),
        extra: Vec::new(),
    };
    let items = [Item {
        stream: 0,
        kind: b"wb",
        data: &[0; 32],
        flags: Some(KEY),
    }];
    let dmx = open(avi(&[wide], &items, true));
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "pcm_s64le");
}

/// An 8-bit indexed DIB is `rawvideo` / `Pal8`: packed top-down index
/// rows, with the color table as `extradata`. Written back from the
/// parameters alone it is an indexed DIB again, table and all.
#[test]
fn an_eight_bit_indexed_dib_is_pal8_rawvideo() {
    use oxideav_core::PixelFormat;
    // 3 x 2, bottom-up, rows padded to 4 bytes; a 2-entry table.
    let table = [10, 20, 30, 0, 40, 50, 60, 0];
    let mut strf = bmih(3, 2, &[0; 4], 8, &table);
    strf[32..36].copy_from_slice(&2u32.to_le_bytes()); // biClrUsed
    let track = Track {
        strh: Strh::video(&[0; 4], 1, 25, 1).bytes(),
        strf,
        extra: Vec::new(),
    };
    let wire = [1, 1, 0, 0xEE, /* top row last */ 0, 1, 0, 0xEE];
    let items = [Item {
        stream: 0,
        kind: b"db",
        data: &wire,
        flags: Some(KEY),
    }];
    let mut dmx = open(avi(&[track], &items, true));
    let mut stream = dmx.streams()[0].clone();
    assert_eq!(stream.params.codec_id.as_str(), "rawvideo");
    assert_eq!(stream.params.pixel_format, Some(PixelFormat::Pal8));
    assert_eq!(stream.params.extradata, table);
    let got = packets(&mut dmx);
    assert_eq!(got[0].data, [0, 1, 0, 1, 1, 0]);

    // Without the recorded headers: built from the parameters.
    stream.params.options = oxideav_core::CodecOptions::new();
    let sink = Sink::default();
    let ws: Box<dyn oxideav_core::WriteSeek> = Box::new(sink.clone());
    let mut mux = oxideav_avi::muxer::open(ws, &[stream]).expect("mux opens");
    mux.write_header().expect("header");
    mux.write_packet(&got[0]).expect("packet");
    mux.write_trailer().expect("trailer");
    let out = sink.0.lock().unwrap().get_ref().clone();
    let mut again = open(out);
    assert_eq!(
        again.streams()[0].params.pixel_format,
        Some(PixelFormat::Pal8)
    );
    assert_eq!(again.streams()[0].params.extradata, table);
    assert_eq!(packets(&mut again)[0].data, got[0].data);
}
