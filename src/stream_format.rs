//! BITMAPINFOHEADER / WAVEFORMATEX parse + emit helpers.
//!
//! These are the two standard Windows structs AVI uses in `strf` chunks:
//! - Video streams embed a `BITMAPINFOHEADER` (optionally followed by codec
//!   extradata).
//! - Audio streams embed a `WAVEFORMATEX` (or the legacy 14-byte
//!   `WAVEFORMAT`/`PCMWAVEFORMAT`) followed by `cbSize` bytes of extradata.

use oxideav_core::{Error, Result};

/// Decoded BITMAPINFOHEADER + trailing extradata.
#[derive(Clone, Debug)]
pub struct BitmapInfoHeader {
    pub width: u32,
    pub height: u32,
    pub planes: u16,
    pub bit_count: u16,
    pub compression: [u8; 4],
    pub size_image: u32,
    pub x_pels_per_meter: i32,
    pub y_pels_per_meter: i32,
    pub clr_used: u32,
    pub clr_important: u32,
    /// Extradata following the 40-byte header (palette/codec private data,
    /// or `BI_BITFIELDS` color masks for 16/32-bpp uncompressed RGB).
    pub extradata: Vec<u8>,
    /// `true` when the on-wire `biHeight` was negative, indicating a
    /// **top-down DIB** for uncompressed RGB streams per VfW
    /// `wingdi.h` §"biHeight sign rules": positive `biHeight` is
    /// bottom-up (origin lower-left), negative is top-down (origin
    /// upper-left). YUV bitmaps are always top-down regardless of
    /// sign; compressed formats MUST use positive `biHeight` per the
    /// same section, so this flag is only semantically meaningful for
    /// `BI_RGB` and `BI_BITFIELDS` streams.
    pub top_down: bool,
}

/// Parse a BITMAPINFOHEADER (+ trailing extradata) from a `strf` payload.
///
/// The header is 40 bytes; anything after is treated as codec extradata.
/// `biCompression` is returned as-is; integer IDs like BI_RGB (0) appear as
/// `[0,0,0,0]`.
pub fn parse_bitmap_info_header(buf: &[u8]) -> Result<BitmapInfoHeader> {
    if buf.len() < 40 {
        return Err(Error::invalid("AVI: BITMAPINFOHEADER too short"));
    }
    let _bi_size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let width = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as i32 as u32;
    let height_signed = i32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    // Per VfW `wingdi.h` §"biHeight sign rules": positive height ⇒
    // bottom-up DIB (origin lower-left), negative ⇒ top-down DIB (origin
    // upper-left). Take the absolute value for the public `height`
    // accessor (pixel count) and preserve the sign in `top_down` so
    // callers can re-emit the correct orientation on remux.
    let top_down = height_signed.is_negative();
    let height = height_signed.unsigned_abs();
    let planes = u16::from_le_bytes([buf[12], buf[13]]);
    let bit_count = u16::from_le_bytes([buf[14], buf[15]]);
    let mut compression = [0u8; 4];
    compression.copy_from_slice(&buf[16..20]);
    let size_image = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    let x_pels_per_meter = i32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
    let y_pels_per_meter = i32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]);
    let clr_used = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
    let clr_important = u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]);
    // Optional extradata. The 40-byte BITMAPINFOHEADER is followed by any
    // codec-private bytes the encoder chose to attach. `biSize` may either
    // equal 40 (fixed header) or include the extension bytes — either way
    // anything past byte 40 belongs to extradata.
    let extradata = if buf.len() > 40 {
        buf[40..].to_vec()
    } else {
        Vec::new()
    };
    Ok(BitmapInfoHeader {
        width,
        height,
        planes,
        bit_count,
        compression,
        size_image,
        x_pels_per_meter,
        y_pels_per_meter,
        clr_used,
        clr_important,
        extradata,
        top_down,
    })
}

/// `BI_BITFIELDS` per `wingdi.h` (compression value `3`, little-endian
/// DWORD `[3, 0, 0, 0]`). Valid for 16-bpp and 32-bpp uncompressed RGB
/// per VfW §"biCompression" — the BMIH is followed by three `DWORD`
/// color masks specifying the red/green/blue byte layout (e.g.
/// `(0xF800, 0x07E0, 0x001F)` for 16-bpp RGB565,
/// `(0x7C00, 0x03E0, 0x001F)` for 16-bpp RGB555,
/// `(0x00FF_0000, 0x0000_FF00, 0x0000_00FF)` for 32-bpp BGRA).
pub const BI_BITFIELDS: [u8; 4] = [3, 0, 0, 0];

/// Parse the three `BI_BITFIELDS` color masks from the trailing
/// extradata of a BMIH (the 12 bytes immediately after the 40-byte
/// fixed header). Returns `(red_mask, green_mask, blue_mask)`.
///
/// Per VfW `wingdi.h` §"Color tables (palettes)": when `biCompression
/// == BI_BITFIELDS`, three `DWORD` color masks follow the fixed
/// header. Returns `None` when fewer than 12 bytes are available.
///
/// Validity (which bpps `BI_BITFIELDS` is legal for) is the caller's
/// concern; the function just reads the three little-endian DWORDs.
pub fn parse_bitfields_masks(extradata: &[u8]) -> Option<(u32, u32, u32)> {
    if extradata.len() < 12 {
        return None;
    }
    let r = u32::from_le_bytes([extradata[0], extradata[1], extradata[2], extradata[3]]);
    let g = u32::from_le_bytes([extradata[4], extradata[5], extradata[6], extradata[7]]);
    let b = u32::from_le_bytes([extradata[8], extradata[9], extradata[10], extradata[11]]);
    Some((r, g, b))
}

/// Emit a 40-byte BITMAPINFOHEADER followed by optional extradata.
///
/// `biHeight` is written positive (bottom-up DIB). Call
/// [`write_bitmap_info_header_oriented`] when the caller needs to
/// signal a top-down DIB per VfW `wingdi.h` §"biHeight sign rules".
pub fn write_bitmap_info_header(
    width: u32,
    height: u32,
    compression: [u8; 4],
    bit_count: u16,
    extradata: &[u8],
) -> Vec<u8> {
    write_bitmap_info_header_oriented(width, height, compression, bit_count, extradata, false)
}

/// Same as [`write_bitmap_info_header`] but stamps a negative `biHeight`
/// when `top_down` is `true`, signalling a top-down DIB (origin at
/// upper-left) per VfW `wingdi.h` §"biHeight sign rules". Only
/// semantically meaningful for uncompressed RGB streams (`BI_RGB` or
/// `BI_BITFIELDS`); compressed formats MUST use positive `biHeight`
/// per the same section.
pub fn write_bitmap_info_header_oriented(
    width: u32,
    height: u32,
    compression: [u8; 4],
    bit_count: u16,
    extradata: &[u8],
    top_down: bool,
) -> Vec<u8> {
    let bi_size = 40u32 + extradata.len() as u32;
    let mut out = Vec::with_capacity(bi_size as usize);
    out.extend_from_slice(&bi_size.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    // Sign rule: positive ⇒ bottom-up, negative ⇒ top-down.
    let height_field: i32 = if top_down {
        -(height as i32)
    } else {
        height as i32
    };
    out.extend_from_slice(&height_field.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes = 1
    out.extend_from_slice(&bit_count.to_le_bytes());
    out.extend_from_slice(&compression);
    // size_image = width * height * bit_count / 8 (rough; 0 acceptable for
    // non-BI_RGB). Use 0 to let the decoder derive it.
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // x_pels_per_meter
    out.extend_from_slice(&0i32.to_le_bytes()); // y_pels_per_meter
    out.extend_from_slice(&0u32.to_le_bytes()); // clr_used
    out.extend_from_slice(&0u32.to_le_bytes()); // clr_important
    out.extend_from_slice(extradata);
    out
}

/// Decoded WAVEFORMATEX + extradata.
#[derive(Clone, Debug)]
pub struct WaveFormatEx {
    pub format_tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub avg_bytes_per_sec: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
    pub extradata: Vec<u8>,
}

/// Parse a WAVEFORMATEX from a `strf` payload.
///
/// The legacy 14-byte WAVEFORMAT (no `wBitsPerSample`) and the 16-byte
/// PCMWAVEFORMAT (no `cbSize`) are both accepted — missing fields default to
/// zero.
pub fn parse_waveformatex(buf: &[u8]) -> Result<WaveFormatEx> {
    if buf.len() < 14 {
        return Err(Error::invalid("AVI: WAVEFORMAT(EX) too short"));
    }
    let format_tag = u16::from_le_bytes([buf[0], buf[1]]);
    let channels = u16::from_le_bytes([buf[2], buf[3]]);
    let samples_per_sec = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let avg_bytes_per_sec = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let block_align = u16::from_le_bytes([buf[12], buf[13]]);
    let bits_per_sample = if buf.len() >= 16 {
        u16::from_le_bytes([buf[14], buf[15]])
    } else {
        0
    };
    let extradata = if buf.len() >= 18 {
        let cb_size = u16::from_le_bytes([buf[16], buf[17]]) as usize;
        if 18 + cb_size <= buf.len() {
            buf[18..18 + cb_size].to_vec()
        } else {
            buf[18..].to_vec()
        }
    } else {
        Vec::new()
    };
    Ok(WaveFormatEx {
        format_tag,
        channels,
        samples_per_sec,
        avg_bytes_per_sec,
        block_align,
        bits_per_sample,
        extradata,
    })
}

/// Emit a WAVEFORMATEX (always 18 bytes + extradata).
#[allow(clippy::too_many_arguments)]
pub fn write_waveformatex(
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    extradata: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(18 + extradata.len());
    out.extend_from_slice(&format_tag.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&samples_per_sec.to_le_bytes());
    out.extend_from_slice(&avg_bytes_per_sec.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(&(extradata.len() as u16).to_le_bytes());
    out.extend_from_slice(extradata);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmih_roundtrip() {
        let bytes = write_bitmap_info_header(320, 240, *b"MJPG", 24, &[0xAA, 0xBB]);
        assert_eq!(bytes.len(), 42);
        let h = parse_bitmap_info_header(&bytes).unwrap();
        assert_eq!(h.width, 320);
        assert_eq!(h.height, 240);
        assert_eq!(&h.compression, b"MJPG");
        assert_eq!(h.bit_count, 24);
        assert_eq!(h.extradata, vec![0xAA, 0xBB]);
        assert!(!h.top_down);
    }

    #[test]
    fn bmih_roundtrip_top_down() {
        // Negative biHeight ⇒ top-down DIB per VfW §"biHeight sign rules".
        let bytes = write_bitmap_info_header_oriented(320, 240, [0, 0, 0, 0], 24, &[], true);
        let h = parse_bitmap_info_header(&bytes).unwrap();
        assert_eq!(h.height, 240, "abs(biHeight) reported as positive pixels");
        assert!(h.top_down, "negative biHeight ⇒ top_down flag set");
        // And the on-wire i32 at offset 8..12 must encode -240.
        let on_wire = i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        assert_eq!(on_wire, -240);
    }

    #[test]
    fn bitfields_masks_rgb565() {
        // RGB565 masks per VfW §"biCompression".
        let mut ext = Vec::new();
        ext.extend_from_slice(&0x0000_F800u32.to_le_bytes()); // R
        ext.extend_from_slice(&0x0000_07E0u32.to_le_bytes()); // G
        ext.extend_from_slice(&0x0000_001Fu32.to_le_bytes()); // B
        let masks = parse_bitfields_masks(&ext).unwrap();
        assert_eq!(masks, (0xF800, 0x07E0, 0x001F));
    }

    #[test]
    fn bitfields_masks_too_short_returns_none() {
        assert!(parse_bitfields_masks(&[1u8, 2, 3]).is_none());
        assert!(parse_bitfields_masks(&[]).is_none());
    }

    #[test]
    fn wfx_roundtrip() {
        let bytes = write_waveformatex(1, 2, 48_000, 192_000, 4, 16, &[]);
        assert_eq!(bytes.len(), 18);
        let h = parse_waveformatex(&bytes).unwrap();
        assert_eq!(h.format_tag, 1);
        assert_eq!(h.channels, 2);
        assert_eq!(h.samples_per_sec, 48_000);
        assert_eq!(h.block_align, 4);
        assert_eq!(h.bits_per_sample, 16);
    }

    #[test]
    fn wfx_short_legacy() {
        // Legacy 14-byte WAVEFORMAT without bps.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0001u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        let h = parse_waveformatex(&bytes).unwrap();
        assert_eq!(h.bits_per_sample, 0);
        assert!(h.extradata.is_empty());
    }
}
