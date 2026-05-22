//! Round-92 `dwPaddingGranularity` + `JUNK`-aligned packet emission
//! AVI tests.
//!
//! Per AVI 1.0 §"AVIMAINHEADER" (docs/container/riff/
//! avi-riff-file-reference.md line 197): *"Alignment for data, in
//! bytes. Pad the data to multiples of this value."* The spec pairs
//! this field with §"Other Data Chunks" line 179: *"Data can be
//! aligned in an AVI file by inserting 'JUNK' chunks as needed.
//! Applications should ignore the contents of a 'JUNK' chunk."*
//!
//! This round wires the muxer's [`AviMuxOptions::with_padding_granularity`]
//! builder so a caller can stamp `avih.dwPaddingGranularity` and have
//! `movi` packet chunks pre-aligned to that granularity via inserted
//! `JUNK` chunks. The demuxer round-trips the value via
//! [`AviDemuxer::padding_granularity`] and surfaces it under the
//! `avi:padding_granularity` metadata key.
//!
//! Exercises:
//!
//! - **avih round-trip**: the muxer stamps the granularity into
//!   `avih.dwPaddingGranularity` and the demuxer reads it back via
//!   the typed accessor + the `avi:padding_granularity` metadata key.
//! - **Alignment promise**: every packet chunk's 8-byte header lands
//!   at a file-absolute offset divisible by the granularity.
//! - **JUNK insertion**: walking the raw `movi` LIST shows one `JUNK`
//!   chunk per misaligned packet (the first packet may have no JUNK
//!   if its natural offset is already aligned).
//! - **Demuxer transparency**: packets round-trip byte-equal through
//!   the JUNK-padded file; the inserted JUNK is invisible to the
//!   packet stream.
//! - **No-opt baseline**: a file written without
//!   `with_padding_granularity(...)` stamps `dwPaddingGranularity = 0`,
//!   has no `JUNK` chunks in `movi`, and the accessor returns 0.
//! - **Builder validation**: non-power-of-two and out-of-range
//!   granularity values reset the field to `None` so the legacy
//!   behaviour is preserved.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Demuxer, MediaType, Muxer, Packet, Rational,
    ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};

// ---------------------------------------------------------------------------
// Fixtures (mirror round-80 / round-89).
// ---------------------------------------------------------------------------

fn video_stream(index: u32) -> StreamInfo {
    let mut params =
        CodecParameters::video(CodecId::new("mjpeg")).with_tag(CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(16);
    params.height = Some(16);
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

/// Write a deterministic 2-stream (MJPEG + PCM) AVI of varying chunk
/// sizes that's small enough to inspect by byte but large enough to
/// force several JUNK insertions when stream-aligned.
fn write_avi(path: &std::path::Path, options: AviMuxOptions) -> Vec<(usize, usize)> {
    let streams = [video_stream(0), audio_stream(1)];
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_avi(ws, &streams, AviKind::Avi10, options).unwrap();
    mux.write_header().unwrap();

    // Six interleaved packets of varying sizes so the alignment
    // recipe encounters odd / even / large / small body lengths in
    // a single file. Returns each packet's (stream_index, size) so
    // tests can verify byte-equal round-trips.
    let plan: [(u32, usize); 6] = [(0, 64), (1, 8), (0, 65), (1, 9), (0, 31), (1, 4)];
    for (i, &(stream, size)) in plan.iter().enumerate() {
        let mut p = Packet::new(
            stream,
            streams[stream as usize].time_base,
            vec![(i as u8).wrapping_mul(0x33).wrapping_add(0x11); size],
        );
        p.pts = Some(i as i64);
        p.flags.keyframe = true;
        mux.write_packet(&p).unwrap();
    }

    mux.write_trailer().unwrap();
    plan.iter().map(|&(s, sz)| (s as usize, sz)).collect()
}

// ---------------------------------------------------------------------------
// 1. avih round-trip + typed accessor + metadata key.
// ---------------------------------------------------------------------------

#[test]
fn padding_granularity_roundtrip_via_accessor_and_metadata() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r92-pg-accessor.avi");
    let opts = AviMuxOptions::new().with_padding_granularity(512);
    write_avi(&tmp, opts);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.padding_granularity(),
        512,
        "avih.dwPaddingGranularity round-trip must match the builder value"
    );

    let md = dmx.metadata();
    let has = |k: &str, want: &str| md.iter().any(|(key, val)| key == k && val == want);
    assert!(
        has("avi:padding_granularity", "512"),
        "missing avi:padding_granularity = \"512\": {:?}",
        md.iter()
            .filter(|(k, _)| k.starts_with("avi:padding"))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// 2. Alignment promise: every packet chunk header is at a
// `granularity`-byte-aligned file-absolute offset.
// ---------------------------------------------------------------------------

/// Scan a raw AVI byte buffer's outer RIFF for the `movi` LIST and
/// return `(packet_chunk_offsets, junk_chunk_count)`. A packet chunk
/// here is any RIFF child of `movi` whose 4-CC parses as `NNxx`
/// (two ASCII digits + suffix); the helper deliberately ignores
/// `LIST rec ` clustering since the muxer is run in non-clustered
/// mode for these tests.
fn scan_movi(bytes: &[u8]) -> (Vec<u64>, usize) {
    assert_eq!(&bytes[0..4], b"RIFF", "outer chunk must be RIFF");
    assert_eq!(&bytes[8..12], b"AVI ", "form must be AVI ");

    // Walk top-level children of the outer RIFF form starting at
    // file offset 12.
    let mut pos: u64 = 12;
    let end = bytes.len() as u64;
    while pos + 8 <= end {
        let id = &bytes[pos as usize..(pos + 4) as usize];
        let size = u32::from_le_bytes(
            bytes[(pos + 4) as usize..(pos + 8) as usize]
                .try_into()
                .unwrap(),
        ) as u64;
        if id == b"LIST" {
            // form-type
            let form = &bytes[(pos + 8) as usize..(pos + 12) as usize];
            if form == b"movi" {
                // Walk children starting at pos+12 (just past form
                // FourCC) up to pos+8+size (LIST end).
                let mut p = pos + 12;
                let list_end = pos + 8 + size;
                let mut packet_offs = Vec::new();
                let mut junk = 0usize;
                while p + 8 <= list_end {
                    let cid = &bytes[p as usize..(p + 4) as usize];
                    let csize = u32::from_le_bytes(
                        bytes[(p + 4) as usize..(p + 8) as usize]
                            .try_into()
                            .unwrap(),
                    ) as u64;
                    if cid == b"JUNK" || cid == b"junk" {
                        junk += 1;
                    } else if cid[0].is_ascii_digit()
                        && cid[1].is_ascii_digit()
                        && cid[2].is_ascii_alphanumeric()
                        && cid[3].is_ascii_alphanumeric()
                    {
                        // Packet chunk — record its header start.
                        packet_offs.push(p);
                    }
                    // Advance: 8-byte header + body + word-pad.
                    p += 8 + csize + (csize & 1);
                }
                return (packet_offs, junk);
            }
            // Other LIST — skip the whole thing (8-byte header + body).
            pos += 8 + size + (size & 1);
            continue;
        }
        // Plain chunk: skip body + pad.
        pos += 8 + size + (size & 1);
    }
    panic!("no movi LIST found in fixture");
}

#[test]
fn every_packet_header_is_aligned() {
    for &granularity in &[16u32, 64, 512, 2048, 4096] {
        let tmp =
            std::env::temp_dir().join(format!("oxideav-avi-r92-pg-alignment-{granularity}.avi"));
        let opts = AviMuxOptions::new().with_padding_granularity(granularity);
        write_avi(&tmp, opts);

        let bytes = std::fs::read(&tmp).unwrap();
        let (offs, junk_count) = scan_movi(&bytes);

        assert!(
            !offs.is_empty(),
            "no packet chunks for granularity {granularity}"
        );
        for &off in &offs {
            assert_eq!(
                off % granularity as u64,
                0,
                "packet header offset {off:#x} not aligned to {granularity}"
            );
        }
        // At least one JUNK is expected for non-trivial granularities
        // — the 6-packet fixture has varying body sizes that won't
        // naturally land at every granularity boundary.
        assert!(
            junk_count > 0,
            "expected at least one JUNK chunk at granularity {granularity}, got 0"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Packet payloads survive the JUNK-padded layout byte-equal.
// ---------------------------------------------------------------------------

#[test]
fn packet_payloads_roundtrip_byte_equal_with_padding() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r92-pg-payload-roundtrip.avi");
    let opts = AviMuxOptions::new().with_padding_granularity(2048);
    let plan = write_avi(&tmp, opts);

    // Reconstruct each packet's expected byte: the same recipe as
    // `write_avi`.
    let expected: Vec<Vec<u8>> = plan
        .iter()
        .enumerate()
        .map(|(i, &(_, sz))| vec![(i as u8).wrapping_mul(0x33).wrapping_add(0x11); sz])
        .collect();

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Read all packets in muxer order.
    let mut got: Vec<Vec<u8>> = Vec::new();
    while let Ok(pkt) = dmx.next_packet() {
        got.push(pkt.data);
    }
    assert_eq!(got.len(), expected.len(), "packet count mismatch");
    for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            g, e,
            "packet {i} payload differs from expected after JUNK padding"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. No-opt baseline: dwPaddingGranularity = 0, no JUNK chunks, no
// metadata key.
// ---------------------------------------------------------------------------

#[test]
fn no_padding_granularity_is_legacy_layout() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r92-pg-no-opt.avi");
    let opts = AviMuxOptions::new();
    write_avi(&tmp, opts);

    let bytes = std::fs::read(&tmp).unwrap();
    let (_offs, junk_count) = scan_movi(&bytes);
    assert_eq!(
        junk_count, 0,
        "no_padding_granularity must not emit JUNK chunks in movi"
    );

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.padding_granularity(), 0, "default avih is 0");
    let md = dmx.metadata();
    assert!(
        !md.iter().any(|(k, _)| k == "avi:padding_granularity"),
        "metadata key must be omitted when value is 0"
    );
}

// ---------------------------------------------------------------------------
// 5. Builder validation: only powers of two in [2, 65536] take effect.
// ---------------------------------------------------------------------------

#[test]
fn builder_rejects_invalid_granularity_values() {
    let cases: &[(u32, u32)] = &[
        (0, 0),     // zero → None → 0 in avih
        (1, 0),     // below floor → None
        (3, 0),     // not power-of-two
        (1000, 0),  // not power-of-two
        (65537, 0), // above ceiling
        (1_000_000, 0),
        (2, 2),
        (16, 16),
        (4096, 4096),
        (65536, 65536),
    ];
    for &(input, want) in cases {
        let tmp = std::env::temp_dir().join(format!("oxideav-avi-r92-pg-build-{input}.avi"));
        let opts = AviMuxOptions::new().with_padding_granularity(input);
        write_avi(&tmp, opts);

        let reg = CodecRegistry::new();
        let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
        let dmx = demuxer_open_avi(rs, &reg).unwrap();
        assert_eq!(
            dmx.padding_granularity(),
            want,
            "with_padding_granularity({input}) stamped wrong avih value"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Pre-existing AVI 1.0 layout still parses (no spurious
// `avi:padding_granularity = 0` key).
// ---------------------------------------------------------------------------

#[test]
fn legacy_zero_padding_is_observable_via_absence() {
    let tmp = std::env::temp_dir().join("oxideav-avi-r92-pg-legacy-zero.avi");
    write_avi(&tmp, AviMuxOptions::new());

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Accessor returns 0 (the spec's "no alignment" sentinel).
    assert_eq!(dmx.padding_granularity(), 0);

    // Metadata key must be absent (omitted when value is 0). This
    // lets a downstream consumer distinguish "muxer asked for 0
    // explicitly" (still no key) from "muxer asked for 4096" (key
    // present with value "4096").
    let md = dmx.metadata();
    let keys: Vec<&String> = md.iter().map(|(k, _)| k).collect();
    assert!(
        !keys.iter().any(|k| k.as_str() == "avi:padding_granularity"),
        "avi:padding_granularity should be omitted, got keys: {keys:?}"
    );
}
