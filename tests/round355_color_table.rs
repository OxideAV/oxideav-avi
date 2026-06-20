//! Round-355: baseline DIB color table (`RGBQUAD bmiColors[]`) parsing
//! for indexed-colour video streams + per-frame effective-palette
//! resolution composing the baseline with `xxpc` palette-change deltas.
//!
//! An indexed-colour AVI video stream (`biBitCount` of 1 / 4 / 8) carries
//! its palette as an `RGBQUAD bmiColors[]` color table immediately after
//! the 40-byte BITMAPINFOHEADER inside the `strf` chunk. Per the RIFF MCI
//! reference (`docs/container/riff/metadata/microsoft-riffmci.pdf`):
//!
//!   §"Bitmap Color Table": "The color table is a collection of 24-bit
//!   RGB values. There are as many entries in the color table as there
//!   are colors in the bitmap. The color table isn't present for bitmaps
//!   with 24 color bits ..."
//!
//!   §"Color Table Structure": `RGBQUAD { rgbBlue; rgbGreen; rgbRed;
//!   rgbReserved; }` — on-disk byte order blue / green / red / reserved.
//!
//!   §"Note on Windows DIBs": "If the biClrUsed field is set to 0, the
//!   bitmap uses the maximum number of colors corresponding to the value
//!   of the [biBitCount] field" — i.e. `1 << biBitCount`.
//!
//! The `xxpc` palette-change chunk (`AVIPALCHANGE`, AVI 1.0 reference
//! §"AVIPALCHANGE") retroactively rewrites palette entries mid-stream;
//! `effective_palette_at` composes the baseline table with the cumulative
//! deltas at or before a packet.
//!
//! Clean-room source:
//!   - `docs/container/riff/metadata/microsoft-riffmci.pdf` §"Bitmap
//!     Color Table" / §"Color Table Structure" / §"Interpreting the
//!     Color Table" / §"Note on Windows DIBs"
//!   - `docs/container/riff/avi-riff-file-reference.md` §"AVIPALCHANGE"

use oxideav_avi::demuxer::open_avi;
use oxideav_avi::stream_format::RgbQuad;
use oxideav_core::{CodecRegistry, Demuxer, ReadSeek};

/// Minimal raw-AVI builder. Appends `RIFF AVI ` with a single `LIST
/// hdrl` (`avih` + one `strl`: `strh` + the supplied `strf`) and a
/// `LIST movi` carrying the supplied data chunks.
struct AviBuilder {
    strf: Vec<u8>,
    movi_chunks: Vec<([u8; 4], Vec<u8>)>,
}

impl AviBuilder {
    fn new(strf: Vec<u8>) -> Self {
        Self {
            strf,
            movi_chunks: Vec::new(),
        }
    }

    fn chunk(mut self, fourcc: &[u8; 4], body: &[u8]) -> Self {
        self.movi_chunks.push((*fourcc, body.to_vec()));
        self
    }

    fn build(&self) -> Vec<u8> {
        // --- avih (56-byte body): one video stream, the rest zeroed.
        let mut avih = vec![0u8; 56];
        avih[24..28].copy_from_slice(&1u32.to_le_bytes()); // dwStreams = 1

        // --- strh (56-byte AVISTREAMHEADER) for a `vids` stream.
        let mut strh = vec![0u8; 56];
        strh[0..4].copy_from_slice(b"vids");
        strh[20..24].copy_from_slice(&1u32.to_le_bytes()); // dwScale = 1
        strh[24..28].copy_from_slice(&25u32.to_le_bytes()); // dwRate = 25

        let strl = list(
            b"strl",
            &[chunk(b"strh", &strh), chunk(b"strf", &self.strf)],
        );
        let hdrl = list(b"hdrl", &[chunk(b"avih", &avih), strl]);

        // Build the movi body, tracking each child chunk's offset
        // relative to the `movi` FourCC so we can emit a matching idx1.
        let mut movi_body = Vec::new();
        movi_body.extend_from_slice(b"movi");
        let mut idx1_entries: Vec<([u8; 4], u32, u32)> = Vec::new();
        for (fourcc, body) in &self.movi_chunks {
            // Offset of this chunk's ckid relative to the `movi` FourCC.
            let rel_off = movi_body.len() as u32;
            idx1_entries.push((*fourcc, rel_off, body.len() as u32));
            movi_body.extend_from_slice(&chunk(fourcc, body));
        }
        let movi = chunk(b"LIST", &movi_body);

        // idx1: one 16-byte AVIOLDINDEX entry per movi chunk.
        // (ckid[4], dwFlags u32, dwOffset u32, dwSize u32).
        let mut idx1_body = Vec::new();
        for (fourcc, off, size) in &idx1_entries {
            idx1_body.extend_from_slice(fourcc);
            idx1_body.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
            idx1_body.extend_from_slice(&off.to_le_bytes()); // dwOffset (movi-relative)
            idx1_body.extend_from_slice(&size.to_le_bytes()); // dwSize
        }
        let idx1 = chunk(b"idx1", &idx1_body);

        // RIFF AVI  form.
        let mut form_body = Vec::new();
        form_body.extend_from_slice(b"AVI ");
        form_body.extend_from_slice(&hdrl);
        form_body.extend_from_slice(&movi);
        form_body.extend_from_slice(&idx1);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(form_body.len() as u32).to_le_bytes());
        out.extend_from_slice(&form_body);
        out
    }
}

/// One RIFF chunk: 4-CC + LE u32 size + body (word-padded to even).
fn chunk(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len() + 1);
    out.extend_from_slice(fourcc);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// One `LIST` chunk: `LIST` + size + form-type 4-CC + concatenated
/// children.
fn list(form: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(form);
    for c in children {
        body.extend_from_slice(c);
    }
    chunk(b"LIST", &body)
}

/// 40-byte BITMAPINFOHEADER for an 8-bpp `BI_RGB` indexed DIB, followed
/// by the supplied color table bytes.
fn indexed_strf(width: u32, height: u32, bit_count: u16, clr_used: u32, table: &[u8]) -> Vec<u8> {
    let mut bmih = vec![0u8; 40];
    bmih[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
    bmih[4..8].copy_from_slice(&width.to_le_bytes());
    bmih[8..12].copy_from_slice(&(height as i32).to_le_bytes());
    bmih[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
    bmih[14..16].copy_from_slice(&bit_count.to_le_bytes());
    // biCompression = BI_RGB ([0,0,0,0]) — already zero.
    bmih[32..36].copy_from_slice(&clr_used.to_le_bytes()); // biClrUsed
    bmih.extend_from_slice(table);
    bmih
}

/// Build an `RGBQUAD` color-table byte buffer (blue/green/red/reserved
/// per entry).
fn color_table_bytes(quads: &[(u8, u8, u8, u8)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(quads.len() * 4);
    for &(b, g, r, x) in quads {
        out.extend_from_slice(&[b, g, r, x]);
    }
    out
}

/// Build an `AVIPALCHANGE` (`xxpc`) chunk body: `bFirstEntry`,
/// `bNumEntries`, `wFlags` (u16), then `PALETTEENTRY[]`
/// (red/green/blue/flags per entry).
fn palchange_body(first_entry: u8, entries: &[(u8, u8, u8, u8)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(first_entry);
    out.push(entries.len() as u8);
    out.extend_from_slice(&0u16.to_le_bytes()); // wFlags
    for &(r, g, b, f) in entries {
        out.extend_from_slice(&[r, g, b, f]);
    }
    out
}

fn open(bytes: Vec<u8>) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = CodecRegistry::new();
    let input: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    open_avi(input, &reg).expect("open_avi")
}

// ---------------------------------------------------------------------
// Baseline color-table parsing.
// ---------------------------------------------------------------------

#[test]
fn stream_palette_8bpp_clr_used() {
    let table = color_table_bytes(&[
        (0x10, 0x20, 0x30, 0x00),
        (0x40, 0x50, 0x60, 0x00),
        (0x70, 0x80, 0x90, 0x00),
    ]);
    let strf = indexed_strf(4, 4, 8, 3, &table);
    let dmx = open(AviBuilder::new(strf).chunk(b"00dc", &[0u8; 16]).build());

    let pal = dmx.stream_palette(0).expect("indexed stream has a palette");
    assert_eq!(pal.len(), 3, "biClrUsed == 3 ⇒ three entries");
    assert_eq!(
        pal[0],
        RgbQuad {
            blue: 0x10,
            green: 0x20,
            red: 0x30,
            reserved: 0x00
        }
    );
    assert_eq!(pal[2].red, 0x90);
}

#[test]
fn stream_palette_clr_used_zero_means_depth_max() {
    // biClrUsed == 0 ⇒ 1 << biBitCount entries. 4-bpp ⇒ 16.
    let quads: Vec<(u8, u8, u8, u8)> = (0..16).map(|i| (i, i, i, 0)).collect();
    let table = color_table_bytes(&quads);
    let strf = indexed_strf(2, 2, 4, 0, &table);
    let dmx = open(AviBuilder::new(strf).chunk(b"00dc", &[0u8; 2]).build());

    let pal = dmx
        .stream_palette(0)
        .expect("4-bpp indexed stream has a palette");
    assert_eq!(pal.len(), 16, "biClrUsed == 0 ⇒ 1 << 4 == 16 entries");
}

#[test]
fn stream_palette_absent_for_truecolour() {
    // 24-bpp truecolour DIB carries no palette even with trailing bytes.
    let strf = indexed_strf(2, 2, 24, 0, &[0u8; 16]);
    let dmx = open(AviBuilder::new(strf).chunk(b"00db", &[0u8; 12]).build());
    assert!(
        dmx.stream_palette(0).is_none(),
        "truecolour DIB has no color table"
    );
}

#[test]
fn palette_entries_metadata_key() {
    let table = color_table_bytes(&[(1, 2, 3, 0), (4, 5, 6, 0), (7, 8, 9, 0)]);
    let strf = indexed_strf(4, 4, 8, 3, &table);
    let dmx = open(AviBuilder::new(strf).chunk(b"00dc", &[0u8; 16]).build());

    let md = dmx.metadata();
    let entry = md
        .iter()
        .find(|(k, _)| k == "avi:vids.0.palette_entries")
        .expect("palette_entries metadata key emitted for indexed stream");
    assert_eq!(entry.1, "3");
}

#[test]
fn palette_entries_metadata_absent_for_truecolour() {
    let strf = indexed_strf(2, 2, 24, 0, &[0u8; 8]);
    let dmx = open(AviBuilder::new(strf).chunk(b"00db", &[0u8; 12]).build());
    let md = dmx.metadata();
    assert!(
        !md.iter().any(|(k, _)| k == "avi:vids.0.palette_entries"),
        "no palette_entries key for a truecolour stream"
    );
}

#[test]
fn stream_palette_out_of_range_stream_is_none() {
    let table = color_table_bytes(&[(1, 2, 3, 0)]);
    let strf = indexed_strf(2, 2, 8, 1, &table);
    let dmx = open(AviBuilder::new(strf).chunk(b"00dc", &[0u8; 4]).build());
    assert!(dmx.stream_palette(7).is_none());
}

// ---------------------------------------------------------------------
// Effective-palette resolution (baseline + xxpc deltas).
// ---------------------------------------------------------------------

#[test]
fn effective_palette_no_changes_equals_baseline() {
    let table = color_table_bytes(&[(0x11, 0x22, 0x33, 0), (0x44, 0x55, 0x66, 0)]);
    let strf = indexed_strf(2, 2, 8, 2, &table);
    let dmx = open(
        AviBuilder::new(strf)
            .chunk(b"00dc", &[0u8; 4])
            .chunk(b"00dc", &[0u8; 4])
            .build(),
    );

    let baseline = dmx.stream_palette(0).unwrap().to_vec();
    let eff = dmx.effective_palette_at(0, 1).expect("effective palette");
    assert_eq!(eff, baseline, "no xxpc ⇒ effective palette == baseline");
}

#[test]
fn effective_palette_applies_delta_after_its_position() {
    // Baseline: 3 entries. An xxpc between packet 0 and packet 1 rewrites
    // entry index 1. PALETTEENTRY on-disk order is red/green/blue/flags;
    // the resolved RgbQuad must carry those swapped into blue/green/red.
    let table = color_table_bytes(&[
        (0x01, 0x01, 0x01, 0),
        (0x02, 0x02, 0x02, 0),
        (0x03, 0x03, 0x03, 0),
    ]);
    let strf = indexed_strf(2, 2, 8, 3, &table);
    let pc = palchange_body(1, &[(0xAA, 0xBB, 0xCC, 0)]); // red,green,blue
    let dmx = open(
        AviBuilder::new(strf)
            .chunk(b"00dc", &[0u8; 4]) // packet seq 0
            .chunk(b"00pc", &pc) // palette change (side-band)
            .chunk(b"00dc", &[0u8; 4]) // packet seq 1
            .build(),
    );

    // Before the change is reached (at packet seq 0) the palette is the
    // untouched baseline.
    let at0 = dmx.effective_palette_at(0, 0).unwrap();
    assert_eq!(at0[1].red, 0x02, "entry 1 still baseline at packet 0");

    // At packet seq 1 the change has been applied. Note the channel
    // swap: PALETTEENTRY red 0xAA ⇒ RgbQuad.red, blue 0xCC ⇒ RgbQuad.blue.
    let at1 = dmx.effective_palette_at(0, 1).unwrap();
    assert_eq!(at1[1].red, 0xAA);
    assert_eq!(at1[1].green, 0xBB);
    assert_eq!(at1[1].blue, 0xCC);
    // Untouched entries keep their baseline value.
    assert_eq!(at1[0].red, 0x01);
    assert_eq!(at1[2].red, 0x03);
}

#[test]
fn effective_palette_none_without_baseline() {
    // A truecolour stream has no baseline ⇒ no effective palette.
    let strf = indexed_strf(2, 2, 24, 0, &[]);
    let dmx = open(AviBuilder::new(strf).chunk(b"00db", &[0u8; 12]).build());
    assert!(dmx.effective_palette_at(0, 0).is_none());
}

#[test]
fn effective_palette_after_changes_count_based() {
    // The count-based accessor composes the baseline with the first N
    // deltas regardless of packet position. Two deltas: the first
    // rewrites entry 0, the second rewrites entry 1.
    let table = color_table_bytes(&[(0, 0, 0, 0), (0, 0, 0, 0)]);
    let strf = indexed_strf(2, 2, 8, 2, &table);
    // palchange_body args are (red, green, blue, flags) — PALETTEENTRY
    // on-disk order. The resolved RgbQuad keeps red↔red, blue↔blue.
    let pc0 = palchange_body(0, &[(0x11, 0x22, 0x33, 0)]);
    let pc1 = palchange_body(1, &[(0x44, 0x55, 0x66, 0)]);
    let dmx = open(
        AviBuilder::new(strf)
            .chunk(b"00dc", &[0u8; 4])
            .chunk(b"00pc", &pc0)
            .chunk(b"00pc", &pc1)
            .build(),
    );

    // Zero changes ⇒ baseline (all black).
    let at0 = dmx.effective_palette_after_changes(0, 0).unwrap();
    assert_eq!(at0[0].red, 0);
    assert_eq!(at0[1].red, 0);

    // One change ⇒ entry 0 updated, entry 1 still baseline.
    let at1 = dmx.effective_palette_after_changes(0, 1).unwrap();
    assert_eq!(at1[0].red, 0x11);
    assert_eq!(at1[0].blue, 0x33);
    assert_eq!(at1[1].red, 0);

    // Saturating: u32::MAX ⇒ both changes applied.
    let all = dmx.effective_palette_after_changes(0, u32::MAX).unwrap();
    assert_eq!(all[0].red, 0x11);
    assert_eq!(all[1].red, 0x44);
    assert_eq!(all[1].blue, 0x66);
}

#[test]
fn palette_change_packet_positions_track_preceding_data_packets() {
    // Two data packets, then an xxpc, then one more data packet, then
    // a second xxpc. The first delta is preceded by 2 data packets,
    // the second by 3.
    let table = color_table_bytes(&[(0, 0, 0, 0)]);
    let strf = indexed_strf(2, 2, 8, 1, &table);
    let pc = palchange_body(0, &[(1, 2, 3, 0)]);
    let dmx = open(
        AviBuilder::new(strf)
            .chunk(b"00dc", &[0u8; 4]) // data seq 0
            .chunk(b"00dc", &[0u8; 4]) // data seq 1
            .chunk(b"00pc", &pc) // first xxpc: 2 data packets precede
            .chunk(b"00dc", &[0u8; 4]) // data seq 2
            .chunk(b"00pc", &pc) // second xxpc: 3 data packets precede
            .build(),
    );

    let positions = dmx.palette_change_packet_positions(0);
    assert_eq!(positions, &[2, 3]);
}
