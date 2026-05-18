//! Round-19 AVI feature tests.
//!
//! Covers:
//!
//! - **C1** Top-down DIB orientation: BMIH `biHeight` sign carries the
//!   origin-corner convention per VfW `wingdi.h` §"biHeight sign
//!   rules" (positive ⇒ bottom-up, negative ⇒ top-down). Round-19
//!   preserves the sign on the parse side
//!   (`AviDemuxer::stream_top_down` + `avi:vids.<n>.top_down`
//!   metadata key) and round-trips it on the mux side
//!   (`AviMuxOptions::with_top_down_video`).
//! - **C2** `BI_BITFIELDS` color-mask exposure: when an uncompressed
//!   RGB stream's BMIH declares `biCompression == 3`, the trailing
//!   three DWORDs are the R/G/B masks per VfW §"Color tables
//!   (palettes)". `AviDemuxer::stream_bitfields_masks` returns the
//!   parsed triple; `avi:vids.<n>.bitfields =
//!   "r=<hex>,g=<hex>,b=<hex>"` metadata key mirrors it for
//!   non-typed callers.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, Demuxer, MediaType, Muxer, Packet, Rational, ReadSeek,
    StreamInfo, TimeBase, WriteSeek,
};

use oxideav_avi::demuxer::open_avi as demuxer_open_avi;
use oxideav_avi::muxer::{open_avi, AviKind, AviMuxOptions};
use oxideav_avi::stream_format::{
    parse_bitfields_masks, write_bitmap_info_header_oriented, BI_BITFIELDS,
};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn rgb24_stream(width: u32, height: u32) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("rgb24"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn synth_rgb24_payload(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let n = (width * height * 3) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(((i as u32).wrapping_mul(seed as u32 + 1)) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// C1: top-down DIB orientation round-trip.
// ---------------------------------------------------------------------------

#[test]
fn top_down_video_round_trips_through_mux_demux() {
    // Round-19 C1: mux a `rgb24` (BI_RGB) stream with
    // `with_top_down_video(0)`, demux, and verify both the typed
    // accessor `stream_top_down` and the metadata key
    // `avi:vids.0.top_down` reflect the on-wire negative biHeight.
    let width = 64u32;
    let height = 48u32;
    let stream = rgb24_stream(width, height);
    let frames: Vec<Vec<u8>> = (0..4)
        .map(|i| synth_rgb24_payload(width, height, i as u8))
        .collect();

    let tmp = std::env::temp_dir().join("oxideav-avi-r19-top-down.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_top_down_video(0);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in frames.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some(i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    // Typed accessor returns true.
    assert_eq!(
        dmx.stream_top_down(0),
        Some(true),
        "stream_top_down(0) must reflect the negative on-wire biHeight"
    );
    // Metadata key surfaces the same fact for non-typed callers.
    let saw_meta = dmx
        .metadata()
        .iter()
        .any(|(k, v)| k == "avi:vids.0.top_down" && v == "true");
    assert!(saw_meta, "avi:vids.0.top_down=true metadata key missing");
    // Width/height stay positive (the absolute pixel count).
    assert_eq!(dmx.streams()[0].params.width, Some(width));
    assert_eq!(dmx.streams()[0].params.height, Some(height));
}

#[test]
fn default_video_is_bottom_up() {
    // Round-19 C1: no `with_top_down_video` call ⇒ positive
    // `biHeight` ⇒ `stream_top_down(0) == Some(false)` and no
    // metadata key.
    let stream = rgb24_stream(32, 32);
    let frame = synth_rgb24_payload(32, 32, 1);
    let tmp = std::env::temp_dir().join("oxideav-avi-r19-bottom-up.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, frame);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(dmx.stream_top_down(0), Some(false));
    let saw_meta = dmx
        .metadata()
        .iter()
        .any(|(k, _)| k == "avi:vids.0.top_down");
    assert!(
        !saw_meta,
        "bottom-up DIB must NOT emit the avi:vids.0.top_down metadata key"
    );
}

#[test]
fn top_down_flag_dropped_for_compressed_fourcc() {
    // Round-19 C1: VfW §"biHeight sign rules" REQUIRES positive
    // `biHeight` for compressed FourCCs. The muxer silently drops
    // the flag (rather than producing an out-of-spec file). Use
    // `mjpeg` as a representative compressed stream.
    let mut params = CodecParameters::video(CodecId::new("mjpeg"))
        .with_tag(oxideav_core::CodecTag::fourcc(b"MJPG"));
    params.media_type = MediaType::Video;
    params.width = Some(32);
    params.height = Some(24);
    params.frame_rate = Some(Rational::new(25, 1));
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 25),
        duration: None,
        start_time: Some(0),
        params,
    };

    let tmp = std::env::temp_dir().join("oxideav-avi-r19-compressed-drops-top-down.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = AviMuxOptions::new().with_top_down_video(0);
        let mut mux = open_avi(ws, std::slice::from_ref(&stream), AviKind::Avi10, opts).unwrap();
        mux.write_header().unwrap();
        // Pretend a one-byte JPEG payload (test only inspects header).
        let mut pkt = Packet::new(0, stream.time_base, vec![0xFFu8; 8]);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert_eq!(
        dmx.stream_top_down(0),
        Some(false),
        "muxer must drop top_down for compressed FourCCs"
    );
}

#[test]
fn with_top_down_video_deduplicates() {
    // Round-19 C1: builder is idempotent — repeated calls with the
    // same stream index don't push duplicate entries.
    let opts = AviMuxOptions::new()
        .with_top_down_video(0)
        .with_top_down_video(0)
        .with_top_down_video(2)
        .with_top_down_video(2);
    assert_eq!(opts.top_down_video_streams, vec![0, 2]);
}

// ---------------------------------------------------------------------------
// C2: BI_BITFIELDS color-mask exposure.
// ---------------------------------------------------------------------------

/// Walk the RIFF tree of `bytes` starting at `cursor` until end,
/// invoking `visit` on every chunk header (`RIFF` / `LIST` / leaf)
/// with `(absolute_header_offset, fourcc, declared_size)`. For
/// `RIFF` / `LIST` chunks, the form type is consumed but the visitor
/// is invoked only on the header itself. The walk is recursive into
/// every `RIFF` / `LIST` body so the visitor sees enclosing chunks
/// before their children — useful for size patching, because the
/// caller can record the offsets of every ancestor of the chunk
/// they're about to grow.
fn walk_riff<F: FnMut(usize, [u8; 4], u32, bool)>(bytes: &[u8], mut visit: F) {
    fn recurse(
        bytes: &[u8],
        start: usize,
        end: usize,
        depth: u32,
        visit: &mut dyn FnMut(usize, [u8; 4], u32, bool),
    ) {
        let mut cur = start;
        while cur + 8 <= end {
            let mut fourcc = [0u8; 4];
            fourcc.copy_from_slice(&bytes[cur..cur + 4]);
            let size = u32::from_le_bytes(bytes[cur + 4..cur + 8].try_into().unwrap());
            let body_start = cur + 8;
            let body_end = body_start + size as usize;
            let is_list = &fourcc == b"RIFF" || &fourcc == b"LIST";
            visit(cur, fourcc, size, depth == 0);
            if body_end > bytes.len() {
                return;
            }
            if is_list && body_start + 4 <= body_end {
                // Form type is bytes 0..4 of the body; recurse into
                // the rest.
                recurse(bytes, body_start + 4, body_end, depth + 1, visit);
            }
            // Pad to word boundary.
            let next = body_end + (size as usize & 1);
            cur = next;
        }
    }
    recurse(bytes, 0, bytes.len(), 0, &mut visit);
}

/// Mutate a freshly muxed `rgb24` AVI to swap its BMIH compression
/// FourCC from `BI_RGB` (all zero) to `BI_BITFIELDS` (`[3,0,0,0]`)
/// and stamp three R/G/B mask DWORDs into the extradata slot
/// immediately following the 40-byte BMIH. Patches every enclosing
/// RIFF/LIST size DWORD (walking the actual chunk tree rather than
/// grepping the byte stream, so patterns inside packet payloads
/// don't get bumped).
fn mutate_avi_to_bitfields(
    src: &std::path::Path,
    dst: &std::path::Path,
    r_mask: u32,
    g_mask: u32,
    b_mask: u32,
) {
    let mut bytes = std::fs::read(src).unwrap();

    // Phase 1: tree-walk to record (a) the strf chunk offset and (b)
    // every ancestor LIST/RIFF whose body wraps it. We track ancestors
    // by stack; the walk_riff helper above doesn't expose a stack
    // directly so reimplement with explicit recursion.
    let mut strf_at: Option<usize> = None;
    let mut wrappers: Vec<usize> = Vec::new();
    fn find(
        bytes: &[u8],
        start: usize,
        end: usize,
        ancestors: &mut Vec<usize>,
        strf_at: &mut Option<usize>,
        wrappers: &mut Vec<usize>,
    ) {
        let mut cur = start;
        while cur + 8 <= end {
            let mut fourcc = [0u8; 4];
            fourcc.copy_from_slice(&bytes[cur..cur + 4]);
            let size = u32::from_le_bytes(bytes[cur + 4..cur + 8].try_into().unwrap());
            let body_start = cur + 8;
            let body_end = body_start + size as usize;
            if body_end > bytes.len() {
                return;
            }
            if &fourcc == b"strf" && strf_at.is_none() {
                *strf_at = Some(cur);
                *wrappers = ancestors.clone();
                return;
            }
            if (&fourcc == b"RIFF" || &fourcc == b"LIST") && body_start + 4 <= body_end {
                ancestors.push(cur);
                find(
                    bytes,
                    body_start + 4,
                    body_end,
                    ancestors,
                    strf_at,
                    wrappers,
                );
                ancestors.pop();
                if strf_at.is_some() {
                    return;
                }
            }
            cur = body_end + (size as usize & 1);
        }
    }
    let mut ancestors = Vec::new();
    find(
        &bytes,
        0,
        bytes.len(),
        &mut ancestors,
        &mut strf_at,
        &mut wrappers,
    );
    let strf_at = strf_at.expect("expected one `strf`");

    let bmih_at = strf_at + 8;
    let bi_size_at = bmih_at;
    let bi_compression_at = bmih_at + 16;
    let bi_size_old = u32::from_le_bytes(bytes[bi_size_at..bi_size_at + 4].try_into().unwrap());

    // Stamp BI_BITFIELDS into biCompression.
    bytes[bi_compression_at..bi_compression_at + 4].copy_from_slice(&BI_BITFIELDS);

    // Build the 12-byte color-mask trailer.
    let mut mask_bytes = Vec::with_capacity(12);
    mask_bytes.extend_from_slice(&r_mask.to_le_bytes());
    mask_bytes.extend_from_slice(&g_mask.to_le_bytes());
    mask_bytes.extend_from_slice(&b_mask.to_le_bytes());

    // Insert immediately after the existing BMIH body.
    let insert_at = bmih_at + bi_size_old as usize;
    bytes.splice(insert_at..insert_at, mask_bytes.iter().copied());

    // Patch biSize, the strf chunk size, and every wrapping LIST/RIFF
    // size by exactly +12.
    let bi_size_new = bi_size_old + 12;
    bytes[bi_size_at..bi_size_at + 4].copy_from_slice(&bi_size_new.to_le_bytes());
    let strf_size_at = strf_at + 4;
    let strf_size_old =
        u32::from_le_bytes(bytes[strf_size_at..strf_size_at + 4].try_into().unwrap());
    let strf_size_new = strf_size_old + 12;
    bytes[strf_size_at..strf_size_at + 4].copy_from_slice(&strf_size_new.to_le_bytes());
    for wrapper_at in wrappers {
        let size_at = wrapper_at + 4;
        let old = u32::from_le_bytes(bytes[size_at..size_at + 4].try_into().unwrap());
        let new = old + 12;
        bytes[size_at..size_at + 4].copy_from_slice(&new.to_le_bytes());
    }

    std::fs::write(dst, &bytes).unwrap();
}

// Silence the now-unused walker helper; keep it around for any
// future test that wants a generic chunk walker.
#[allow(dead_code)]
fn _walk_riff_unused(bytes: &[u8]) {
    walk_riff(bytes, |_, _, _, _| {});
}

#[test]
fn bitfields_masks_surface_via_typed_accessor_and_metadata() {
    // Round-19 C2: mux a vanilla `rgb24` (BI_RGB) AVI, then byte-mutate
    // its BMIH to declare BI_BITFIELDS with RGB565 masks. The demuxer
    // must parse the three trailing DWORDs and expose them via
    // `stream_bitfields_masks` + `avi:vids.0.bitfields`.
    let width = 32u32;
    let height = 32u32;
    let stream = rgb24_stream(width, height);
    let frame = synth_rgb24_payload(width, height, 7);

    let tmp = std::env::temp_dir().join("oxideav-avi-r19-bitfields-src.avi");
    let mutated = std::env::temp_dir().join("oxideav-avi-r19-bitfields-mut.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, frame);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    // RGB565 masks per VfW §"biCompression".
    let r = 0xF800u32;
    let g = 0x07E0u32;
    let b = 0x001Fu32;
    mutate_avi_to_bitfields(&tmp, &mutated, r, g, b);

    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&mutated).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();

    assert_eq!(
        dmx.stream_bitfields_masks(0),
        Some((r, g, b)),
        "stream_bitfields_masks(0) must parse the three trailing DWORDs"
    );

    let meta = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "avi:vids.0.bitfields")
        .expect("avi:vids.0.bitfields metadata key missing");
    let expected = format!("r=0x{r:08X},g=0x{g:08X},b=0x{b:08X}");
    assert_eq!(meta.1, expected, "metadata value mismatch");
}

#[test]
fn bitfields_masks_none_for_non_bitfields_streams() {
    // Round-19 C2: a vanilla rgb24 (BI_RGB) stream has no
    // BI_BITFIELDS masks; the accessor must return None.
    let stream = rgb24_stream(32, 32);
    let frame = synth_rgb24_payload(32, 32, 1);
    let tmp = std::env::temp_dir().join("oxideav-avi-r19-no-bitfields.avi");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_avi(
            ws,
            std::slice::from_ref(&stream),
            AviKind::Avi10,
            AviMuxOptions::new(),
        )
        .unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, frame);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let reg = CodecRegistry::new();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = demuxer_open_avi(rs, &reg).unwrap();
    assert!(dmx.stream_bitfields_masks(0).is_none());
}

#[test]
fn bitfields_masks_helper_matches_manual_le_decode() {
    // Round-19 C2: the pure-bytes helper agrees with a manual
    // little-endian decode for the three common mask layouts in
    // VfW §"biCompression".
    let cases: &[(u32, u32, u32, &str)] = &[
        (0xF800, 0x07E0, 0x001F, "RGB565"),
        (0x7C00, 0x03E0, 0x001F, "RGB555"),
        (0x00FF_0000, 0x0000_FF00, 0x0000_00FF, "BGRA32"),
    ];
    for (r, g, b, label) in cases {
        let mut buf = Vec::new();
        buf.extend_from_slice(&r.to_le_bytes());
        buf.extend_from_slice(&g.to_le_bytes());
        buf.extend_from_slice(&b.to_le_bytes());
        let parsed = parse_bitfields_masks(&buf).unwrap_or_else(|| panic!("{label}"));
        assert_eq!(parsed, (*r, *g, *b), "{label}");
    }
}

// ---------------------------------------------------------------------------
// C1 + C2: the orientation helper byte layout.
// ---------------------------------------------------------------------------

#[test]
fn write_bitmap_info_header_oriented_top_down_carries_negative_height() {
    // Direct check on the low-level helper: top_down=true on a
    // 24-bit BI_RGB header writes biHeight as i32 = -480 (offset
    // 8..12 in the BMIH).
    let bytes = write_bitmap_info_header_oriented(640, 480, [0, 0, 0, 0], 24, &[], true);
    assert!(bytes.len() >= 40);
    let h = i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    assert_eq!(h, -480);

    // top_down=false ⇒ positive.
    let bytes = write_bitmap_info_header_oriented(640, 480, [0, 0, 0, 0], 24, &[], false);
    let h = i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    assert_eq!(h, 480);
}
