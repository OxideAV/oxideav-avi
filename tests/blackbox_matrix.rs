//! Live black-box matrix: files written by an outside muxer, read by
//! this crate and by an outside reader; then this crate's own output,
//! read by the outside reader.
//!
//! Needs `ffmpeg` and `ffprobe` on `PATH` — the test returns early
//! (passes vacuously, saying so) when either is missing. Every file is
//! generated into a temporary directory; nothing is committed.
//!
//! **Foreign → ours.** Each generated file is checked three ways:
//!
//! * against a model of the file built here from the AVI 1.0 / OpenDML
//!   structure tables alone (a second, independent reading of the
//!   bytes): stream tag, time base, start, length, configuration bytes,
//!   and per packet its payload CRC, timestamp, duration and key flag;
//! * against the outside reader's packet listing: same packets, same
//!   payload bytes (when that reader re-frames a stream — it splits PCM
//!   chunks — the concatenated stream bytes are compared instead);
//! * stream parameters against the outside reader's.
//!
//! Where the two references disagree the model wins and the case is
//! counted in the summary line: on frame-per-chunk audio the outside
//! reader does not advance the clock over an empty chunk and divides
//! chunks by `nBlockAlign`; it reports a stream length of its own
//! estimate rather than `dwLength`; and it flags some delta frames key.
//!
//! **Ours → foreign.** Every generated file is remuxed through this
//! crate in each writer mode (AVI 1.0, OpenDML, OpenDML rolled into
//! many `RIFF AVIX` segments, OpenDML with the compact in-`strl`
//! index); the outside reader must report the same streams and the
//! same packets for the remux as for the original, with nothing new on
//! its error output, and decode both to the same frames.

#[path = "support/claims.rs"]
mod claims;
#[path = "support/remux.rs"]
mod remux;

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};
use oxideav_core::{Demuxer, Error, ReadSeek, WriteSeek};

// ───────────────────────── tool plumbing ─────────────────────────

fn tool_present(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// One `key=value` record of the prober's compact listing.
type Record = HashMap<String, String>;

struct Probe {
    streams: Vec<Record>,
    packets: Vec<Record>,
    format: Record,
    stderr: String,
    ok: bool,
}

fn probe(path: &Path) -> Probe {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-show_packets",
        ])
        .args(["-show_data_hash", "crc32", "-of", "compact=nk=0:p=1"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    let mut p = Probe {
        streams: Vec::new(),
        packets: Vec::new(),
        format: Record::new(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ok: out.status.success(),
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('|');
        let kind = fields.next().unwrap_or_default();
        let rec: Record = fields
            .filter_map(|f| f.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        match kind {
            "stream" => p.streams.push(rec),
            "packet" => p.packets.push(rec),
            "format" => p.format = rec,
            _ => {}
        }
    }
    p
}

fn field<'a>(rec: &'a Record, key: &str) -> &'a str {
    rec.get(key).map_or("", String::as_str)
}

/// Addresses in the outside tool's log lines differ run to run.
fn without_addresses(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("0x") {
        out.push_str(&rest[..at + 2]);
        rest = rest[at + 2..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
    }
    out.push_str(rest);
    out
}

/// MD5 of everything the outside decoder makes of the file, and its
/// error output.
fn decoded(path: &Path) -> (String, String) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0", "-f", "hash", "-hash", "md5", "-"])
        .output()
        .expect("ffmpeg runs");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        without_addresses(String::from_utf8_lossy(&out.stderr).trim()),
    )
}

/// CRC of one stream's payload bytes as the outside reader frames them.
fn foreign_stream_crc(path: &Path, stream: usize) -> u32 {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            &format!("0:{stream}"),
            "-c",
            "copy",
            "-f",
            "data",
            "-",
        ])
        .output()
        .expect("ffmpeg runs");
    claims::crc32(&out.stdout)
}

// ───────────────────────── the file model ─────────────────────────

#[derive(Default, Clone)]
struct MStream {
    fcc_type: [u8; 4],
    scale: u32,
    rate: u32,
    start: u32,
    length: u32,
    sample_size: u32,
    strf: Vec<u8>,
}

struct MChunk {
    stream: usize,
    kind: [u8; 2],
    size: usize,
    /// Offset of the chunk header.
    at: usize,
}

struct MPacket {
    dts: i64,
    ticks: i64,
    size: usize,
    key: bool,
    at: usize,
}

/// The file as the structure tables of AVI 1.0 (`avih` / `strl` /
/// `movi` / `idx1`) and OpenDML (`RIFF AVIX`, `indx` / `ix##`) lay it
/// out, read independently of the crate under test.
#[derive(Default)]
struct Model {
    streams: Vec<MStream>,
    chunks: Vec<MChunk>,
    idx1_raw: Vec<([u8; 4], u32, u32)>,
    movi_starts: Vec<usize>,
    movi_bytes: u64,
    key_by_offset: HashMap<usize, bool>,
}

fn le32(d: &[u8], at: usize) -> u32 {
    d.get(at..at + 4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le16(d: &[u8], at: usize) -> u16 {
    d.get(at..at + 2)
        .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

impl Model {
    fn read(d: &[u8]) -> Self {
        let mut m = Self::default();
        let mut pos = 0;
        while pos + 12 <= d.len() && &d[pos..pos + 4] == b"RIFF" {
            let size = le32(d, pos + 4) as usize;
            let mut end = d.len().min(pos + 8 + size);
            if end <= pos + 12 {
                end = d.len();
            }
            m.walk(d, pos + 12, end);
            pos += 8 + size + (size & 1);
        }
        // `idx1` offsets are from the `movi` FourCC, or — "in some AVI
        // files" — from the start of the file: whichever lands the
        // first entry on its own chunk id.
        if let (Some(first), Some(&movi)) = (m.idx1_raw.first(), m.movi_starts.first()) {
            let relative = movi - 4;
            let hit = |base: usize| {
                d.get(base + first.2 as usize..)
                    .is_some_and(|c| c.starts_with(&first.0))
            };
            let base = if hit(relative) { relative } else { 0 };
            for (_, flags, off) in &m.idx1_raw {
                m.key_by_offset
                    .insert(base + *off as usize, flags & 0x10 != 0);
            }
        }
        m
    }

    fn walk(&mut self, d: &[u8], mut pos: usize, end: usize) {
        while pos + 8 <= end {
            let id = [d[pos], d[pos + 1], d[pos + 2], d[pos + 3]];
            let mut size = le32(d, pos + 4) as usize;
            let body = pos + 8;
            if &id == b"LIST" {
                let form = d.get(body..body + 4).unwrap_or_default();
                let mut list_end = end.min(body + size);
                if form == b"movi" {
                    // A writer that could not seek back leaves the size 0.
                    if size == 0 {
                        list_end = end;
                    }
                    self.movi_starts.push(body + 4);
                    self.movi_bytes += (list_end.saturating_sub(body + 4)) as u64;
                }
                self.walk(d, body + 4, list_end);
                if size == 0 && form == b"movi" {
                    return;
                }
            } else {
                // Cut short: what is left of the last chunk.
                size = size.min(d.len() - body);
                let b = &d[body..body + size];
                match &id {
                    b"strh" if b.len() >= 48 => self.streams.push(MStream {
                        fcc_type: [b[0], b[1], b[2], b[3]],
                        scale: le32(b, 20),
                        rate: le32(b, 24),
                        start: le32(b, 28),
                        length: le32(b, 32),
                        sample_size: le32(b, 44),
                        strf: Vec::new(),
                    }),
                    b"strf" => {
                        if let Some(s) = self.streams.last_mut() {
                            s.strf = b.to_vec();
                        }
                    }
                    b"idx1" => {
                        for e in b.as_chunks::<16>().0 {
                            self.idx1_raw
                                .push(([e[0], e[1], e[2], e[3]], le32(e, 4), le32(e, 8)));
                        }
                    }
                    _ if (&id[..2] == b"ix" || (&id == b"indx" && b.get(3) == Some(&1)))
                        && b.len() >= 24 =>
                    {
                        // AVISTDINDEX: entries of `wLongsPerEntry` DWORDs,
                        // offset from `qwBaseOffset` to the chunk data,
                        // size with the high bit set on a delta frame.
                        let stride = 4 * le16(b, 0) as usize;
                        let base = u64::from(le32(b, 12)) | u64::from(le32(b, 16)) << 32;
                        for k in 0..le32(b, 4) as usize {
                            let e = 24 + stride * k;
                            if stride == 0 || e + 8 > b.len() {
                                break;
                            }
                            let at = base as usize + le32(b, e) as usize - 8;
                            self.key_by_offset
                                .entry(at)
                                .or_insert(le32(b, e + 4) & 0x8000_0000 == 0);
                        }
                    }
                    _ if id[0].is_ascii_digit() && id[1].is_ascii_digit() => {
                        self.chunks.push(MChunk {
                            stream: usize::from(id[0] - b'0') * 10 + usize::from(id[1] - b'0'),
                            kind: [id[2], id[3]],
                            size,
                            at: pos,
                        });
                    }
                    _ => {}
                }
            }
            pos = body + size + (size & 1);
        }
    }

    /// Bytes per tick of a stream: `dwSampleSize` when set, the
    /// `nBlockAlign` of PCM-family audio whose writer left it 0, else
    /// one tick per chunk (`None`).
    fn divisor(&self, i: usize) -> Option<usize> {
        let s = &self.streams[i];
        if &s.fcc_type != b"auds" {
            return None;
        }
        if s.sample_size != 0 {
            return Some(s.sample_size as usize);
        }
        matches!(le16(&s.strf, 0), 1 | 3 | 6 | 7).then(|| usize::from(le16(&s.strf, 12)).max(1))
    }

    /// The packets of stream `i`, and whether it holds an empty chunk
    /// (a time slot with nothing in it).
    fn packets(&self, i: usize) -> (Vec<MPacket>, bool) {
        let s = &self.streams[i];
        let divisor = self.divisor(i);
        let mine = |c: &&MChunk| {
            c.stream == i && &c.kind != b"pc" && (&c.kind != b"tx" || &s.fcc_type == b"txts")
        };
        let mjpg = s
            .strf
            .get(16..20)
            .is_some_and(|f| f.eq_ignore_ascii_case(b"MJPG"));
        // An index that flags no frame of the stream key was not
        // maintaining the flag.
        let any_key = self
            .chunks
            .iter()
            .filter(mine)
            .any(|c| c.size != 0 && self.key_by_offset.get(&c.at) == Some(&true));
        let mut out = Vec::new();
        let (mut clock, mut gaps) = (i64::from(s.start), false);
        for c in self.chunks.iter().filter(mine) {
            let ticks = divisor.map_or(1, |d| (c.size / d) as i64);
            if c.size == 0 {
                gaps = true;
            } else {
                out.push(MPacket {
                    dts: clock,
                    ticks,
                    size: c.size,
                    key: mjpg || !any_key || self.key_by_offset.get(&c.at).copied().unwrap_or(true),
                    at: c.at,
                });
            }
            clock += ticks;
        }
        (out, gaps)
    }

    /// `(tag, configuration bytes)` the stream header states.
    fn tag_and_config(&self, i: usize) -> (u32, Vec<u8>) {
        let s = &self.streams[i];
        match &s.fcc_type {
            b"vids" => (
                le32(&s.strf, 16),
                s.strf.get(40..).unwrap_or_default().to_vec(),
            ),
            b"auds" => {
                let cb = usize::from(le16(&s.strf, 16));
                let ext = s
                    .strf
                    .get(18..(18 + cb).min(s.strf.len()))
                    .unwrap_or_default();
                let tag = le16(&s.strf, 0);
                // WAVE_FORMAT_EXTENSIBLE: a SubFormat GUID of the
                // `xxxxxxxx-0000-0010-8000-00aa00389b71` family wraps a
                // format tag; the 22-byte extension is not codec data.
                const FAMILY: [u8; 14] =
                    [0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71];
                if tag == 0xFFFE && ext.len() >= 22 {
                    let inner = if ext[8..22] == FAMILY {
                        le16(ext, 6)
                    } else {
                        tag
                    };
                    (u32::from(inner), ext[22..].to_vec())
                } else {
                    (u32::from(tag), ext.to_vec())
                }
            }
            _ => (0, s.strf.clone()),
        }
    }
}

/// Packed top-down rows of an uncompressed DIB frame: rows are padded
/// to a DWORD stride and stored bottom-up unless `biHeight` is negative
/// (VfW `BITMAPINFOHEADER`, "biHeight sign rules" / "Calculating
/// surface stride").
fn dib_rows(strf: &[u8], wire: &[u8]) -> Vec<u8> {
    let width = le32(strf, 4) as i32;
    let height = le32(strf, 8) as i32;
    if strf[16..20].eq_ignore_ascii_case(b"RGBA") {
        return wire.to_vec();
    }
    let row = width.unsigned_abs() as usize * usize::from(le16(strf, 14) / 8);
    let stride = (row + 3) & !3;
    let rows = height.unsigned_abs() as usize;
    if rows == 0 || wire.len() < stride * (rows - 1) + row || (height < 0 && stride == row) {
        return wire.to_vec();
    }
    let mut out = Vec::with_capacity(row * rows);
    for r in 0..rows {
        let src = if height > 0 { rows - 1 - r } else { r };
        out.extend_from_slice(&wire[src * stride..src * stride + row]);
    }
    out
}

// ───────────────────────── this crate's reading ─────────────────────────

struct Ours {
    streams: Vec<oxideav_core::StreamInfo>,
    packets: Vec<oxideav_core::Packet>,
}

fn ours(bytes: &[u8]) -> Result<Ours, String> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    let mut dmx = oxideav_avi::demuxer::open_avi(rs, &claims::registry())
        .map_err(|e| format!("open: {e}"))?;
    let streams = dmx.streams().to_vec();
    let mut packets = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => packets.push(p),
            Err(Error::Eof) => break,
            Err(e) => return Err(format!("next_packet: {e}")),
        }
    }
    Ok(Ours { streams, packets })
}

fn tag_number(tag: &Option<oxideav_core::CodecTag>) -> Option<u32> {
    match tag {
        Some(oxideav_core::CodecTag::Fourcc(f)) => Some(u32::from_le_bytes(*f)),
        Some(oxideav_core::CodecTag::WaveFormat(t)) => Some(u32::from(*t)),
        _ => None,
    }
}

fn upper(tag: u32) -> [u8; 4] {
    tag.to_le_bytes().map(|b| b.to_ascii_uppercase())
}

/// Tallies of the cases where the outside reader and the file model
/// part ways (the model is followed).
#[derive(Default)]
struct Notes {
    foreign_unreadable: usize,
    foreign_reframes: usize,
    foreign_clock: usize,
    foreign_length: usize,
    neither_opens: usize,
}

/// Foreign → ours for one file. Returns the mismatches.
fn check_foreign(path: &Path, bytes: &[u8], notes: &Mutex<Notes>) -> Vec<String> {
    let mut bad = Vec::new();
    let reference = probe(path);
    let mine = match ours(bytes) {
        Ok(o) => o,
        Err(e) => {
            if reference.ok {
                bad.push(format!("ours fails ({e}), the outside reader does not"));
            } else {
                notes.lock().unwrap().neither_opens += 1;
            }
            return bad;
        }
    };
    let model = Model::read(bytes);
    if model.streams.len() != mine.streams.len() {
        bad.push(format!(
            "streams: ours {} / file {}",
            mine.streams.len(),
            model.streams.len()
        ));
        return bad;
    }
    let reference_reads = reference.streams.len() == mine.streams.len();
    if !reference_reads {
        if reference.streams.is_empty() {
            notes.lock().unwrap().foreign_unreadable += 1;
        } else {
            bad.push(format!(
                "streams: ours {} / outside reader {}",
                mine.streams.len(),
                reference.streams.len()
            ));
            return bad;
        }
    }
    for (i, st) in mine.streams.iter().enumerate() {
        let ms = &model.streams[i];
        let p = &st.params;
        let (tag, config) = model.tag_and_config(i);
        if &ms.fcc_type != b"txts" && tag_number(&p.tag).map(upper) != Some(upper(tag)) {
            bad.push(format!("s{i} tag {:?} / file {tag:#x}", p.tag));
        }
        if p.extradata != config {
            bad.push(format!(
                "s{i} extradata {} bytes / file {}",
                p.extradata.len(),
                config.len()
            ));
        }
        let tb = st.time_base.0;
        if ms.scale != 0
            && ms.rate != 0
            && tb.num * i64::from(ms.rate) != tb.den * i64::from(ms.scale)
        {
            bad.push(format!("s{i} time base {}/{}", tb.num, tb.den));
        }
        if st.start_time != Some(i64::from(ms.start)) {
            bad.push(format!(
                "s{i} start {:?} / dwStart {}",
                st.start_time, ms.start
            ));
        }
        // `dwLength`, or unknown when the file has no room for it.
        let per_tick = if &ms.fcc_type == b"auds" && ms.sample_size != 0 {
            u64::from(ms.sample_size)
        } else {
            8
        };
        let want = if ms.length == 0 || u64::from(ms.length) * per_tick > model.movi_bytes {
            None
        } else {
            Some(i64::from(ms.length))
        };
        if st.duration != want {
            bad.push(format!("s{i} duration {:?} / {want:?}", st.duration));
        }
        let (expect, gaps) = model.packets(i);
        let got: Vec<_> = mine
            .packets
            .iter()
            .filter(|k| k.stream_index as usize == i)
            .collect();
        let raw = p.codec_id.as_str() == "rawvideo";
        if got.len() != expect.len() {
            bad.push(format!(
                "s{i} packets {} / file {}",
                got.len(),
                expect.len()
            ));
            continue;
        }
        let mut wrong = 0;
        for (n, (g, e)) in got.iter().zip(&expect).enumerate() {
            let wire = &bytes[e.at + 8..e.at + 8 + e.size];
            let payload_ok = if raw {
                g.data == dib_rows(&ms.strf, wire)
            } else {
                g.data == wire
            };
            let ok = payload_ok
                && g.dts == Some(e.dts)
                && g.pts == Some(e.dts)
                && g.duration == (e.ticks > 0).then_some(e.ticks)
                && g.flags.keyframe == e.key;
            if !ok {
                wrong += 1;
                if wrong == 1 {
                    bad.push(format!(
                        "s{i} packet {n}: ours dts {:?} dur {:?} key {} len {} / file dts {} ticks {} key {} len {} payload-equal {payload_ok}",
                        g.dts, g.duration, g.flags.keyframe, g.data.len(), e.dts, e.ticks, e.key, e.size
                    ));
                }
            }
        }
        if wrong > 1 {
            bad.push(format!(
                "s{i}: {wrong} of {} packets differ from the file",
                got.len()
            ));
        }
        if !reference_reads {
            continue;
        }
        // The outside reader's view.
        let rs = &reference.streams[i];
        let kind = match p.media_type {
            oxideav_core::MediaType::Video => "video",
            oxideav_core::MediaType::Audio => "audio",
            oxideav_core::MediaType::Subtitle => "subtitle",
            _ => "data",
        };
        if field(rs, "codec_type") != kind {
            bad.push(format!(
                "s{i} is {kind} / outside reader {}",
                field(rs, "codec_type")
            ));
        }
        if kind == "video"
            && (field(rs, "width") != p.width.unwrap_or(0).to_string()
                || field(rs, "height") != p.height.unwrap_or(0).to_string())
        {
            bad.push(format!("s{i} picture size {:?}x{:?}", p.width, p.height));
        }
        if kind == "audio"
            && (field(rs, "sample_rate") != p.sample_rate.unwrap_or(0).to_string()
                || field(rs, "channels") != p.channels.unwrap_or(0).to_string())
        {
            bad.push(format!(
                "s{i} audio shape {:?} Hz {:?} ch",
                p.sample_rate, p.channels
            ));
        }
        if field(rs, "duration_ts") != ms.length.to_string() {
            notes.lock().unwrap().foreign_length += 1;
        }
        let theirs: Vec<&Record> = reference
            .packets
            .iter()
            .filter(|r| field(r, "stream_index") == i.to_string())
            .collect();
        if raw {
            // The outside reader hands out the on-wire DIB; the packed
            // rows were checked against the file above.
            if theirs.len() != got.len() {
                bad.push(format!(
                    "s{i} packets {} / outside reader {}",
                    got.len(),
                    theirs.len()
                ));
            }
            continue;
        }
        if theirs.len() != got.len() {
            // It re-frames (sample-sized audio split into smaller
            // packets): the stream bytes must still be the same.
            let all: Vec<u8> = got.iter().flat_map(|g| g.data.iter().copied()).collect();
            if claims::crc32(&all) != foreign_stream_crc(path, i) {
                bad.push(format!(
                    "s{i}: {} packets / outside reader {}, and the stream bytes differ",
                    got.len(),
                    theirs.len()
                ));
            }
            notes.lock().unwrap().foreign_reframes += 1;
            continue;
        }
        let mut clock_differs = false;
        for (n, (g, t)) in got.iter().zip(&theirs).enumerate() {
            let crc = format!("CRC32:{:08x}", claims::crc32(&g.data));
            if field(t, "size") != g.data.len().to_string() || field(t, "data_hash") != crc {
                bad.push(format!(
                    "s{i} packet {n}: payload differs from the outside reader's"
                ));
                break;
            }
            clock_differs |= field(t, "dts") != g.dts.unwrap_or(-1).to_string();
        }
        if clock_differs {
            // On a frame-per-chunk audio stream the outside reader does
            // not count an empty chunk as a tick, and counts a chunk as
            // `size / nBlockAlign` ticks when `nBlockAlign` is not 1 —
            // against `dwSampleSize` 0, "each sample of data must be in
            // a separate chunk" (and against the `dwLength` it reports
            // for the same stream). The model is followed there.
            let frame_per_chunk_audio = kind == "audio" && ms.sample_size == 0;
            if gaps || frame_per_chunk_audio {
                notes.lock().unwrap().foreign_clock += 1;
            } else {
                bad.push(format!("s{i}: timestamps differ from the outside reader's"));
            }
        }
    }
    bad
}

// ───────────────────────── ours → foreign ─────────────────────────

const STREAM_FIELDS: &[&str] = &[
    "codec_name",
    "codec_type",
    "codec_tag",
    "width",
    "height",
    "pix_fmt",
    "sample_rate",
    "channels",
    "channel_layout",
    "bits_per_sample",
    "time_base",
    "start_pts",
    "extradata_size",
    "extradata_hash",
    "sample_aspect_ratio",
    "display_aspect_ratio",
    "field_order",
    "r_frame_rate",
];
const PACKET_FIELDS: &[&str] = &["dts", "duration", "size", "flags", "data_hash"];

fn modes() -> Vec<(&'static str, AviKind, AviMuxOptions)> {
    vec![
        ("avi10", AviKind::Avi10, AviMuxOptions::default()),
        (
            "opendml",
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::default(),
        ),
        (
            "opendml-rolled",
            AviKind::OpenDml(RiffSegmentLimit::Bytes(20_000)),
            AviMuxOptions::default(),
        ),
        (
            "opendml-compact",
            AviKind::OpenDml(RiffSegmentLimit::OneGiB),
            AviMuxOptions::default().with_strl_std_index(1 << 16),
        ),
    ]
}

#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<Mutex<Cursor<Vec<u8>>>>);

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

/// Ours → foreign for one original, in every writer mode. Returns
/// `(checks run, mismatches)`.
fn check_remux(path: &Path, bytes: &[u8], deep: bool) -> (usize, Vec<String>) {
    let mut bad = Vec::new();
    let original = probe(path);
    let original_model = Model::read(bytes);
    let Ok(original_ours) = ours(bytes) else {
        return (0, bad);
    };
    let original_decoded = deep.then(|| decoded(path));
    let mut runs = 0;
    for (name, kind, options) in modes() {
        runs += 1;
        let sink = SharedBuf::default();
        let ws: Box<dyn WriteSeek> = Box::new(sink.clone());
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
        if let Err(e) = remux::remux(rs, ws, kind, options) {
            bad.push(format!("{name}: remux failed: {e}"));
            continue;
        }
        let out = sink.0.lock().unwrap().get_ref().clone();
        let out_path = path.with_extension(format!("{name}.avi"));
        std::fs::write(&out_path, &out).expect("temp file writes");
        let again = probe(&out_path);
        let model = Model::read(&out);
        if !again.stderr.is_empty()
            && without_addresses(&again.stderr) != without_addresses(&original.stderr)
        {
            bad.push(format!(
                "{name}: outside reader complains: {}",
                again.stderr
            ));
        }
        if again.streams.len() != original.streams.len() {
            bad.push(format!(
                "{name}: {} streams / original {}",
                again.streams.len(),
                original.streams.len()
            ));
            continue;
        }
        for (i, (a, b)) in original.streams.iter().zip(&again.streams).enumerate() {
            for key in STREAM_FIELDS {
                if field(a, key) != field(b, key) {
                    bad.push(format!(
                        "{name}: s{i} {key} {} / original {}",
                        field(b, key),
                        field(a, key)
                    ));
                }
            }
            // The stream length is `dwLength`, which the writer counts
            // from what it wrote. The outside reader echoes it for
            // frame-per-chunk streams; compare where the original's
            // figure was the header's own.
            let stated = original_model.streams[i].length.to_string();
            if field(a, "duration_ts") == stated
                && model.streams[i].length == original_model.streams[i].length
                && field(b, "duration_ts") != stated
            {
                bad.push(format!(
                    "{name}: s{i} duration_ts {}",
                    field(b, "duration_ts")
                ));
            }
            let tags = |r: &Record| -> Vec<(String, String)> {
                let mut t: Vec<_> = r
                    .iter()
                    .filter(|(k, _)| k.starts_with("tag:"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                t.sort();
                t
            };
            if tags(a) != tags(b) {
                bad.push(format!(
                    "{name}: s{i} tags {:?} / original {:?}",
                    tags(b),
                    tags(a)
                ));
            }
        }
        let text = |r: &Record| -> Vec<(String, String)> {
            let mut t: Vec<_> = r
                .iter()
                .filter(|(k, _)| k.starts_with("tag:"))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                .collect();
            t.sort();
            t
        };
        if text(&original.format) != text(&again.format) {
            bad.push(format!(
                "{name}: file tags {:?} / original {:?}",
                text(&again.format),
                text(&original.format)
            ));
        }
        if original.packets.len() != again.packets.len() {
            bad.push(format!(
                "{name}: {} packets / original {}",
                again.packets.len(),
                original.packets.len()
            ));
        } else {
            // Interleave may differ; compare stream by stream.
            for i in 0..original.streams.len() {
                let of = |p: &Probe| -> Vec<Vec<String>> {
                    p.packets
                        .iter()
                        .filter(|r| field(r, "stream_index") == i.to_string())
                        .map(|r| {
                            PACKET_FIELDS
                                .iter()
                                .map(|k| field(r, k).to_string())
                                .collect()
                        })
                        .collect()
                };
                let (a, b) = (of(&original), of(&again));
                if let Some(n) = (0..a.len().max(b.len())).find(|&n| a.get(n) != b.get(n)) {
                    bad.push(format!(
                        "{name}: s{i} packet {n} {:?} / original {:?}",
                        b.get(n),
                        a.get(n)
                    ));
                }
            }
        }
        // This crate reads its own output as it read the original.
        match ours(&out) {
            Ok(mine) => {
                let lines = |o: &Ours, m: &Model| -> Vec<String> {
                    let mut l: Vec<String> = o
                        .streams
                        .iter()
                        .map(|s| {
                            let mut s = s.clone();
                            // A frame-per-chunk stream's length is the
                            // count of chunks written.
                            if m.streams[s.index as usize].sample_size == 0 {
                                s.duration = None;
                            }
                            claims::stream_line(&s)
                        })
                        .collect();
                    l.extend(o.packets.iter().map(claims::packet_line));
                    l
                };
                let (a, b) = (lines(&original_ours, &original_model), lines(&mine, &model));
                if a != b {
                    let at = (0..a.len().max(b.len())).find(|&n| a.get(n) != b.get(n));
                    bad.push(format!(
                        "{name}: own reading differs at line {at:?}: {:?} / original {:?}",
                        at.and_then(|n| b.get(n)),
                        at.and_then(|n| a.get(n))
                    ));
                }
            }
            Err(e) => bad.push(format!("{name}: own output does not read back: {e}")),
        }
        if let Some((frames, complaints)) = &original_decoded
            && matches!(name, "avi10" | "opendml-rolled")
        {
            let (f, c) = decoded(&out_path);
            if &f != frames {
                bad.push(format!("{name}: decodes to different frames"));
            }
            if !c.is_empty() && &c != complaints {
                bad.push(format!("{name}: outside decoder complains: {c}"));
            }
        }
        let _ = std::fs::remove_file(&out_path);
    }
    (runs, bad)
}

// ───────────────────────── the matrix ─────────────────────────

struct Case {
    name: String,
    /// Arguments between `ffmpeg -y` and the output.
    args: Vec<String>,
    /// Write through a pipe (the muxer cannot seek back).
    piped: bool,
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn video_source(size: &str, rate: &str, seconds: &str) -> String {
    format!("-f lavfi -i testsrc2=s={size}:r={rate}:d={seconds}")
}

fn audio_source(rate: u32, seconds: &str) -> String {
    format!("-f lavfi -i sine=f=440:r={rate}:d={seconds}")
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    let mut add = |name: String, args: String| {
        out.push(Case {
            name,
            args: words(&args),
            piped: false,
        });
    };
    let v = |enc: &str, size: &str, rate: &str, secs: &str| {
        format!("{} {enc}", video_source(size, rate, secs))
    };
    // Video codecs, one stream each.
    for (name, enc, size, rate, secs) in [
        ("mpeg4", "-c:v mpeg4", "64x48", "25", "1"),
        ("mpeg4-b", "-c:v mpeg4 -bf 2", "64x48", "25", "1"),
        ("mpeg4-b-gop", "-c:v mpeg4 -bf 2 -g 12", "64x48", "25", "4"),
        ("mpeg4-xvid", "-c:v mpeg4 -vtag XVID", "64x48", "25", "1"),
        ("mpeg4-divx", "-c:v mpeg4 -vtag DIVX", "64x48", "25", "1"),
        ("mpeg4-dx50", "-c:v mpeg4 -vtag DX50", "64x48", "25", "1"),
        ("mpeg4-lower", "-c:v mpeg4 -vtag xvid", "64x48", "25", "1"),
        (
            "mpeg4-qpel",
            "-c:v mpeg4 -flags +qpel+mv4 -bf 1",
            "64x48",
            "25",
            "1",
        ),
        ("mpeg4-1frame", "-c:v mpeg4 -frames:v 1", "64x48", "25", "1"),
        ("msmpeg4v2", "-c:v msmpeg4v2", "64x48", "25", "1"),
        ("msmpeg4v3", "-c:v msmpeg4", "64x48", "25", "1"),
        (
            "msmpeg4v3-mp43",
            "-c:v msmpeg4 -vtag MP43",
            "64x48",
            "25",
            "1",
        ),
        ("wmv1", "-c:v wmv1", "64x48", "25", "1"),
        ("wmv2", "-c:v wmv2", "64x48", "25", "1"),
        ("h264", "-c:v libx264 -pix_fmt yuv420p", "64x48", "25", "1"),
        (
            "h264-nob",
            "-c:v libx264 -pix_fmt yuv420p -bf 0",
            "64x48",
            "25",
            "1",
        ),
        (
            "h264-b3",
            "-c:v libx264 -pix_fmt yuv420p -bf 3 -g 10",
            "64x48",
            "25",
            "2",
        ),
        (
            "h264-444",
            "-c:v libx264 -pix_fmt yuv444p",
            "64x48",
            "25",
            "1",
        ),
        (
            "h264-odd",
            "-c:v libx264 -pix_fmt yuv420p",
            "66x50",
            "25",
            "1",
        ),
        ("mjpeg", "-c:v mjpeg", "64x48", "25", "1"),
        (
            "mjpeg-422",
            "-c:v mjpeg -pix_fmt yuvj422p",
            "64x48",
            "25",
            "1",
        ),
        (
            "mjpeg-444",
            "-c:v mjpeg -pix_fmt yuvj444p",
            "64x48",
            "25",
            "1",
        ),
        ("mjpeg-odd", "-c:v mjpeg", "67x51", "25", "1"),
        ("mjpeg-720p", "-c:v mjpeg -q:v 2", "1280x720", "25", "0.4"),
        ("ljpeg", "-c:v ljpeg -pix_fmt yuvj420p", "64x48", "25", "1"),
        ("mpeg1", "-c:v mpeg1video", "64x48", "25", "1"),
        ("mpeg1-b", "-c:v mpeg1video -bf 2", "64x48", "25", "1"),
        ("mpeg2", "-c:v mpeg2video", "64x48", "25", "1"),
        ("mpeg2-b", "-c:v mpeg2video -bf 2", "64x48", "25", "1"),
        (
            "mpeg2-interlaced",
            "-c:v mpeg2video -flags +ilme+ildct",
            "720x576",
            "25",
            "0.4",
        ),
        ("h261", "-c:v h261", "176x144", "25", "1"),
        ("h263", "-c:v h263", "176x144", "25", "1"),
        ("h263p", "-c:v h263p", "64x48", "25", "1"),
        ("flv1", "-c:v flv", "64x48", "25", "1"),
        (
            "dv-ntsc",
            "-c:v dvvideo -pix_fmt yuv411p",
            "720x480",
            "30000/1001",
            "0.3",
        ),
        (
            "dv-pal",
            "-c:v dvvideo -pix_fmt yuv420p",
            "720x576",
            "25",
            "0.3",
        ),
        (
            "dv50",
            "-c:v dvvideo -pix_fmt yuv422p",
            "720x576",
            "25",
            "0.3",
        ),
        ("cinepak", "-c:v cinepak", "64x48", "25", "0.4"),
        (
            "msvideo1",
            "-c:v msvideo1 -pix_fmt rgb555le",
            "64x48",
            "25",
            "1",
        ),
        ("msrle", "-c:v msrle -pix_fmt pal8", "64x48", "25", "1"),
        ("zmbv", "-c:v zmbv -pix_fmt pal8", "64x48", "25", "1"),
        ("zlib", "-c:v zlib -pix_fmt bgr24", "64x48", "25", "1"),
        ("flashsv", "-c:v flashsv", "64x48", "25", "1"),
        ("snow", "-c:v snow -strict -2", "64x48", "25", "1"),
        ("svq1", "-c:v svq1", "64x48", "25", "1"),
        ("asv1", "-c:v asv1", "64x48", "25", "1"),
        ("asv2", "-c:v asv2", "64x48", "25", "1"),
        ("cljr", "-c:v cljr", "64x48", "25", "1"),
        ("huffyuv", "-c:v huffyuv", "64x48", "25", "1"),
        (
            "huffyuv-rgb",
            "-c:v huffyuv -pix_fmt bgr0",
            "64x48",
            "25",
            "1",
        ),
        (
            "huffyuv-bgra",
            "-c:v huffyuv -pix_fmt bgra",
            "64x48",
            "25",
            "1",
        ),
        ("ffvhuff", "-c:v ffvhuff", "64x48", "25", "1"),
        (
            "ffvhuff-420",
            "-c:v ffvhuff -pix_fmt yuv420p",
            "64x48",
            "25",
            "1",
        ),
        ("png", "-c:v png", "64x48", "25", "1"),
        ("jpeg2000", "-c:v jpeg2000", "64x48", "25", "1"),
        ("jpegls", "-c:v jpegls", "64x48", "25", "1"),
        (
            "prores",
            "-c:v prores -pix_fmt yuv422p10le",
            "128x96",
            "25",
            "1",
        ),
        (
            "prores-4444",
            "-c:v prores_ks -profile:v 4 -pix_fmt yuva444p10le",
            "128x96",
            "25",
            "1",
        ),
        (
            "cfhd",
            "-c:v cfhd -pix_fmt yuv422p10le",
            "128x96",
            "25",
            "1",
        ),
        (
            "vc2",
            "-c:v vc2 -strict -2 -pix_fmt yuv422p10le",
            "128x96",
            "25",
            "1",
        ),
        ("vp8", "-c:v libvpx -b:v 200k", "64x48", "25", "1"),
        ("vp9", "-c:v libvpx-vp9 -b:v 200k", "64x48", "25", "1"),
        ("speedhq", "-c:v speedhq", "64x48", "25", "1"),
        ("amv", "-c:v amv", "64x48", "25", "1"),
        ("r210", "-c:v r210", "64x48", "25", "1"),
        ("r10k", "-c:v r10k", "64x48", "25", "1"),
        ("v210", "-c:v v210", "64x48", "25", "1"),
        ("y41p", "-c:v y41p", "64x48", "25", "1"),
        ("yuv4", "-c:v yuv4", "64x48", "25", "1"),
        ("avrp", "-c:v avrp", "64x48", "25", "1"),
    ] {
        add(format!("v-{name}"), v(enc, size, rate, secs));
    }
    for pf in [
        "yuv420p",
        "yuv422p",
        "yuv444p",
        "gbrp",
        "gbrap",
        "yuv420p10le",
        "yuv422p10le",
    ] {
        add(
            format!("v-utvideo-{pf}"),
            v(&format!("-c:v utvideo -pix_fmt {pf}"), "64x48", "25", "1"),
        );
    }
    for pf in [
        "yuv420p", "yuv422p", "yuv444p", "gbrp", "gbrap", "gray", "yuva444p",
    ] {
        add(
            format!("v-magicyuv-{pf}"),
            v(&format!("-c:v magicyuv -pix_fmt {pf}"), "64x48", "25", "1"),
        );
    }
    for level in ["1", "3"] {
        for pf in ["yuv420p", "yuv444p10le", "bgra", "gray"] {
            add(
                format!("v-ffv1-l{level}-{pf}"),
                v(
                    &format!("-c:v ffv1 -level {level} -pix_fmt {pf}"),
                    "64x48",
                    "25",
                    "1",
                ),
            );
        }
    }
    // Uncompressed video: DIB depths, orientations, raw FourCCs, sizes
    // whose rows need padding.
    for pf in [
        "bgr24", "bgra", "bgr0", "rgb555le", "rgb565le", "pal8", "gray", "monow", "monob", "bgr8",
        "yuv420p", "yuyv422", "uyvy422", "nv12", "nv21", "yuv422p", "yuv444p", "yuv410p",
        "yuv411p", "yvyu422", "rgba", "rgb444le",
    ] {
        add(
            format!("raw-{pf}"),
            v(
                &format!("-c:v rawvideo -pix_fmt {pf}"),
                "64x48",
                "25",
                "0.2",
            ),
        );
    }
    for pf in ["bgr24", "bgra", "rgb555le", "pal8", "monow", "gray"] {
        add(
            format!("raw-bottom-up-{pf}"),
            v(
                &format!("-c:v rawvideo -pix_fmt {pf} -flipped_raw_rgb 1"),
                "64x48",
                "25",
                "0.2",
            ),
        );
    }
    for size in ["2x4", "64x46", "66x2", "30x30"] {
        for pf in ["bgr24", "yuv420p", "pal8", "rgb555le", "monow"] {
            add(
                format!("raw-{pf}-{size}"),
                v(&format!("-c:v rawvideo -pix_fmt {pf}"), size, "25", "0.2"),
            );
            if pf == "bgr24" {
                add(
                    format!("raw-bottom-up-{pf}-{size}"),
                    v(
                        &format!("-c:v rawvideo -pix_fmt {pf} -flipped_raw_rgb 1"),
                        size,
                        "25",
                        "0.2",
                    ),
                );
            }
        }
    }
    // Palette changes: a new palette per frame.
    let per_frame_palette = "-filter_complex [0:v]split[a][b];[a]palettegen=stats_mode=single[p];[b][p]paletteuse=new=1";
    for (name, enc) in [
        ("raw-pal8", "-c:v rawvideo -pix_fmt pal8"),
        ("msrle", "-c:v msrle -pix_fmt pal8"),
    ] {
        add(
            format!("palette-changes-{name}"),
            format!(
                "{} {per_frame_palette} {enc}",
                video_source("64x48", "10", "1")
            ),
        );
    }
    // Frame rates.
    for (rate, secs) in [
        ("30000/1001", "1"),
        ("24000/1001", "1"),
        ("1", "3"),
        ("12.5", "1"),
        ("60", "1"),
        ("1000", "0.2"),
        ("15/2", "1"),
    ] {
        add(
            format!("v-mjpeg-rate-{}", rate.replace('/', "_")),
            v("-c:v mjpeg", "64x48", rate, secs),
        );
    }
    // Dropped frames (holes in the timestamps, written as empty chunks).
    for (name, select, enc) in [
        ("mjpeg", "not(mod(n\\,3))", "-c:v mjpeg"),
        ("mpeg4", "gt(mod(n\\,7)\\,2)", "-c:v mpeg4"),
    ] {
        add(
            format!("v-dropped-{name}"),
            format!(
                "{} -vf select={select} -fps_mode passthrough {enc}",
                video_source("64x48", "25", "2")
            ),
        );
    }
    // Audio codecs, one stream each.
    let a = |enc: &str, rate: u32, secs: &str| format!("{} {enc}", audio_source(rate, secs));
    for codec in [
        "pcm_u8",
        "pcm_s16le",
        "pcm_s24le",
        "pcm_s32le",
        "pcm_f32le",
        "pcm_f64le",
        "pcm_s64le",
        "pcm_alaw",
        "pcm_mulaw",
    ] {
        for channels in [1, 2, 6, 8] {
            add(
                format!("a-{codec}-{channels}ch"),
                a(&format!("-c:a {codec} -ac {channels}"), 44_100, "1"),
            );
        }
        add(
            format!("a-{codec}-6ch-nomask"),
            a(
                &format!("-c:a {codec} -ac 6 -write_channel_mask 0"),
                44_100,
                "1",
            ),
        );
    }
    for rate in [8000, 11_025, 22_050, 32_000, 48_000, 96_000] {
        add(
            format!("a-pcm_s16le-{rate}"),
            a("-c:a pcm_s16le -ac 2", rate, "1"),
        );
    }
    for (codec, rate, channels) in [
        ("adpcm_ms", 44_100, 1),
        ("adpcm_ms", 44_100, 2),
        ("adpcm_ms", 8000, 1),
        ("adpcm_ima_wav", 44_100, 1),
        ("adpcm_ima_wav", 44_100, 2),
        ("adpcm_ima_wav", 22_050, 2),
        ("adpcm_yamaha", 44_100, 1),
        ("adpcm_yamaha", 44_100, 2),
        ("adpcm_swf", 44_100, 2),
        ("g722", 16_000, 1),
        ("g726", 8000, 1),
        ("g723_1", 8000, 1),
    ] {
        add(
            format!("a-{codec}-{rate}-{channels}ch"),
            a(&format!("-c:a {codec} -ac {channels}"), rate, "1"),
        );
    }
    for (name, enc, rate) in [
        ("mp3-cbr64", "-c:a libmp3lame -b:a 64k -ac 2", 44_100),
        ("mp3-cbr128", "-c:a libmp3lame -b:a 128k -ac 2", 44_100),
        ("mp3-cbr320", "-c:a libmp3lame -b:a 320k -ac 2", 44_100),
        ("mp3-vbr0", "-c:a libmp3lame -q:a 0 -ac 2", 44_100),
        ("mp3-vbr4", "-c:a libmp3lame -q:a 4 -ac 2", 44_100),
        ("mp3-vbr9", "-c:a libmp3lame -q:a 9 -ac 2", 44_100),
        ("mp3-abr", "-c:a libmp3lame -abr 1 -b:a 96k -ac 2", 44_100),
        ("mp3-8k", "-c:a libmp3lame -ac 1", 8000),
        ("mp3-11k", "-c:a libmp3lame -ac 1", 11_025),
        ("mp3-16k", "-c:a libmp3lame -ac 1", 16_000),
        ("mp3-22k", "-c:a libmp3lame -ac 1", 22_050),
        ("mp3-24k", "-c:a libmp3lame -ac 1", 24_000),
        ("mp3-32k", "-c:a libmp3lame -ac 1", 32_000),
        ("mp3-48k", "-c:a libmp3lame -ac 1", 48_000),
        ("mp2", "-c:a mp2 -ac 2", 44_100),
        ("mp2-mono-16k", "-c:a mp2 -ac 1 -b:a 32k", 16_000),
        ("mp2-48k", "-c:a mp2 -ac 2", 48_000),
        ("ac3", "-c:a ac3 -ac 2", 48_000),
        ("ac3-5.1", "-c:a ac3 -ac 6", 48_000),
        ("ac3-44k", "-c:a ac3 -ac 2", 44_100),
        ("eac3", "-c:a eac3 -ac 2", 48_000),
        ("aac", "-c:a aac -ac 2", 44_100),
        ("aac-mono-8k", "-c:a aac -ac 1", 8000),
        ("aac-5.1", "-c:a aac -ac 6", 48_000),
        ("dts", "-c:a dca -strict -2 -ac 2", 48_000),
        ("flac", "-c:a flac -ac 2", 44_100),
        ("wmav1", "-c:a wmav1 -ac 2", 44_100),
        ("wmav2", "-c:a wmav2 -ac 2", 44_100),
    ] {
        add(format!("a-{name}"), a(enc, rate, "2"));
    }
    add("a-pcm-long".into(), a("-c:a pcm_s16le -ac 2", 44_100, "30"));
    add(
        "a-pcm-1-sample".into(),
        a("-c:a pcm_s16le -ac 2 -frames:a 1", 44_100, "1"),
    );
    // Audio + video, interleaved; header options; several tracks.
    let av =
        |venc: &str, aenc: &str, size: &str, vrate: &str, arate: u32, secs: &str, extra: &str| {
            format!(
                "{} {} {venc} {aenc} {extra}",
                video_source(size, vrate, secs),
                audio_source(arate, secs)
            )
        };
    for (name, venc, aenc, size, vrate, arate, secs, extra) in [
        (
            "mpeg4-mp3",
            "-c:v mpeg4",
            "-c:a libmp3lame",
            "64x48",
            "25",
            44_100,
            "1",
            "",
        ),
        (
            "mpeg4-b-mp3-vbr",
            "-c:v mpeg4 -bf 2",
            "-c:a libmp3lame -q:a 5",
            "64x48",
            "25",
            44_100,
            "3",
            "",
        ),
        (
            "h264-aac",
            "-c:v libx264 -pix_fmt yuv420p",
            "-c:a aac",
            "64x48",
            "25",
            44_100,
            "2",
            "",
        ),
        (
            "h264-ac3",
            "-c:v libx264 -pix_fmt yuv420p",
            "-c:a ac3",
            "64x48",
            "25",
            48_000,
            "2",
            "",
        ),
        (
            "mjpeg-pcm",
            "-c:v mjpeg",
            "-c:a pcm_s16le -ac 2",
            "64x48",
            "25",
            44_100,
            "2",
            "",
        ),
        (
            "mjpeg-pcm-u8-8k",
            "-c:v mjpeg",
            "-c:a pcm_u8 -ac 1",
            "64x48",
            "25",
            8000,
            "2",
            "",
        ),
        (
            "dv-pcm",
            "-c:v dvvideo -pix_fmt yuv420p",
            "-c:a pcm_s16le -ac 2",
            "720x576",
            "25",
            48_000,
            "0.4",
            "",
        ),
        (
            "raw-pcm24",
            "-c:v rawvideo -pix_fmt bgr24",
            "-c:a pcm_s24le -ac 2",
            "64x48",
            "25",
            44_100,
            "1",
            "",
        ),
        (
            "mpeg2-mp2",
            "-c:v mpeg2video -bf 2",
            "-c:a mp2",
            "64x48",
            "25",
            44_100,
            "2",
            "",
        ),
        (
            "msmpeg4-adpcm",
            "-c:v msmpeg4",
            "-c:a adpcm_ms",
            "64x48",
            "25",
            44_100,
            "2",
            "",
        ),
        (
            "xvid-ima",
            "-c:v mpeg4 -vtag XVID",
            "-c:a adpcm_ima_wav",
            "64x48",
            "25",
            44_100,
            "2",
            "",
        ),
        (
            "cinepak-alaw",
            "-c:v cinepak",
            "-c:a pcm_alaw -ac 1",
            "64x48",
            "25",
            8000,
            "0.4",
            "",
        ),
        (
            "ntsc-48k",
            "-c:v mpeg4",
            "-c:a libmp3lame",
            "64x48",
            "30000/1001",
            48_000,
            "2",
            "",
        ),
        (
            "aspect-16-9",
            "-c:v mpeg4",
            "-c:a libmp3lame",
            "64x48",
            "25",
            44_100,
            "1",
            "-aspect 16:9",
        ),
        (
            "index-in-header",
            "-c:v mjpeg",
            "-c:a pcm_s16le -ac 2",
            "64x48",
            "25",
            44_100,
            "2",
            "-reserve_index_space 4096",
        ),
        (
            "index-in-header-small",
            "-c:v mjpeg",
            "-c:a pcm_s16le -ac 2",
            "64x48",
            "25",
            44_100,
            "20",
            "-reserve_index_space 256",
        ),
        (
            "index-in-header-mp3",
            "-c:v mpeg4 -bf 2",
            "-c:a libmp3lame",
            "64x48",
            "25",
            44_100,
            "2",
            "-reserve_index_space 8192",
        ),
        (
            "interleave-tight",
            "-c:v mjpeg",
            "-c:a pcm_s16le -ac 2",
            "64x48",
            "25",
            44_100,
            "3",
            "-max_interleave_delta 1",
        ),
        (
            "long",
            "-c:v mpeg4 -bf 2 -g 50",
            "-c:a libmp3lame -q:a 4",
            "64x48",
            "25",
            44_100,
            "60",
            "",
        ),
        (
            "text",
            "-c:v mjpeg",
            "-c:a pcm_s16le -ac 2",
            "64x48",
            "25",
            44_100,
            "1",
            "-metadata title=TheTitle -metadata artist=Somebody -metadata comment=Hello -metadata date=2026 -metadata genre=Test -metadata copyright=CC -metadata album=Alb -metadata track=3 -metadata:s:v:0 title=VideoName -metadata:s:a:0 title=AudioName",
        ),
    ] {
        add(
            format!("av-{name}"),
            av(venc, aenc, size, vrate, arate, secs, extra),
        );
    }
    let two = video_source("64x48", "25", "2");
    add(
        "av-2-audio".into(),
        format!(
            "{two} {} -f lavfi -i sine=f=880:r=48000:d=2 -map 0 -map 1 -map 2 -c:v mjpeg -c:a:0 libmp3lame -c:a:1 pcm_s16le",
            audio_source(44_100, "2")
        ),
    );
    add(
        "av-3-audio".into(),
        format!(
            "{two} {} -f lavfi -i sine=f=880:r=48000:d=2 -f lavfi -i sine=f=220:r=8000:d=2 -map 0 -map 1 -map 2 -map 3 -c:v mpeg4 -c:a:0 ac3 -c:a:1 aac -c:a:2 pcm_mulaw",
            audio_source(44_100, "2")
        ),
    );
    add(
        "av-2-video".into(),
        format!(
            "{} -f lavfi -i testsrc=s=32x32:r=10:d=1 -map 0 -map 1 -c:v:0 mjpeg -c:v:1 mpeg4",
            video_source("64x48", "25", "1")
        ),
    );
    add(
        "a-2-audio".into(),
        format!(
            "{} -f lavfi -i sine=f=880:r=48000:d=2 -map 0 -map 1 -c:a:0 pcm_s16le -c:a:1 libmp3lame",
            audio_source(44_100, "2")
        ),
    );
    add(
        "av-audio-first".into(),
        format!(
            "{} {} -map 0 -map 1 -c:v mjpeg -c:a pcm_s16le",
            audio_source(44_100, "1"),
            video_source("64x48", "25", "1")
        ),
    );
    add(
        "av-audio-late".into(),
        format!(
            "{two} -itsoffset 0.5 {} -c:v mjpeg -c:a libmp3lame",
            audio_source(44_100, "1")
        ),
    );
    add(
        "av-dropped-pcm".into(),
        format!(
            "{two} {} -vf select=not(mod(n\\,4)) -fps_mode passthrough -c:v mjpeg -c:a pcm_s16le",
            audio_source(44_100, "2")
        ),
    );
    // Written through a pipe: no index, sizes and lengths never patched.
    for (name, args) in [
        (
            "mpeg4-mp3",
            av(
                "-c:v mpeg4 -bf 2",
                "-c:a libmp3lame",
                "64x48",
                "25",
                44_100,
                "2",
                "",
            ),
        ),
        (
            "mjpeg-pcm",
            av(
                "-c:v mjpeg",
                "-c:a pcm_s16le",
                "64x48",
                "25",
                44_100,
                "2",
                "",
            ),
        ),
        (
            "h264",
            v("-c:v libx264 -pix_fmt yuv420p", "64x48", "25", "2"),
        ),
        ("ac3", a("-c:a ac3", 44_100, "2")),
    ] {
        out.push(Case {
            name: format!("piped-{name}"),
            args: words(&args),
            piped: true,
        });
    }
    out
}

/// Files derived from a generated one by damaging it: cut short at
/// several points, the index removed or disguised, the index rewritten
/// with file-absolute offsets.
fn damaged(name: &str, d: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let Some(idx1) = d.windows(4).rposition(|w| w == b"idx1") else {
        return out;
    };
    for percent in [40, 50, 77, 93] {
        out.push((
            format!("{name}-cut-{percent}"),
            d[..d.len() * percent / 100].to_vec(),
        ));
    }
    out.push((
        format!("{name}-cut-in-header"),
        d[..d.len().min(3000)].to_vec(),
    ));
    out.push((format!("{name}-cut-in-index"), d[..idx1 + 9].to_vec()));
    out.push((format!("{name}-index-cut-off"), d[..idx1].to_vec()));
    let mut junked = d.to_vec();
    junked[idx1..idx1 + 4].copy_from_slice(b"JUNK");
    out.push((format!("{name}-index-disguised"), junked));
    if let Some(movi) = d.windows(4).position(|w| w == b"movi") {
        let mut absolute = d.to_vec();
        let entries = le32(d, idx1 + 4) as usize / 16;
        for k in 0..entries {
            let at = idx1 + 8 + 16 * k + 8;
            if at + 4 <= absolute.len() {
                let v = le32(d, at) + movi as u32;
                absolute[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        out.push((format!("{name}-index-absolute"), absolute));
    }
    out
}

fn generate(case: &Case, dir: &Path) -> Option<PathBuf> {
    let path = dir.join(format!("{}.avi", case.name));
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(&case.args)
        .args(["-f", "avi"]);
    let ok = if case.piped {
        let out = cmd.arg("-").output().ok()?;
        out.status.success() && std::fs::write(&path, &out.stdout).is_ok()
    } else {
        cmd.arg(&path).output().ok()?.status.success()
    };
    (ok && std::fs::metadata(&path).is_ok_and(|m| m.len() > 0)).then_some(path)
}

#[test]
fn foreign_files_and_our_remuxes_agree_with_the_outside_reader() {
    if !tool_present("ffmpeg") || !tool_present("ffprobe") {
        eprintln!("skip: ffmpeg / ffprobe not on PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("oxideav-avi-matrix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let cases = cases();
    let next = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let notes: Mutex<Notes> = Mutex::new(Notes::default());
    // (files read, remuxes compared, cases the tool could not write)
    let counts: Mutex<(usize, usize, usize)> = Mutex::new((0, 0, 0));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = cases.get(n) else { break };
                    let Some(path) = generate(case, &dir) else {
                        counts.lock().unwrap().2 += 1;
                        continue;
                    };
                    let bytes = std::fs::read(&path).expect("generated file reads");
                    let mut bad = check_foreign(&path, &bytes, &notes);
                    let mut files = 1;
                    let mut remuxes = 0;
                    if !case.piped {
                        // The decode comparison costs two more decoder
                        // runs per mode; every third case gets it.
                        let (runs, more) = check_remux(&path, &bytes, n.is_multiple_of(3));
                        remuxes += runs;
                        bad.extend(more);
                    }
                    if matches!(case.name.as_str(), "av-mpeg4-b-mp3-vbr" | "av-mjpeg-pcm") {
                        for (name, data) in damaged(&case.name, &bytes) {
                            let p = dir.join(format!("{name}.avi"));
                            std::fs::write(&p, &data).expect("temp file writes");
                            files += 1;
                            for m in check_foreign(&p, &data, &notes) {
                                failures.lock().unwrap().push(format!("{name}: {m}"));
                            }
                            let _ = std::fs::remove_file(&p);
                        }
                    }
                    let _ = std::fs::remove_file(&path);
                    let mut c = counts.lock().unwrap();
                    c.0 += files;
                    c.1 += remuxes;
                    drop(c);
                    let mut f = failures.lock().unwrap();
                    f.extend(bad.into_iter().map(|m| format!("{}: {m}", case.name)));
                }
            });
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
    let (files, remuxes, unwritable) = *counts.lock().unwrap();
    let notes = notes.into_inner().unwrap();
    let failures = failures.into_inner().unwrap();
    eprintln!(
        "black-box matrix: {files} foreign files read, {remuxes} remuxes compared, \
         {unwritable} cases the outside muxer could not write; \
         {} streams the outside reader re-frames, {} where its clock skips empty chunks, \
         {} where its length is not dwLength, {} files it cannot read back, {} neither reads; \
         {} mismatches",
        notes.foreign_reframes,
        notes.foreign_clock,
        notes.foreign_length,
        notes.foreign_unreadable,
        notes.neither_opens,
        failures.len()
    );
    assert!(files > 0, "the outside muxer wrote nothing");
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
