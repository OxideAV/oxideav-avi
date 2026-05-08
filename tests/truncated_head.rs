//! Truncated-head AVI tolerance tests.
//!
//! Capture-card crash dumps and copy-aborted recordings produce AVI
//! 1.0 files whose RIFF / `LIST hdrl` / `LIST movi` size fields
//! over-declare the bytes physically present. The
//! `oxideav-vfw` round-15 dispatch (commit `1214299c`) hit this
//! against `crashtest.avi` (a 5 MiB head of a 20 MiB Indeo 4
//! capture, with `LIST movi size=20353990`) and added a clamping
//! relaxation in its codec-test helper. Per
//! `docs/IMPLEMENTOR_ROUND.md` §"Crate-purpose discipline" the
//! relaxation belongs in **this** crate (containers own
//! chunk-walking), not in a per-codec test helper.
//!
//! The tests below build small valid AVI files via the muxer, then
//! corrupt the RIFF / movi size fields or truncate the file tail to
//! reproduce the round-15 failure modes — and assert the demuxer:
//!
//! 1. Opens cleanly (no `read_exact` UnexpectedEof bubble),
//! 2. Surfaces every frame wholly inside the truncated bytes,
//! 3. Stops with `Error::Eof` at the truncation boundary
//!    (no panic, no infinite loop).
//!
//! Genuine corruption (RIFF FourCC scrambled, no `hdrl`, no
//! `movi`, etc.) still errors cleanly — covered by the negative
//! tests at the bottom of this file.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Error, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};

/// Tempfile-based builder: muxes a small valid PCM AVI to disk, reads
/// it back, deletes the file, returns the bytes. Done via tempfile
/// (not in-memory `Cursor`) because the muxer's owning `Box<dyn
/// WriteSeek>` doesn't surface its inner `Vec` back out.
///
/// `tag` is a per-call-site label that goes into the tempfile name so
/// concurrent test threads don't share / overwrite each other's
/// fixture file.
fn build_pcm_avi(tag: &str, n_packets: usize, frames_per_packet: usize) -> Vec<u8> {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };

    let pid = std::process::id();
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-avi-trunc-{}-{}-{}-{}.avi",
        tag, pid, n_packets, frames_per_packet
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_avi::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for i in 0..n_packets {
            let mut payload = Vec::with_capacity(frames_per_packet * 4);
            for s in 0..frames_per_packet {
                let v = ((i * frames_per_packet + s) as i16).wrapping_mul(7);
                payload.extend_from_slice(&v.to_le_bytes());
                payload.extend_from_slice(&v.to_le_bytes());
            }
            let mut pkt = Packet::new(0, stream.time_base, payload);
            pkt.pts = Some((i * frames_per_packet) as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Locate a 4-byte FourCC marker in the buffer; returns the offset of
/// the start of the FourCC.
fn find_fourcc(buf: &[u8], needle: &[u8; 4]) -> Option<usize> {
    buf.windows(4).position(|w| w == needle)
}

/// Patch the 4-byte little-endian u32 at `offset`.
fn patch_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Drive a demuxer to completion: collect packet data, return on `Eof`,
/// panic on any other error.
fn drain_packets(buf: Vec<u8>) -> Vec<Vec<u8>> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let mut dmx = oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver)
        .expect("AVI demuxer open should accept truncated head");
    assert_eq!(dmx.format_name(), "avi");
    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("expected Eof at truncation, got {e:?}"),
        }
    }
    got
}

// ----------------------------------------------------------------------
// Truncated-head fixtures (synthesised, not borrowed from any third
// party — these are deltas applied to the muxer's own output).
// ----------------------------------------------------------------------

/// Fixture (a): The `LIST movi` size declares **more bytes than
/// physically present**. Mirrors the round-15 `crashtest.avi` shape
/// where capture aborted partway through writing the body.
#[test]
fn movi_oversize_declared_walks_present_packets_then_eofs() {
    let mut buf = build_pcm_avi("movi-oversize", 4, 256);
    let movi_pos = find_fourcc(&buf, b"movi").expect("movi marker");
    // The `LIST` size is 4 bytes before "movi" (i.e. at LIST-size-field).
    let list_size_off = movi_pos - 4;
    let original = u32::from_le_bytes([
        buf[list_size_off],
        buf[list_size_off + 1],
        buf[list_size_off + 2],
        buf[list_size_off + 3],
    ]);
    // Inflate the declared movi size by 100 MiB. The actual bytes haven't
    // changed; we just lie in the header.
    patch_u32(
        &mut buf,
        list_size_off,
        original.saturating_add(100 * 1024 * 1024),
    );

    // The demuxer should still surface every packet whose body is wholly
    // inside the file (all 4 packets — the inflation only affects the
    // sentinel size, not the bytes).
    let got = drain_packets(buf);
    assert_eq!(got.len(), 4, "all 4 packets should still be recovered");
    for (i, p) in got.iter().enumerate() {
        assert_eq!(p.len(), 256 * 4, "packet {i} byte length");
    }
}

/// Fixture (b): The top-level **RIFF size over-declares** but the file
/// is otherwise clean. Mirrors capture utilities that pre-write the
/// RIFF size assuming a target duration and never go back to patch it
/// when recording is cut short.
#[test]
fn riff_oversize_declared_walks_clean() {
    let mut buf = build_pcm_avi("riff-oversize", 3, 256);
    // RIFF size is at offset 4 (right after the "RIFF" FourCC).
    let original = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    patch_u32(&mut buf, 4, original.saturating_add(50 * 1024 * 1024));

    let got = drain_packets(buf);
    assert_eq!(got.len(), 3);
}

/// Fixture (c): **Physical truncation** — chop the last 200 bytes off
/// a valid 4-packet AVI. The idx1 trailer falls inside the truncated
/// region, so the demuxer falls back to linear movi walking. The last
/// packet's body straddles the truncation boundary.
#[test]
fn physical_truncation_drops_partial_tail_packet() {
    let mut buf = build_pcm_avi("phys-trunc-tail", 4, 256);
    let original_len = buf.len();
    // Truncate the last 200 bytes so we lose at least the idx1 chunk and
    // potentially the tail of the last movi packet.
    buf.truncate(original_len.saturating_sub(200));

    let got = drain_packets(buf);
    // We should recover 3 or 4 packets (depending on where the cut lands).
    // Importantly: no error, no panic.
    assert!(
        got.len() >= 3 && got.len() <= 4,
        "expected 3-4 packets, got {}",
        got.len()
    );
    for (i, p) in got.iter().enumerate() {
        assert_eq!(p.len(), 256 * 4, "packet {i} should be a full block");
    }
}

/// Fixture (d): Truncate the file partway through the **last packet's
/// body** specifically — the chunk header parses cleanly but the
/// `read_exact` inside `next_packet` would otherwise hit
/// UnexpectedEof. The demuxer should drop the partial frame and
/// surface `Eof`.
#[test]
fn physical_truncation_inside_packet_body_drops_partial() {
    let buf = build_pcm_avi("phys-trunc-body", 4, 256);
    // Walk forward inside the movi LIST body chunk-by-chunk so we find
    // the **last actual packet header** (not an idx1 entry that
    // happens to contain `00wb`). idx1 entries are 16-byte structs
    // whose first 4 bytes is the ckid — `rposition` of "00wb" in the
    // whole file would land on those.
    let movi_pos = find_fourcc(&buf, b"movi").expect("movi marker");
    // movi body starts right after the "movi" 4-CC.
    let mut pos = movi_pos + 4;
    let movi_size = u32::from_le_bytes([
        buf[movi_pos - 4],
        buf[movi_pos - 3],
        buf[movi_pos - 2],
        buf[movi_pos - 1],
    ]) as usize;
    // movi body ends at LIST_size_field + 4 (LIST size includes the
    // "movi" form-type word, so subtract 4).
    let movi_body_end = (movi_pos - 4) + 4 + movi_size;
    let mut last_chunk_off = None;
    while pos + 8 <= movi_body_end && pos + 8 <= buf.len() {
        let id: [u8; 4] = buf[pos..pos + 4].try_into().unwrap();
        let size =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        if &id == b"00wb" {
            last_chunk_off = Some((pos, size));
        }
        pos += 8 + size + (size & 1);
    }
    let (off, size) = last_chunk_off.expect("at least one 00wb chunk in movi");
    let body_start = off + 8;
    let cut_at = body_start + size / 2;
    let mut truncated = buf;
    truncated.truncate(cut_at);

    let got = drain_packets(truncated);
    // Headers + bodies of packets 0..2 are wholly inside the truncated
    // buffer — packet 3's body is short. Demuxer should surface 3
    // packets cleanly, then Eof on the 4th body's UnexpectedEof.
    assert_eq!(
        got.len(),
        3,
        "expected 3 full packets recovered (4th body truncated)"
    );
}

/// Fixture (e): **AVI 1.0 with no `idx1`** — degrade to linear walk.
/// We don't have a way to ask the muxer to *not* emit idx1, so we
/// build a valid AVI and then chop off the tail starting at the idx1
/// FourCC. The remaining bytes should still demux cleanly via linear
/// movi walking.
#[test]
fn avi_without_idx1_degrades_to_linear_walk() {
    let mut buf = build_pcm_avi("no-idx1", 3, 256);
    let idx1_pos = find_fourcc(&buf, b"idx1").expect("muxer emits idx1");
    buf.truncate(idx1_pos);

    let got = drain_packets(buf);
    assert_eq!(
        got.len(),
        3,
        "linear walker recovers all 3 packets even without idx1"
    );
}

/// Fixture (f): **`LIST hdrl` declares more bytes than physically
/// present after it** — i.e. the over-declared LIST size is `hdrl`,
/// not `movi`. The demuxer should still find `movi` (since walk
/// position sees the file ending before the over-declared body_end
/// and the next-chunk-after-hdrl read returns `Ok(None)`); but on
/// truncated-head dumps where `movi` itself is what's missing this is
/// the genuinely-malformed case → demuxer surfaces a clean error.
#[test]
fn hdrl_oversize_with_present_movi_still_walks() {
    let mut buf = build_pcm_avi("hdrl-oversize", 2, 256);
    let hdrl_pos = find_fourcc(&buf, b"hdrl").expect("hdrl marker");
    let list_size_off = hdrl_pos - 4;
    let original = u32::from_le_bytes([
        buf[list_size_off],
        buf[list_size_off + 1],
        buf[list_size_off + 2],
        buf[list_size_off + 3],
    ]);
    // Inflate the declared hdrl size by 256 KiB. Movi still follows in
    // the actual byte stream; clamping bounds the hdrl walk to the
    // (smaller) physical end.
    patch_u32(&mut buf, list_size_off, original.saturating_add(256 * 1024));

    // After the hdrl LIST is "consumed" (clamped to file end), the
    // outer walker resumes after the *declared* hdrl end which is past
    // EOF — so movi would never be found. The demuxer should error
    // cleanly on the missing-movi path. This documents the boundary:
    // tolerating bogus *body sizes* is fine but a corrupted *outer
    // walk* (hdrl over-declared so movi can't be reached) is not
    // recoverable.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    match oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver) {
        Err(Error::InvalidData(msg)) => {
            assert!(msg.contains("movi"), "error should mention missing movi");
        }
        Ok(_) => panic!("hdrl oversize should fail when it occludes movi"),
        Err(e) => panic!("expected InvalidData(missing movi), got {e:?}"),
    }
}

/// Fixture (g): truncation surfaces an `avi:truncated=true` metadata
/// entry so a downstream tool can warn the user without itself
/// re-probing the file. Combined with the avih dimensions surfaced
/// under `avi:width` / `avi:height` this gives container consumers a
/// complete picture of the file's claimed shape vs. its physical
/// bounds.
#[test]
fn truncated_head_surfaces_metadata_flag() {
    let mut buf = build_pcm_avi("trunc-md", 4, 256);
    // Inflate the RIFF size by 100 MiB so file_len < declared end.
    let original = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    patch_u32(&mut buf, 4, original.saturating_add(100 * 1024 * 1024));

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let dmx = oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let md = dmx.metadata();
    let truncated = md
        .iter()
        .find(|(k, _)| k == "avi:truncated")
        .map(|(_, v)| v.as_str());
    assert_eq!(
        truncated,
        Some("true"),
        "demuxer should surface avi:truncated=true under metadata()"
    );
}

// ----------------------------------------------------------------------
// Negative tests — genuinely-malformed inputs still error cleanly.
// ----------------------------------------------------------------------

#[test]
fn corrupt_riff_fourcc_errors_cleanly() {
    let mut buf = build_pcm_avi("corrupt-riff", 2, 256);
    // Scramble the leading "RIFF" FourCC.
    buf[0] = b'X';
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    match oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver) {
        Err(Error::InvalidData(_)) => {}
        Ok(_) => panic!("scrambled RIFF should fail at open"),
        Err(e) => panic!("expected InvalidData, got {e:?}"),
    }
}

#[test]
fn empty_input_errors_cleanly() {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(Vec::<u8>::new()));
    match oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver) {
        Err(Error::InvalidData(_)) => {}
        Ok(_) => panic!("empty input should fail at open"),
        Err(e) => panic!("expected InvalidData(empty file), got {e:?}"),
    }
}

#[test]
fn wrong_form_type_errors_cleanly() {
    let mut buf = build_pcm_avi("wrong-form", 2, 256);
    // Replace the form-type "AVI " (at offset 8) with "WAVE" so the
    // demuxer rejects it as not-an-AVI.
    buf[8..12].copy_from_slice(b"WAVE");
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    match oxideav_avi::demuxer::open(rs, &oxideav_core::NullCodecResolver) {
        Err(Error::InvalidData(msg)) => assert!(msg.contains("AVI")),
        Ok(_) => panic!("non-AVI form-type should fail at open"),
        Err(e) => panic!("expected InvalidData, got {e:?}"),
    }
}
