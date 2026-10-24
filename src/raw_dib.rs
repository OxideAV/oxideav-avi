//! Uncompressed (DIB / raw-FourCC) video tracks ↔ the fleet's
//! `rawvideo` codec.
//!
//! The `rawvideo` decoder takes one packet per frame holding the
//! picture's rows **top-down and tightly packed** (no stride padding),
//! with the layout named by `CodecParameters::pixel_format`. An AVI
//! uncompressed video stream stores a device-independent bitmap per
//! frame (VfW `BITMAPINFOHEADER`, `docs/container/vfw/video-for-windows.md`
//! and `docs/image/bmp/mslearn-struct-ns-wingdi-bitmapinfoheader.html`):
//!
//! * **`BI_RGB`** — 16 bpp is RGB 5-5-5 (blue in the low five bits,
//!   most significant bit unused), 24 bpp is a blue / green / red byte
//!   triplet, 32 bpp a DWORD with blue in the low byte and the high
//!   byte unused (`BITMAPV5HEADER` §`bV5BitCount`).
//! * **`BI_BITFIELDS`** — 16 / 32 bpp whose layout the three trailing
//!   red / green / blue DWORD masks declare.
//! * Both use a minimum row stride of the width in bytes rounded up to
//!   a DWORD (`((biWidth × biBitCount + 31) & ~31) >> 3`) and are
//!   bottom-up when `biHeight` is positive, top-down when negative.
//! * **`RGBA`** FourCC — 32 bpp R, G, B, A bytes, rows top-down and
//!   packed. No staged reference defines this FourCC; the layout is
//!   the one observed black-box on files ffmpeg writes and reads
//!   (`-c:v rawvideo -pix_fmt rgba`), and for FourCC formats
//!   `biHeight` is positive regardless of orientation.
//!
//! Palettised depths (1 / 4 / 8 bpp) are not mapped: `rawvideo` has no
//! out-of-band palette carriage.

use oxideav_core::PixelFormat;

/// `BI_RGB` as the on-wire `biCompression` bytes.
const BI_RGB: [u8; 4] = [0, 0, 0, 0];
/// `BI_BITFIELDS` (value 3, little-endian).
const BI_BITFIELDS: [u8; 4] = [3, 0, 0, 0];

/// Row orientation / stride class of a raw video layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowRule {
    /// DIB rules: DWORD-aligned stride, bottom-up unless `biHeight`
    /// was negative.
    Dib,
    /// Packed top-down rows (raw FourCC formats).
    PackedTopDown,
}

/// Map a `BITMAPINFOHEADER` (`biCompression`, `biBitCount`, the
/// `BI_BITFIELDS` masks when present) to the `rawvideo` pixel format.
pub(crate) fn pixel_format_for(
    compression: [u8; 4],
    bit_count: u16,
    masks: Option<(u32, u32, u32)>,
) -> Option<(PixelFormat, RowRule)> {
    let fmt = match (compression, bit_count) {
        (BI_RGB, 16) => PixelFormat::Rgb555Le,
        (BI_RGB, 24) => PixelFormat::Bgr24,
        (BI_RGB, 32) => PixelFormat::Bgrx,
        (BI_BITFIELDS, 16) => match masks? {
            (0x7C00, 0x03E0, 0x001F) => PixelFormat::Rgb555Le,
            (0xF800, 0x07E0, 0x001F) => PixelFormat::Rgb565Le,
            (0x001F, 0x03E0, 0x7C00) => PixelFormat::Bgr555Le,
            (0x001F, 0x07E0, 0xF800) => PixelFormat::Bgr565Le,
            _ => return None,
        },
        (BI_BITFIELDS, 32) => match masks? {
            (0x00FF_0000, 0x0000_FF00, 0x0000_00FF) => PixelFormat::Bgrx,
            (0x0000_00FF, 0x0000_FF00, 0x00FF_0000) => PixelFormat::Rgbx,
            (0x0000_FF00, 0x00FF_0000, 0xFF00_0000) => PixelFormat::Xrgb,
            (0xFF00_0000, 0x00FF_0000, 0x0000_FF00) => PixelFormat::Xbgr,
            _ => return None,
        },
        (c, 32) if c.eq_ignore_ascii_case(b"RGBA") => {
            return Some((PixelFormat::Rgba, RowRule::PackedTopDown));
        }
        _ => return None,
    };
    Some((fmt, RowRule::Dib))
}

/// Bytes per pixel of the packed single-plane formats mapped above.
fn bytes_per_pixel(fmt: PixelFormat) -> Option<usize> {
    Some(match fmt {
        PixelFormat::Rgb555Le
        | PixelFormat::Rgb565Le
        | PixelFormat::Bgr555Le
        | PixelFormat::Bgr565Le => 2,
        PixelFormat::Bgr24 => 3,
        PixelFormat::Bgrx
        | PixelFormat::Rgbx
        | PixelFormat::Xbgr
        | PixelFormat::Xrgb
        | PixelFormat::Rgba
        | PixelFormat::Bgra => 4,
        _ => return None,
    })
}

/// Per-stream conversion between the on-wire frame rows and the
/// `rawvideo` packet layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawLayout {
    /// Visible bytes per row (`width × bytes per pixel`).
    pub row_bytes: usize,
    /// On-wire bytes from one row start to the next.
    pub stride: usize,
    /// Picture height in rows.
    pub rows: usize,
    /// On-wire rows are bottom-up.
    pub bottom_up: bool,
}

impl RawLayout {
    /// Layout of a stream with the given format / rule / geometry.
    pub(crate) fn new(
        fmt: PixelFormat,
        rule: RowRule,
        width: u32,
        height: u32,
        top_down: bool,
    ) -> Option<Self> {
        let row_bytes = bytes_per_pixel(fmt)?.checked_mul(width as usize)?;
        let (stride, bottom_up) = match rule {
            RowRule::Dib => (row_bytes.checked_add(3)? & !3, !top_down),
            RowRule::PackedTopDown => (row_bytes, false),
        };
        Some(Self {
            row_bytes,
            stride,
            rows: height as usize,
            bottom_up,
        })
    }

    /// `true` when the on-wire bytes already are the packet layout.
    pub(crate) fn is_identity(&self) -> bool {
        !self.bottom_up && self.stride == self.row_bytes
    }

    /// On-wire frame → `rawvideo` packet (top-down, packed). Frames too
    /// short for the declared geometry are passed through untouched so
    /// the decoder reports the size mismatch.
    pub(crate) fn unpack(&self, wire: Vec<u8>) -> Vec<u8> {
        if self.is_identity() || self.rows == 0 {
            return wire;
        }
        let need = self.stride * (self.rows - 1) + self.row_bytes;
        if wire.len() < need {
            return wire;
        }
        let mut out = Vec::with_capacity(self.row_bytes * self.rows);
        for r in 0..self.rows {
            let src = if self.bottom_up { self.rows - 1 - r } else { r };
            let at = src * self.stride;
            out.extend_from_slice(&wire[at..at + self.row_bytes]);
        }
        out
    }

    /// `rawvideo` packet → on-wire frame (inverse of [`Self::unpack`]),
    /// padding each row to the stride with zero bytes.
    pub(crate) fn pack(&self, packet: &[u8]) -> Option<Vec<u8>> {
        if self.is_identity() || packet.len() != self.row_bytes * self.rows {
            return None;
        }
        let mut out = vec![0u8; self.stride * self.rows];
        for r in 0..self.rows {
            let dst = if self.bottom_up { self.rows - 1 - r } else { r };
            out[dst * self.stride..dst * self.stride + self.row_bytes]
                .copy_from_slice(&packet[r * self.row_bytes..(r + 1) * self.row_bytes]);
        }
        Some(out)
    }
}

/// Write-side inverse of [`pixel_format_for`]: the `biCompression`,
/// `biBitCount`, trailing mask bytes, and row rule an AVI stream uses
/// to carry `rawvideo` frames of `fmt`.
pub(crate) fn header_for(fmt: PixelFormat) -> Option<([u8; 4], u16, Vec<u8>, RowRule)> {
    let masks = |r: u32, g: u32, b: u32| {
        let mut v = Vec::with_capacity(12);
        v.extend_from_slice(&r.to_le_bytes());
        v.extend_from_slice(&g.to_le_bytes());
        v.extend_from_slice(&b.to_le_bytes());
        v
    };
    Some(match fmt {
        PixelFormat::Rgb555Le => (BI_RGB, 16, Vec::new(), RowRule::Dib),
        PixelFormat::Bgr24 => (BI_RGB, 24, Vec::new(), RowRule::Dib),
        PixelFormat::Bgrx => (BI_RGB, 32, Vec::new(), RowRule::Dib),
        PixelFormat::Rgb565Le => (
            BI_BITFIELDS,
            16,
            masks(0xF800, 0x07E0, 0x001F),
            RowRule::Dib,
        ),
        PixelFormat::Bgr555Le => (
            BI_BITFIELDS,
            16,
            masks(0x001F, 0x03E0, 0x7C00),
            RowRule::Dib,
        ),
        PixelFormat::Bgr565Le => (
            BI_BITFIELDS,
            16,
            masks(0x001F, 0x07E0, 0xF800),
            RowRule::Dib,
        ),
        PixelFormat::Rgbx => (
            BI_BITFIELDS,
            32,
            masks(0x0000_00FF, 0x0000_FF00, 0x00FF_0000),
            RowRule::Dib,
        ),
        PixelFormat::Xrgb => (
            BI_BITFIELDS,
            32,
            masks(0x0000_FF00, 0x00FF_0000, 0xFF00_0000),
            RowRule::Dib,
        ),
        PixelFormat::Xbgr => (
            BI_BITFIELDS,
            32,
            masks(0xFF00_0000, 0x00FF_0000, 0x0000_FF00),
            RowRule::Dib,
        ),
        PixelFormat::Rgba => (*b"RGBA", 32, Vec::new(), RowRule::PackedTopDown),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bi_rgb_depths() {
        assert_eq!(
            pixel_format_for(BI_RGB, 24, None),
            Some((PixelFormat::Bgr24, RowRule::Dib))
        );
        assert_eq!(
            pixel_format_for(BI_RGB, 32, None),
            Some((PixelFormat::Bgrx, RowRule::Dib))
        );
        assert_eq!(
            pixel_format_for(BI_RGB, 16, None),
            Some((PixelFormat::Rgb555Le, RowRule::Dib))
        );
        assert_eq!(pixel_format_for(BI_RGB, 8, None), None);
        assert_eq!(pixel_format_for(*b"MJPG", 24, None), None);
    }

    #[test]
    fn bitfields_masks() {
        let f = |m| pixel_format_for(BI_BITFIELDS, 16, Some(m)).map(|x| x.0);
        assert_eq!(f((0xF800, 0x07E0, 0x001F)), Some(PixelFormat::Rgb565Le));
        assert_eq!(f((0x7C00, 0x03E0, 0x001F)), Some(PixelFormat::Rgb555Le));
        assert_eq!(f((0xF000, 0x0F00, 0x00F0)), None);
        assert_eq!(pixel_format_for(BI_BITFIELDS, 16, None), None);
        let g = |m| pixel_format_for(BI_BITFIELDS, 32, Some(m)).map(|x| x.0);
        assert_eq!(g((0xFF_0000, 0xFF00, 0xFF)), Some(PixelFormat::Bgrx));
        assert_eq!(g((0xFF, 0xFF00, 0xFF_0000)), Some(PixelFormat::Rgbx));
    }

    #[test]
    fn rgba_fourcc_is_packed_top_down() {
        assert_eq!(
            pixel_format_for(*b"RGBA", 32, None),
            Some((PixelFormat::Rgba, RowRule::PackedTopDown))
        );
        let l = RawLayout::new(PixelFormat::Rgba, RowRule::PackedTopDown, 3, 2, false).unwrap();
        assert!(l.is_identity());
    }

    #[test]
    fn header_round_trips_through_mapping() {
        for fmt in [
            PixelFormat::Rgb555Le,
            PixelFormat::Rgb565Le,
            PixelFormat::Bgr555Le,
            PixelFormat::Bgr565Le,
            PixelFormat::Bgr24,
            PixelFormat::Bgrx,
            PixelFormat::Rgbx,
            PixelFormat::Xrgb,
            PixelFormat::Xbgr,
            PixelFormat::Rgba,
        ] {
            let (c, bits, extra, rule) = header_for(fmt).unwrap();
            let masks = (extra.len() == 12).then(|| {
                let d = |i: usize| u32::from_le_bytes(extra[i..i + 4].try_into().unwrap());
                (d(0), d(4), d(8))
            });
            assert_eq!(
                pixel_format_for(c, bits, masks),
                Some((fmt, rule)),
                "{fmt:?}"
            );
        }
    }

    #[test]
    fn bottom_up_padded_rows_are_flipped_and_unpadded() {
        // 1×3 BGR24, bottom-up: row bytes 3, stride 4.
        let l = RawLayout::new(PixelFormat::Bgr24, RowRule::Dib, 1, 3, false).unwrap();
        assert_eq!((l.row_bytes, l.stride, l.bottom_up), (3, 4, true));
        let wire = vec![7, 7, 7, 0, 8, 8, 8, 0, 9, 9, 9, 0];
        let pkt = l.unpack(wire.clone());
        assert_eq!(pkt, [9, 9, 9, 8, 8, 8, 7, 7, 7]);
        assert_eq!(l.pack(&pkt).unwrap(), wire);
    }

    #[test]
    fn top_down_aligned_rows_are_identity() {
        let l = RawLayout::new(PixelFormat::Bgr24, RowRule::Dib, 4, 2, true).unwrap();
        assert!(l.is_identity());
        let wire: Vec<u8> = (0..24).collect();
        assert_eq!(l.unpack(wire.clone()), wire);
        assert_eq!(l.pack(&wire), None);
    }

    #[test]
    fn short_frame_passes_through() {
        let l = RawLayout::new(PixelFormat::Bgr24, RowRule::Dib, 1, 3, false).unwrap();
        assert_eq!(l.unpack(vec![1, 2, 3]), [1, 2, 3]);
    }
}
