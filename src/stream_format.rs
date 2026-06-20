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

/// One entry of a DIB color table — `RGBQUAD` per the RIFF MCI
/// reference (`docs/container/riff/metadata/microsoft-riffmci.pdf`
/// §"Color Table Structure"):
///
/// ```text
/// typedef struct tagRGBQUAD {
///   BYTE rgbBlue;
///   BYTE rgbGreen;
///   BYTE rgbRed;
///   BYTE rgbReserved;   // "Not used. Must be set to 0."
/// } RGBQUAD;
/// ```
///
/// The on-disk byte order is blue / green / red / reserved (note the
/// inverted channel order versus the per-entry `PALETTEENTRY` of an
/// `xxpc` palette-change chunk, which is red / green / blue / flags).
/// The four bytes are surfaced verbatim — `reserved` is preserved
/// even though the spec pins it to `0`, so a non-conformant writer's
/// stray byte stays observable and the table round-trips exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RgbQuad {
    /// `rgbBlue` — blue intensity.
    pub blue: u8,
    /// `rgbGreen` — green intensity.
    pub green: u8,
    /// `rgbRed` — red intensity.
    pub red: u8,
    /// `rgbReserved` — "Not used. Must be set to 0." per the spec;
    /// surfaced verbatim for round-trip parity.
    pub reserved: u8,
}

/// Parse the DIB color table (`RGBQUAD bmiColors[]`) that follows the
/// 40-byte BITMAPINFOHEADER inside a `strf` payload, for an
/// indexed-colour video stream.
///
/// Per the RIFF MCI reference
/// (`docs/container/riff/metadata/microsoft-riffmci.pdf`
/// §"Bitmap Color Table" / §"Interpreting the Color Table"): the color
/// table "isn't present for bitmaps with 24 color bits"; for a
/// `biBitCount` of 1 / 4 / 8 the bitmap is indexed and the table holds
/// up to `2 ^ biBitCount` `RGBQUAD` entries. The number of entries
/// "actually used" is `biClrUsed`, and "if the biClrUsed field is set
/// to 0, the bitmap uses the maximum number of colors corresponding to
/// the value of the [biBitCount] field" — i.e. `1 << biBitCount`.
///
/// `bit_count` is the BMIH `biBitCount`; `clr_used` is `biClrUsed`;
/// `extradata` is the strf bytes following the 40-byte fixed header
/// (the same slice surfaced as [`BitmapInfoHeader::extradata`]).
///
/// Returns `None` for:
/// - `bit_count == 0` or `bit_count > 8` (no palette: 16/24/32-bpp
///   truecolour DIBs and the "unspecified depth" `0` carry no table;
///   the spec's §"Interpreting the Color Table" only enumerates the
///   indexed depths 1 / 4 / 8 as palettised),
/// - a resolved entry count of `0`,
/// - extradata too short to hold even one `RGBQUAD`.
///
/// When the extradata is shorter than the resolved `count * 4` bytes
/// (a truncated / hand-edited table) only the complete `RGBQUAD`s the
/// buffer physically contains are returned, so a short table stays
/// usable for the colours it does carry rather than being dropped
/// wholesale.
pub fn parse_color_table(bit_count: u16, clr_used: u32, extradata: &[u8]) -> Option<Vec<RgbQuad>> {
    // The color table exists only for indexed DIBs (1/4/8-bpp). A
    // `biBitCount` of 0 is the "depth unspecified / carried elsewhere"
    // sentinel; 16/24/32-bpp are truecolour. The spec's §"Interpreting
    // the Color Table" only enumerates 1 / 4 / 8 as palettised, so any
    // depth above 8 carries no table.
    if bit_count == 0 || bit_count > 8 {
        return None;
    }
    // Resolved entry count: `biClrUsed`, or the depth maximum when 0.
    // `1 << bit_count` is safe for the 1/4/8 range this arm reaches
    // (max 256). A pathological `biClrUsed` larger than the depth
    // maximum is honoured verbatim — some writers over-declare — and
    // capped only by what the extradata physically holds below.
    let max_entries = 1u32 << bit_count;
    let declared = if clr_used == 0 { max_entries } else { clr_used };
    if declared == 0 {
        return None;
    }
    let available = (extradata.len() / 4) as u32;
    let count = declared.min(available);
    if count == 0 {
        return None;
    }
    let mut table = Vec::with_capacity(count as usize);
    for quad in extradata.chunks_exact(4).take(count as usize) {
        table.push(RgbQuad {
            blue: quad[0],
            green: quad[1],
            red: quad[2],
            reserved: quad[3],
        });
    }
    Some(table)
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

// --- WAVEFORMATEXTENSIBLE -------------------------------------------------
//
// Per docs/container/riff/waveformatextensible/README.md (Microsoft Learn
// mirror, 2026-05-18) and Microsoft `mmreg.h` § "WAVEFORMATEXTENSIBLE": when
// a WAVEFORMATEX's `wFormatTag` equals [`WAVE_FORMAT_EXTENSIBLE`] (`0xFFFE`),
// the 22-byte extension that follows the standard 18-byte WAVEFORMATEX
// (and is gated by `cbSize == 22`) carries three additional fields:
//
//   union { WORD wValidBitsPerSample; WORD wSamplesPerBlock; WORD wReserved; }
//                                          // 2 bytes — `Samples` union
//   DWORD dwChannelMask;                   // 4 bytes — SPEAKER_* bitmap
//   GUID  SubFormat;                       // 16 bytes — codec identifier
//
// Total: 18 (WAVEFORMATEX) + 22 (extension) = 40 bytes on the wire. The
// canonical use-cases per Microsoft Learn § "Extensible Wave-Format
// Descriptors" — surround / float / >16-bit / 24-in-32 container — all
// require this struct over the legacy WAVEFORMATEX.

/// `WAVE_FORMAT_EXTENSIBLE` per Microsoft `mmreg.h` — the escape-hatch
/// `wFormatTag` (`0xFFFE`) that signals the trailing 22-byte
/// [`WaveFormatExtensible`] extension carries the real codec identity
/// in its `SubFormat` GUID field.
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// 128-bit GUID. Microsoft `KSDATAFORMAT_SUBTYPE_*` constants and the
/// `SubFormat` field of [`WaveFormatExtensible`] use this shape on the
/// wire: 4-byte `Data1` (LE) + 2-byte `Data2` (LE) + 2-byte `Data3`
/// (LE) + 8 raw bytes (`Data4`) — i.e. mixed-endian, the standard
/// Microsoft GUID layout. Stored as the 16 wire bytes verbatim so
/// equality, hashing, and round-trip serialisation are byte-exact;
/// the [`Guid::display`] method emits the canonical
/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// Build a GUID from the canonical Microsoft hex-text components.
    /// `data1` / `data2` / `data3` are little-endian on the wire (as
    /// per Microsoft `guiddef.h`); `data4` is the raw 8-byte tail.
    pub const fn from_components(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        let d1 = data1.to_le_bytes();
        let d2 = data2.to_le_bytes();
        let d3 = data3.to_le_bytes();
        let bytes = [
            d1[0], d1[1], d1[2], d1[3], d2[0], d2[1], d3[0], d3[1], data4[0], data4[1], data4[2],
            data4[3], data4[4], data4[5], data4[6], data4[7],
        ];
        Self(bytes)
    }

    /// Read a GUID from a 16-byte slice. Returns `None` on shorter input.
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < 16 {
            return None;
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&buf[..16]);
        Some(Self(out))
    }

    /// Canonical `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` string. Data1/2/3
    /// are printed in their natural integer order (read back from LE).
    pub fn display(&self) -> String {
        let d1 = u32::from_le_bytes([self.0[0], self.0[1], self.0[2], self.0[3]]);
        let d2 = u16::from_le_bytes([self.0[4], self.0[5]]);
        let d3 = u16::from_le_bytes([self.0[6], self.0[7]]);
        format!(
            "{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.0[8],
            self.0[9],
            self.0[10],
            self.0[11],
            self.0[12],
            self.0[13],
            self.0[14],
            self.0[15],
        )
    }

    /// True when this GUID follows the Microsoft `KSDATAFORMAT` namespace
    /// pattern `XXXXXXXX-0000-0010-8000-00AA00389B71` per the docs
    /// reference table — the trailing 12 bytes are the canonical
    /// "DataFormat" base GUID, and the leading 4-byte `Data1` field is
    /// the legacy [`WaveFormatEx::format_tag`] value. Microsoft-defined
    /// extensions (e.g. KSDATAFORMAT_SUBTYPE_AC3_AUDIO) do NOT follow
    /// this pattern; see the docs README's "Other KSDATAFORMAT_SUBTYPE_*
    /// GUIDs do not follow this pattern" note.
    pub fn is_ksdataformat_base(&self) -> bool {
        // `00000000-0000-0010-8000-00aa00389b71` tail (bytes 4..16).
        const KS_TAIL: [u8; 12] = [
            0x00, 0x00, // Data2
            0x10, 0x00, // Data3
            0x80, 0x00, 0x00, 0xAA, // Data4[0..4]
            0x00, 0x38, 0x9B, 0x71, // Data4[4..8]
        ];
        self.0[4..16] == KS_TAIL
    }

    /// Recover the legacy [`WaveFormatEx::format_tag`] when this GUID
    /// follows the `KSDATAFORMAT` namespace base pattern (see
    /// [`Self::is_ksdataformat_base`]); `None` for unrelated GUIDs.
    /// The legacy tag lives in `Data1` (LE u32 on the wire); only the
    /// low 16 bits are used (the 17-bit space WAVEFORMATEX uses).
    pub fn ksdataformat_tag(&self) -> Option<u16> {
        if !self.is_ksdataformat_base() {
            return None;
        }
        let d1 = u32::from_le_bytes([self.0[0], self.0[1], self.0[2], self.0[3]]);
        if d1 > u16::MAX as u32 {
            return None;
        }
        Some(d1 as u16)
    }
}

/// `KSDATAFORMAT_SUBTYPE_PCM` per Microsoft `KSMedia.h`. Per the docs
/// table, equivalent to `WAVE_FORMAT_PCM (0x0001)` in the legacy
/// `wFormatTag` namespace.
pub const KSDATAFORMAT_SUBTYPE_PCM: Guid = Guid::from_components(
    0x0000_0001,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` per `KSMedia.h`. Equivalent to
/// `WAVE_FORMAT_IEEE_FLOAT (0x0003)`.
pub const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: Guid = Guid::from_components(
    0x0000_0003,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// `KSDATAFORMAT_SUBTYPE_DRM` per `KSMedia.h`. Equivalent to
/// `WAVE_FORMAT_DRM (0x0009)`.
pub const KSDATAFORMAT_SUBTYPE_DRM: Guid = Guid::from_components(
    0x0000_0009,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// `KSDATAFORMAT_SUBTYPE_ALAW` per `KSMedia.h`. Equivalent to
/// `WAVE_FORMAT_ALAW (0x0006)`.
pub const KSDATAFORMAT_SUBTYPE_ALAW: Guid = Guid::from_components(
    0x0000_0006,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// `KSDATAFORMAT_SUBTYPE_MULAW` per `KSMedia.h`. Equivalent to
/// `WAVE_FORMAT_MULAW (0x0007)`.
pub const KSDATAFORMAT_SUBTYPE_MULAW: Guid = Guid::from_components(
    0x0000_0007,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// `KSDATAFORMAT_SUBTYPE_ADPCM` per `KSMedia.h`. Equivalent to
/// `WAVE_FORMAT_ADPCM (0x0002)`.
pub const KSDATAFORMAT_SUBTYPE_ADPCM: Guid = Guid::from_components(
    0x0000_0002,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// `KSDATAFORMAT_SUBTYPE_MPEG` per `KSMedia.h`. Equivalent to
/// `WAVE_FORMAT_MPEG (0x0050)`.
pub const KSDATAFORMAT_SUBTYPE_MPEG: Guid = Guid::from_components(
    0x0000_0050,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

// --- WAVEFORMATEXTENSIBLE channel-mask SPEAKER_* bitmap -------------------
//
// Round 163: typed `dwChannelMask` surface (`Speaker` + [`ChannelMask`] +
// [`ChannelLayout`]). The bit assignments mirror the Microsoft Learn
// "Channel-mask channel ordering" section verbatim from
// `docs/container/riff/waveformatextensible/README.md` (2026-05-18). The
// 18 standard `SPEAKER_*` bits run from `FL = 0x00001` up through `TBR =
// 0x20000`; the `SPEAKER_ALL = 0x80000000` flag is a separate top-bit
// catch-all. Channels are laid out in the PCM byte stream in the bit
// order of the mask (lowest set bit first) — driving the
// [`ChannelMask::iter_speakers`] iteration order.

/// `SPEAKER_FRONT_LEFT` (`0x00001`) per Microsoft Learn "Channel-mask
/// channel ordering". Front-left full-range loudspeaker.
pub const SPEAKER_FRONT_LEFT: u32 = 0x0000_0001;
/// `SPEAKER_FRONT_RIGHT` (`0x00002`).
pub const SPEAKER_FRONT_RIGHT: u32 = 0x0000_0002;
/// `SPEAKER_FRONT_CENTER` (`0x00004`).
pub const SPEAKER_FRONT_CENTER: u32 = 0x0000_0004;
/// `SPEAKER_LOW_FREQUENCY` (`0x00008`) — subwoofer.
pub const SPEAKER_LOW_FREQUENCY: u32 = 0x0000_0008;
/// `SPEAKER_BACK_LEFT` (`0x00010`).
pub const SPEAKER_BACK_LEFT: u32 = 0x0000_0010;
/// `SPEAKER_BACK_RIGHT` (`0x00020`).
pub const SPEAKER_BACK_RIGHT: u32 = 0x0000_0020;
/// `SPEAKER_FRONT_LEFT_OF_CENTER` (`0x00040`).
pub const SPEAKER_FRONT_LEFT_OF_CENTER: u32 = 0x0000_0040;
/// `SPEAKER_FRONT_RIGHT_OF_CENTER` (`0x00080`).
pub const SPEAKER_FRONT_RIGHT_OF_CENTER: u32 = 0x0000_0080;
/// `SPEAKER_BACK_CENTER` (`0x00100`).
pub const SPEAKER_BACK_CENTER: u32 = 0x0000_0100;
/// `SPEAKER_SIDE_LEFT` (`0x00200`).
pub const SPEAKER_SIDE_LEFT: u32 = 0x0000_0200;
/// `SPEAKER_SIDE_RIGHT` (`0x00400`).
pub const SPEAKER_SIDE_RIGHT: u32 = 0x0000_0400;
/// `SPEAKER_TOP_CENTER` (`0x00800`).
pub const SPEAKER_TOP_CENTER: u32 = 0x0000_0800;
/// `SPEAKER_TOP_FRONT_LEFT` (`0x01000`).
pub const SPEAKER_TOP_FRONT_LEFT: u32 = 0x0000_1000;
/// `SPEAKER_TOP_FRONT_CENTER` (`0x02000`).
pub const SPEAKER_TOP_FRONT_CENTER: u32 = 0x0000_2000;
/// `SPEAKER_TOP_FRONT_RIGHT` (`0x04000`).
pub const SPEAKER_TOP_FRONT_RIGHT: u32 = 0x0000_4000;
/// `SPEAKER_TOP_BACK_LEFT` (`0x08000`).
pub const SPEAKER_TOP_BACK_LEFT: u32 = 0x0000_8000;
/// `SPEAKER_TOP_BACK_CENTER` (`0x10000`).
pub const SPEAKER_TOP_BACK_CENTER: u32 = 0x0001_0000;
/// `SPEAKER_TOP_BACK_RIGHT` (`0x20000`).
pub const SPEAKER_TOP_BACK_RIGHT: u32 = 0x0002_0000;
/// `SPEAKER_ALL` (`0x80000000`) — top-bit "all speakers" catch-all
/// per Microsoft Learn § "Extensible Wave-Format Descriptors".
pub const SPEAKER_ALL: u32 = 0x8000_0000;

/// One named `SPEAKER_*` position from the
/// `WAVEFORMATEXTENSIBLE.dwChannelMask` bitmap (round 163).
///
/// The variants are listed in the same bit order as the docs README
/// table at `docs/container/riff/waveformatextensible/README.md`,
/// which is the PCM byte-stream channel order. [`ChannelMask`] iterates
/// `Speaker`s in this exact order so callers can pair the iteration
/// with their per-channel buffer indices.
///
/// `SpeakerAll` represents `SPEAKER_ALL (0x80000000)` — a catch-all
/// top bit Microsoft uses to mean "feed every speaker the same mono
/// channel". It is listed separately from the 18 positional bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Speaker {
    /// `SPEAKER_FRONT_LEFT` — bit `0x00001`.
    FrontLeft,
    /// `SPEAKER_FRONT_RIGHT` — bit `0x00002`.
    FrontRight,
    /// `SPEAKER_FRONT_CENTER` — bit `0x00004`.
    FrontCenter,
    /// `SPEAKER_LOW_FREQUENCY` — bit `0x00008` (LFE / subwoofer).
    LowFrequency,
    /// `SPEAKER_BACK_LEFT` — bit `0x00010`.
    BackLeft,
    /// `SPEAKER_BACK_RIGHT` — bit `0x00020`.
    BackRight,
    /// `SPEAKER_FRONT_LEFT_OF_CENTER` — bit `0x00040`.
    FrontLeftOfCenter,
    /// `SPEAKER_FRONT_RIGHT_OF_CENTER` — bit `0x00080`.
    FrontRightOfCenter,
    /// `SPEAKER_BACK_CENTER` — bit `0x00100`.
    BackCenter,
    /// `SPEAKER_SIDE_LEFT` — bit `0x00200`.
    SideLeft,
    /// `SPEAKER_SIDE_RIGHT` — bit `0x00400`.
    SideRight,
    /// `SPEAKER_TOP_CENTER` — bit `0x00800`.
    TopCenter,
    /// `SPEAKER_TOP_FRONT_LEFT` — bit `0x01000`.
    TopFrontLeft,
    /// `SPEAKER_TOP_FRONT_CENTER` — bit `0x02000`.
    TopFrontCenter,
    /// `SPEAKER_TOP_FRONT_RIGHT` — bit `0x04000`.
    TopFrontRight,
    /// `SPEAKER_TOP_BACK_LEFT` — bit `0x08000`.
    TopBackLeft,
    /// `SPEAKER_TOP_BACK_CENTER` — bit `0x10000`.
    TopBackCenter,
    /// `SPEAKER_TOP_BACK_RIGHT` — bit `0x20000`.
    TopBackRight,
    /// `SPEAKER_ALL` — bit `0x80000000`. Catch-all "feed every speaker"
    /// flag per Microsoft Learn § "Extensible Wave-Format Descriptors".
    SpeakerAll,
}

impl Speaker {
    /// Underlying `SPEAKER_*` bit mask for this position.
    pub const fn mask_bit(self) -> u32 {
        match self {
            Self::FrontLeft => SPEAKER_FRONT_LEFT,
            Self::FrontRight => SPEAKER_FRONT_RIGHT,
            Self::FrontCenter => SPEAKER_FRONT_CENTER,
            Self::LowFrequency => SPEAKER_LOW_FREQUENCY,
            Self::BackLeft => SPEAKER_BACK_LEFT,
            Self::BackRight => SPEAKER_BACK_RIGHT,
            Self::FrontLeftOfCenter => SPEAKER_FRONT_LEFT_OF_CENTER,
            Self::FrontRightOfCenter => SPEAKER_FRONT_RIGHT_OF_CENTER,
            Self::BackCenter => SPEAKER_BACK_CENTER,
            Self::SideLeft => SPEAKER_SIDE_LEFT,
            Self::SideRight => SPEAKER_SIDE_RIGHT,
            Self::TopCenter => SPEAKER_TOP_CENTER,
            Self::TopFrontLeft => SPEAKER_TOP_FRONT_LEFT,
            Self::TopFrontCenter => SPEAKER_TOP_FRONT_CENTER,
            Self::TopFrontRight => SPEAKER_TOP_FRONT_RIGHT,
            Self::TopBackLeft => SPEAKER_TOP_BACK_LEFT,
            Self::TopBackCenter => SPEAKER_TOP_BACK_CENTER,
            Self::TopBackRight => SPEAKER_TOP_BACK_RIGHT,
            Self::SpeakerAll => SPEAKER_ALL,
        }
    }

    /// Short docs-table abbreviation used in the
    /// `docs/container/riff/waveformatextensible/README.md` channel
    /// ordering table (e.g. `"FL"`, `"FR"`, `"FC"`, `"LFE"`, `"BL"`,
    /// `"BR"`, `"SL"`, `"SR"`). `SpeakerAll` returns `"ALL"`.
    pub const fn abbrev(self) -> &'static str {
        match self {
            Self::FrontLeft => "FL",
            Self::FrontRight => "FR",
            Self::FrontCenter => "FC",
            Self::LowFrequency => "LFE",
            Self::BackLeft => "BL",
            Self::BackRight => "BR",
            Self::FrontLeftOfCenter => "FLC",
            Self::FrontRightOfCenter => "FRC",
            Self::BackCenter => "BC",
            Self::SideLeft => "SL",
            Self::SideRight => "SR",
            Self::TopCenter => "TC",
            Self::TopFrontLeft => "TFL",
            Self::TopFrontCenter => "TFC",
            Self::TopFrontRight => "TFR",
            Self::TopBackLeft => "TBL",
            Self::TopBackCenter => "TBC",
            Self::TopBackRight => "TBR",
            Self::SpeakerAll => "ALL",
        }
    }
}

// Bit-order table — Microsoft Learn lists the 18 positional `SPEAKER_*`
// bits from `0x00001` through `0x20000`. `SPEAKER_ALL` (`0x80000000`)
// is appended at the tail because it's a non-positional catch-all.
const SPEAKER_BIT_ORDER: [Speaker; 19] = [
    Speaker::FrontLeft,
    Speaker::FrontRight,
    Speaker::FrontCenter,
    Speaker::LowFrequency,
    Speaker::BackLeft,
    Speaker::BackRight,
    Speaker::FrontLeftOfCenter,
    Speaker::FrontRightOfCenter,
    Speaker::BackCenter,
    Speaker::SideLeft,
    Speaker::SideRight,
    Speaker::TopCenter,
    Speaker::TopFrontLeft,
    Speaker::TopFrontCenter,
    Speaker::TopFrontRight,
    Speaker::TopBackLeft,
    Speaker::TopBackCenter,
    Speaker::TopBackRight,
    Speaker::SpeakerAll,
];

/// Typed view of a `WAVEFORMATEXTENSIBLE.dwChannelMask` 32-bit value
/// (round 163).
///
/// Wraps the raw `u32` returned by
/// [`crate::demuxer::AviDemuxer::stream_channel_mask`] / stored on
/// [`WaveFormatExtensible::channel_mask`] and exposes the
/// `SPEAKER_*` decode without forcing callers to write the bit
/// arithmetic themselves. Iteration order matches the PCM byte-stream
/// channel order per Microsoft Learn § "Channel-mask channel
/// ordering" (lowest set bit first).
///
/// Source: `docs/container/riff/waveformatextensible/README.md`
/// (Microsoft Learn mirror, 2026-05-18). Bit values verified against
/// the same docs table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelMask(pub u32);

impl ChannelMask {
    /// Wrap a raw `dwChannelMask` value.
    pub const fn from_raw(mask: u32) -> Self {
        Self(mask)
    }

    /// Raw `dwChannelMask` value back out.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Iterate the [`Speaker`] positions present in this mask, in the
    /// PCM byte-stream channel order (lowest set bit first per
    /// Microsoft Learn § "Channel-mask channel ordering").
    ///
    /// Unrecognised bits — Microsoft reserves the gap between
    /// `SPEAKER_TOP_BACK_RIGHT (0x20000)` and `SPEAKER_ALL (0x80000000)`
    /// for `SPEAKER_RESERVED` — are silently skipped. Use
    /// [`Self::reserved_bits`] to inspect them.
    pub fn iter_speakers(self) -> impl Iterator<Item = Speaker> {
        let mask = self.0;
        SPEAKER_BIT_ORDER
            .iter()
            .copied()
            .filter(move |sp| mask & sp.mask_bit() != 0)
    }

    /// Count of recognised `SPEAKER_*` bits present (i.e. number of
    /// items [`Self::iter_speakers`] would yield). This is also the
    /// number of audio channels the docs README's "channel ordering"
    /// table associates with this mask.
    pub fn len(self) -> u32 {
        // Population count over the documented bits only — Microsoft's
        // `SPEAKER_RESERVED` range is excluded so a corrupt / unknown
        // bit doesn't inflate the channel count.
        let mut count = 0u32;
        for sp in SPEAKER_BIT_ORDER {
            if self.0 & sp.mask_bit() != 0 {
                count += 1;
            }
        }
        count
    }

    /// `true` when no `SPEAKER_*` bits are set (`dwChannelMask == 0`).
    /// Microsoft Learn treats a zero mask as "speaker assignment
    /// unknown / unspecified".
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Bits set in `dwChannelMask` that do NOT correspond to one of the
    /// 18 documented positional `SPEAKER_*` bits or `SPEAKER_ALL` — i.e.
    /// the Microsoft `SPEAKER_RESERVED` range. Returned as a raw `u32`
    /// (the same bits, with documented bits cleared) so callers can
    /// surface them as opaque metadata without losing information.
    pub fn reserved_bits(self) -> u32 {
        let mut known = 0u32;
        for sp in SPEAKER_BIT_ORDER {
            known |= sp.mask_bit();
        }
        self.0 & !known
    }

    /// Recognise one of the named layouts from the docs README's
    /// "Standard layouts" table (round 163). Returns `None` for any
    /// non-standard combination — the caller can still consume
    /// [`Self::iter_speakers`] for the raw decode.
    ///
    /// The seven entries match the docs README verbatim:
    /// Mono / Stereo / 2.1 / Quad / 5.1 (Microsoft back) / 5.1 (DVD
    /// side) / 7.1.
    pub fn layout(self) -> Option<ChannelLayout> {
        ChannelLayout::from_mask(self.0)
    }
}

/// Named multi-channel layout matching one of the rows of the
/// "Standard layouts" table in
/// `docs/container/riff/waveformatextensible/README.md` (round 163).
///
/// The variants intentionally distinguish "Microsoft 5.1" (with rear
/// `BL`/`BR`) from "DVD-style 5.1" (with side `SL`/`SR`) since both
/// are equally common in the wild and the per-bit channel order
/// differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChannelLayout {
    /// `SPEAKER_FRONT_CENTER` — 1 channel.
    Mono,
    /// `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT` — 2 channels.
    Stereo,
    /// `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT |
    /// SPEAKER_LOW_FREQUENCY` — 3 channels.
    TwoPointOne,
    /// `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT | SPEAKER_BACK_LEFT |
    /// SPEAKER_BACK_RIGHT` — 4 channels.
    Quad,
    /// Microsoft 5.1: `FL | FR | FC | LFE | BL | BR` — 6 channels.
    FivePointOneBack,
    /// DVD-style 5.1: `FL | FR | FC | LFE | SL | SR` — 6 channels.
    FivePointOneSide,
    /// 7.1: `FL | FR | FC | LFE | BL | BR | SL | SR` — 8 channels.
    SevenPointOne,
}

impl ChannelLayout {
    /// The exact `dwChannelMask` value matching this named layout per
    /// the docs README table.
    pub const fn mask(self) -> u32 {
        match self {
            Self::Mono => SPEAKER_FRONT_CENTER,
            Self::Stereo => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            Self::TwoPointOne => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT | SPEAKER_LOW_FREQUENCY,
            Self::Quad => {
                SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT | SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT
            }
            Self::FivePointOneBack => {
                SPEAKER_FRONT_LEFT
                    | SPEAKER_FRONT_RIGHT
                    | SPEAKER_FRONT_CENTER
                    | SPEAKER_LOW_FREQUENCY
                    | SPEAKER_BACK_LEFT
                    | SPEAKER_BACK_RIGHT
            }
            Self::FivePointOneSide => {
                SPEAKER_FRONT_LEFT
                    | SPEAKER_FRONT_RIGHT
                    | SPEAKER_FRONT_CENTER
                    | SPEAKER_LOW_FREQUENCY
                    | SPEAKER_SIDE_LEFT
                    | SPEAKER_SIDE_RIGHT
            }
            Self::SevenPointOne => {
                SPEAKER_FRONT_LEFT
                    | SPEAKER_FRONT_RIGHT
                    | SPEAKER_FRONT_CENTER
                    | SPEAKER_LOW_FREQUENCY
                    | SPEAKER_BACK_LEFT
                    | SPEAKER_BACK_RIGHT
                    | SPEAKER_SIDE_LEFT
                    | SPEAKER_SIDE_RIGHT
            }
        }
    }

    /// Resolve a raw `dwChannelMask` to a named layout, or `None`. Bits
    /// outside the 18 documented positional `SPEAKER_*` bits and
    /// `SPEAKER_ALL` are ignored for matching purposes (a stream with
    /// stereo + a stray reserved bit still classifies as
    /// [`ChannelLayout::Stereo`]). Use [`ChannelMask::reserved_bits`]
    /// if the caller wants to detect that situation explicitly.
    pub fn from_mask(mask: u32) -> Option<Self> {
        // Strip any non-documented bits before equality testing.
        let mut known = 0u32;
        for sp in SPEAKER_BIT_ORDER {
            known |= sp.mask_bit();
        }
        let cleaned = mask & known;
        [
            Self::Mono,
            Self::Stereo,
            Self::TwoPointOne,
            Self::Quad,
            Self::FivePointOneBack,
            Self::FivePointOneSide,
            Self::SevenPointOne,
        ]
        .into_iter()
        .find(|layout| cleaned == layout.mask())
    }

    /// Short docs-table label (e.g. `"mono"`, `"stereo"`, `"5.1"`,
    /// `"7.1"`). The two 5.1 variants disambiguate as `"5.1(back)"`
    /// (Microsoft) and `"5.1(side)"` (DVD-style).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Stereo => "stereo",
            Self::TwoPointOne => "2.1",
            Self::Quad => "quad",
            Self::FivePointOneBack => "5.1(back)",
            Self::FivePointOneSide => "5.1(side)",
            Self::SevenPointOne => "7.1",
        }
    }
}

/// Decoded WAVEFORMATEXTENSIBLE.
///
/// `wfx.format_tag` is always [`WAVE_FORMAT_EXTENSIBLE`] (`0xFFFE`) for
/// a well-formed extensible struct; the codec's "real" identity lives
/// in [`Self::subformat`]. `wfx.extradata` is consumed by the parser
/// for the 22-byte extension and surfaces here as
/// `(valid_bits_per_sample, channel_mask, subformat)`.
#[derive(Clone, Debug)]
pub struct WaveFormatExtensible {
    /// Underlying WAVEFORMATEX (always with `format_tag == 0xFFFE`).
    pub wfx: WaveFormatEx,
    /// `Samples.wValidBitsPerSample` — actual bit precision per sample.
    /// May be lower than `wfx.bits_per_sample` (the container size).
    /// Per Microsoft Learn, this is the active union member for the
    /// PCM/IEEE_FLOAT/ALAW/MULAW SubFormats; packed-sample codecs (e.g.
    /// ADPCM variants) use it for `wSamplesPerBlock` instead — the
    /// union shape is identical and the parser keeps the raw value.
    pub valid_bits_per_sample: u16,
    /// `dwChannelMask` — SPEAKER_* bitmap. The channel order in the
    /// PCM byte stream is the bit order of this mask (lowest set bit
    /// first) per Microsoft Learn § "Channel-mask channel ordering".
    pub channel_mask: u32,
    /// `SubFormat` GUID — the codec identifier. Most KSDATAFORMAT
    /// SubFormats follow the [`Guid::is_ksdataformat_base`] pattern and
    /// can be re-mapped to a legacy `wFormatTag` via
    /// [`Guid::ksdataformat_tag`].
    pub subformat: Guid,
}

/// Parse a WAVEFORMATEXTENSIBLE from a `strf` payload (the
/// 18-byte WAVEFORMATEX header + 2-byte `cbSize` + 22-byte extension =
/// 40 bytes minimum). Returns `Error::Invalid` when the on-wire
/// `wFormatTag` is not [`WAVE_FORMAT_EXTENSIBLE`] or the trailing
/// extension is short.
///
/// Tolerates `cbSize > 22` (extra trailing bytes are preserved as
/// extradata on the returned `wfx`) — some encoders pad the extension
/// for alignment or attach additional codec-private data per
/// Microsoft Learn § "WAVEFORMATEXTENSIBLE.Format.cbSize".
pub fn parse_waveformatextensible(buf: &[u8]) -> Result<WaveFormatExtensible> {
    let mut wfx = parse_waveformatex(buf)?;
    if wfx.format_tag != WAVE_FORMAT_EXTENSIBLE {
        return Err(Error::invalid(format!(
            "AVI: WAVEFORMATEXTENSIBLE requires wFormatTag = 0x{:04X}, got 0x{:04X}",
            WAVE_FORMAT_EXTENSIBLE, wfx.format_tag
        )));
    }
    if wfx.extradata.len() < 22 {
        return Err(Error::invalid(format!(
            "AVI: WAVEFORMATEXTENSIBLE requires >= 22 bytes of cbSize extension, got {}",
            wfx.extradata.len()
        )));
    }
    let valid_bits_per_sample = u16::from_le_bytes([wfx.extradata[0], wfx.extradata[1]]);
    let channel_mask = u32::from_le_bytes([
        wfx.extradata[2],
        wfx.extradata[3],
        wfx.extradata[4],
        wfx.extradata[5],
    ]);
    let subformat = Guid::from_bytes(&wfx.extradata[6..22]).expect("checked >= 22 bytes above");
    // Strip the consumed 22 bytes off `wfx.extradata` so the caller
    // sees only any genuine trailing payload (rare; usually empty).
    wfx.extradata = wfx.extradata[22..].to_vec();
    Ok(WaveFormatExtensible {
        wfx,
        valid_bits_per_sample,
        channel_mask,
        subformat,
    })
}

/// Emit a WAVEFORMATEXTENSIBLE: 18-byte WAVEFORMATEX with `wFormatTag
/// = 0xFFFE`, `cbSize = 22`, then the 22-byte extension. Total 40 bytes.
///
/// `container_bits` is the WAVEFORMATEX `wBitsPerSample` field (the
/// container size — e.g. 32 for 24-in-32 packing); `valid_bps` is the
/// `Samples.wValidBitsPerSample` member of the extension union (the
/// actual precision — e.g. 24 for 24-in-32). Use the same value for
/// both when the container size equals the precision.
#[allow(clippy::too_many_arguments)]
pub fn write_waveformatextensible(
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    container_bits: u16,
    valid_bps: u16,
    channel_mask: u32,
    subformat: &Guid,
) -> Vec<u8> {
    // Build the 22-byte extension first, then hand it to
    // `write_waveformatex` as extradata so the `cbSize` field carries
    // 22 automatically.
    let mut extension = Vec::with_capacity(22);
    extension.extend_from_slice(&valid_bps.to_le_bytes());
    extension.extend_from_slice(&channel_mask.to_le_bytes());
    extension.extend_from_slice(&subformat.0);
    write_waveformatex(
        WAVE_FORMAT_EXTENSIBLE,
        channels,
        samples_per_sec,
        avg_bytes_per_sec,
        block_align,
        container_bits,
        &extension,
    )
}

/// Resolve a [`WaveFormatExtensible::subformat`] GUID to a codec-id
/// hint when it matches one of the well-known `KSDATAFORMAT_SUBTYPE_*`
/// entries documented in `docs/container/riff/waveformatextensible/`.
///
/// Returns `None` for SubFormats outside the known set (the caller
/// should fall back to a synthetic `avi:guid_<canonical-text>` codec
/// id and let downstream dispatch decide). `bits_per_sample` is the
/// `Samples.wValidBitsPerSample` — used to pick the right
/// integer-PCM / float-PCM depth flavour the same way
/// [`crate::demuxer::audio_codec_id_fallback`] does for legacy
/// `WAVEFORMATEX`.
///
/// The PCM family in particular folds the legacy
/// `audio_codec_id_fallback` logic so that the demuxer's
/// PCM-with-`KSDATAFORMAT_SUBTYPE_PCM` path produces the same
/// depth-aware `pcm_u8` / `pcm_s16le` / `pcm_s24le` / `pcm_s32le`
/// codec ids as a legacy `WAVE_FORMAT_PCM` stream would, keeping
/// downstream consumers identical for the two encoding paths.
pub fn subformat_codec_hint(subformat: &Guid, bits_per_sample: u16) -> Option<&'static str> {
    if *subformat == KSDATAFORMAT_SUBTYPE_PCM {
        return Some(match bits_per_sample {
            8 => "pcm_u8",
            24 => "pcm_s24le",
            32 => "pcm_s32le",
            _ => "pcm_s16le",
        });
    }
    if *subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
        return Some(match bits_per_sample {
            64 => "pcm_f64le",
            _ => "pcm_f32le",
        });
    }
    if *subformat == KSDATAFORMAT_SUBTYPE_ALAW {
        return Some("pcm_alaw");
    }
    if *subformat == KSDATAFORMAT_SUBTYPE_MULAW {
        return Some("pcm_mulaw");
    }
    if *subformat == KSDATAFORMAT_SUBTYPE_ADPCM {
        return Some("adpcm_ms");
    }
    if *subformat == KSDATAFORMAT_SUBTYPE_MPEG {
        return Some("mp2");
    }
    if *subformat == KSDATAFORMAT_SUBTYPE_DRM {
        return Some("drm");
    }
    None
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
    fn color_table_8bpp_clr_used() {
        // 8-bpp DIB declaring 3 used colours. RGBQUAD on-disk order is
        // blue / green / red / reserved per RIFF MCI §"Color Table
        // Structure".
        let ext = [
            0x10, 0x20, 0x30, 0x00, // blue=0x10 green=0x20 red=0x30
            0x40, 0x50, 0x60, 0x00, // blue=0x40 green=0x50 red=0x60
            0x70, 0x80, 0x90, 0xFF, // blue=0x70 green=0x80 red=0x90 reserved=0xFF
        ];
        let table = parse_color_table(8, 3, &ext).unwrap();
        assert_eq!(table.len(), 3);
        assert_eq!(
            table[0],
            RgbQuad {
                blue: 0x10,
                green: 0x20,
                red: 0x30,
                reserved: 0x00
            }
        );
        assert_eq!(
            table[2],
            RgbQuad {
                blue: 0x70,
                green: 0x80,
                red: 0x90,
                reserved: 0xFF
            }
        );
    }

    #[test]
    fn color_table_clr_used_zero_means_depth_max() {
        // biClrUsed == 0 ⇒ "the bitmap uses the maximum number of
        // colors corresponding to the value of the [biBitCount] field"
        // (RIFF MCI §"Note on Windows DIBs"): 1 << 4 == 16 for a 4-bpp
        // DIB.
        let ext = vec![0u8; 16 * 4];
        let table = parse_color_table(4, 0, &ext).unwrap();
        assert_eq!(table.len(), 16);
    }

    #[test]
    fn color_table_monochrome_two_entries() {
        // 1-bpp ⇒ "monochrome, and the color table contains two
        // entries" per RIFF MCI §"Interpreting the Color Table".
        let ext = vec![0u8; 2 * 4];
        let table = parse_color_table(1, 0, &ext).unwrap();
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn color_table_truncated_returns_what_fits() {
        // Declared 8 colours but only 2 RGBQUADs present: parse the 2
        // complete entries the buffer holds rather than dropping the
        // whole table.
        let ext = [1, 2, 3, 0, 4, 5, 6, 0];
        let table = parse_color_table(8, 8, &ext).unwrap();
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn color_table_truecolour_has_no_table() {
        // 16 / 24 / 32-bpp truecolour DIBs carry no palette; bit_count
        // 0 (depth unspecified) likewise.
        let ext = vec![0u8; 256 * 4];
        assert!(parse_color_table(16, 0, &ext).is_none());
        assert!(parse_color_table(24, 0, &ext).is_none());
        assert!(parse_color_table(32, 0, &ext).is_none());
        assert!(parse_color_table(0, 0, &ext).is_none());
    }

    #[test]
    fn color_table_empty_extradata_returns_none() {
        assert!(parse_color_table(8, 0, &[]).is_none());
        assert!(parse_color_table(8, 4, &[1, 2, 3]).is_none());
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
    fn guid_display_canonical_form() {
        // Microsoft canonical Data1-Data2-Data3-Data4 hex layout. Use
        // KSDATAFORMAT_SUBTYPE_PCM which the docs README explicitly
        // shows as `00000001-0000-0010-8000-00aa00389b71`.
        let s = KSDATAFORMAT_SUBTYPE_PCM.display();
        assert_eq!(s, "00000001-0000-0010-8000-00aa00389b71");
    }

    #[test]
    fn guid_ksdataformat_base_pattern_and_tag_recovery() {
        // All seven canonical PCM/IEEE_FLOAT/DRM/ALAW/MULAW/ADPCM/MPEG
        // GUIDs follow the KSDATAFORMAT base pattern and round-trip to
        // their legacy wFormatTag value via `ksdataformat_tag`.
        for (guid, expected_tag) in [
            (KSDATAFORMAT_SUBTYPE_PCM, 0x0001u16),
            (KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, 0x0003),
            (KSDATAFORMAT_SUBTYPE_DRM, 0x0009),
            (KSDATAFORMAT_SUBTYPE_ALAW, 0x0006),
            (KSDATAFORMAT_SUBTYPE_MULAW, 0x0007),
            (KSDATAFORMAT_SUBTYPE_ADPCM, 0x0002),
            (KSDATAFORMAT_SUBTYPE_MPEG, 0x0050),
        ] {
            assert!(
                guid.is_ksdataformat_base(),
                "{} not KSDATAFORMAT base",
                guid.display()
            );
            assert_eq!(guid.ksdataformat_tag(), Some(expected_tag));
        }

        // A non-KS GUID returns None.
        let foreign = Guid::from_components(0xDEAD_BEEF, 0xCAFE, 0xBABE, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(!foreign.is_ksdataformat_base());
        assert_eq!(foreign.ksdataformat_tag(), None);
    }

    #[test]
    fn wfex_roundtrip_pcm_24in32() {
        // 24-bit-in-32-bit container PCM is the canonical use case for
        // WAVEFORMATEXTENSIBLE per the docs § "WAVEFORMATEX conflates
        // nBlockAlign / wBitsPerSample / 'container size'".
        let channels = 6u16;
        let sample_rate = 48_000u32;
        let block_align = channels * 4; // 32-bit container per sample
        let avg_bps = sample_rate * block_align as u32;
        // 5.1 Microsoft layout per docs README table.
        let channel_mask: u32 = 0x0000_003F;
        let bytes = write_waveformatextensible(
            channels,
            sample_rate,
            avg_bps,
            block_align,
            32, // container
            24, // valid
            channel_mask,
            &KSDATAFORMAT_SUBTYPE_PCM,
        );
        assert_eq!(bytes.len(), 40, "WAVEFORMATEXTENSIBLE is 18 + 2 + 22 bytes");
        // cbSize (offset 16..18) must be 22.
        assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 22);

        let parsed = parse_waveformatextensible(&bytes).unwrap();
        assert_eq!(parsed.wfx.format_tag, WAVE_FORMAT_EXTENSIBLE);
        assert_eq!(parsed.wfx.channels, channels);
        assert_eq!(parsed.wfx.samples_per_sec, sample_rate);
        assert_eq!(parsed.wfx.avg_bytes_per_sec, avg_bps);
        assert_eq!(parsed.wfx.block_align, block_align);
        assert_eq!(parsed.wfx.bits_per_sample, 32);
        assert_eq!(parsed.valid_bits_per_sample, 24);
        assert_eq!(parsed.channel_mask, channel_mask);
        assert_eq!(parsed.subformat, KSDATAFORMAT_SUBTYPE_PCM);
        // After consuming the 22-byte extension, no genuine trailing
        // extradata should remain.
        assert!(parsed.wfx.extradata.is_empty());
    }

    #[test]
    fn wfex_rejects_non_extensible_format_tag() {
        // Force a legacy PCM `wFormatTag` (0x0001) and verify the
        // parser refuses to treat it as extensible.
        let bytes = write_waveformatex(0x0001, 2, 48_000, 192_000, 4, 16, &[0u8; 22]);
        assert!(parse_waveformatextensible(&bytes).is_err());
    }

    #[test]
    fn wfex_rejects_short_extension() {
        // wFormatTag = 0xFFFE but only 10 bytes of extension (< 22) —
        // must error rather than silently zero-pad.
        let bytes = write_waveformatex(
            WAVE_FORMAT_EXTENSIBLE,
            2,
            48_000,
            192_000,
            4,
            16,
            &[0u8; 10],
        );
        assert!(parse_waveformatextensible(&bytes).is_err());
    }

    #[test]
    fn subformat_codec_hint_pcm_depth_aware() {
        // PCM-family hint mirrors the demuxer's legacy
        // `audio_codec_id_fallback` depth selection.
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_PCM, 8),
            Some("pcm_u8")
        );
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_PCM, 16),
            Some("pcm_s16le")
        );
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_PCM, 24),
            Some("pcm_s24le")
        );
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_PCM, 32),
            Some("pcm_s32le")
        );
        // Float family.
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, 32),
            Some("pcm_f32le")
        );
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, 64),
            Some("pcm_f64le")
        );
        // Companded + ADPCM + MPEG + DRM map to single codec ids.
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_ALAW, 8),
            Some("pcm_alaw")
        );
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_MULAW, 8),
            Some("pcm_mulaw")
        );
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_ADPCM, 4),
            Some("adpcm_ms")
        );
        assert_eq!(
            subformat_codec_hint(&KSDATAFORMAT_SUBTYPE_MPEG, 0),
            Some("mp2")
        );
        // Unknown GUID returns None for the caller-side fallback path.
        let foreign = Guid::from_components(0xDEAD, 0xBEEF, 0xCAFE, [0; 8]);
        assert!(subformat_codec_hint(&foreign, 16).is_none());
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

    // ---- Round 163: ChannelMask + ChannelLayout typed surface ----------

    #[test]
    fn channel_mask_empty_and_layout_for_unrecognised() {
        // dwChannelMask == 0 ⇒ "speaker assignment unspecified" per
        // Microsoft Learn. Returns no Speakers, no named layout.
        let m = ChannelMask::from_raw(0);
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert_eq!(m.iter_speakers().count(), 0);
        assert!(m.layout().is_none());
        assert_eq!(m.reserved_bits(), 0);
    }

    #[test]
    fn channel_mask_iter_order_matches_docs_table() {
        // 5.1 (Microsoft back) per docs README:
        //   FL | FR | FC | LFE | BL | BR (== 0x0000_003F).
        // Channels are stored in the file in the bit order of the mask
        // (lowest set bit first) ⇒ iter_speakers yields the same order.
        let m = ChannelMask::from_raw(0x0000_003F);
        let got: Vec<Speaker> = m.iter_speakers().collect();
        assert_eq!(
            got,
            vec![
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::FrontCenter,
                Speaker::LowFrequency,
                Speaker::BackLeft,
                Speaker::BackRight,
            ],
            "iter_speakers must follow lowest-set-bit-first PCM channel order"
        );
        assert_eq!(m.len(), 6);
        assert_eq!(m.layout(), Some(ChannelLayout::FivePointOneBack));
    }

    #[test]
    fn channel_mask_named_layouts_round_trip() {
        // Every named layout from the docs README "Standard layouts"
        // table must round-trip mask -> layout -> mask exactly.
        for layout in [
            ChannelLayout::Mono,
            ChannelLayout::Stereo,
            ChannelLayout::TwoPointOne,
            ChannelLayout::Quad,
            ChannelLayout::FivePointOneBack,
            ChannelLayout::FivePointOneSide,
            ChannelLayout::SevenPointOne,
        ] {
            let mask = layout.mask();
            let recovered = ChannelLayout::from_mask(mask);
            assert_eq!(
                recovered,
                Some(layout),
                "layout {:?} (mask 0x{:08X}) must round-trip",
                layout,
                mask
            );
        }
    }

    #[test]
    fn channel_mask_layout_specific_values_match_docs_table() {
        // Spot-check the four mask values explicitly called out in the
        // docs README "Standard layouts" table.
        assert_eq!(ChannelLayout::Mono.mask(), 0x0000_0004);
        assert_eq!(ChannelLayout::Stereo.mask(), 0x0000_0003);
        assert_eq!(ChannelLayout::Quad.mask(), 0x0000_0033);
        assert_eq!(ChannelLayout::FivePointOneBack.mask(), 0x0000_003F);
        // DVD-style: FL|FR|FC|LFE|SL|SR = 0x0000_060F per docs README.
        assert_eq!(ChannelLayout::FivePointOneSide.mask(), 0x0000_060F);
        // 7.1: FL|FR|FC|LFE|BL|BR|SL|SR = 0x0000_063F.
        assert_eq!(ChannelLayout::SevenPointOne.mask(), 0x0000_063F);
    }

    #[test]
    fn channel_mask_reserved_bits_isolated_and_ignored_for_layout() {
        // Microsoft reserves the gap between TBR (0x20000) and ALL
        // (0x80000000). Stereo + a stray reserved bit in that gap
        // still classifies as Stereo; the reserved bit is surfaced
        // separately so the caller can warn on it.
        let stereo_plus_reserved = ChannelLayout::Stereo.mask() | 0x0040_0000;
        let m = ChannelMask::from_raw(stereo_plus_reserved);
        assert_eq!(m.layout(), Some(ChannelLayout::Stereo));
        assert_eq!(m.reserved_bits(), 0x0040_0000);
        // iter_speakers must NOT yield anything for the reserved bit.
        let got: Vec<Speaker> = m.iter_speakers().collect();
        assert_eq!(got, vec![Speaker::FrontLeft, Speaker::FrontRight]);
        assert_eq!(m.len(), 2, "len counts only documented bits");
    }

    #[test]
    fn channel_mask_speaker_all_surfaces_separately() {
        // SPEAKER_ALL (0x80000000) is the top-bit catch-all per
        // Microsoft Learn. iter_speakers must yield SpeakerAll for it,
        // but a bare SpeakerAll bit is NOT one of the named layouts.
        let m = ChannelMask::from_raw(SPEAKER_ALL);
        let got: Vec<Speaker> = m.iter_speakers().collect();
        assert_eq!(got, vec![Speaker::SpeakerAll]);
        assert_eq!(m.layout(), None);
        assert_eq!(m.len(), 1);
        assert_eq!(m.reserved_bits(), 0);
    }

    #[test]
    fn speaker_abbrev_and_mask_bit_table_complete() {
        // Every Speaker variant must have a non-empty abbreviation and
        // a non-zero mask bit; mask bits must be unique (no overlap).
        let all = [
            Speaker::FrontLeft,
            Speaker::FrontRight,
            Speaker::FrontCenter,
            Speaker::LowFrequency,
            Speaker::BackLeft,
            Speaker::BackRight,
            Speaker::FrontLeftOfCenter,
            Speaker::FrontRightOfCenter,
            Speaker::BackCenter,
            Speaker::SideLeft,
            Speaker::SideRight,
            Speaker::TopCenter,
            Speaker::TopFrontLeft,
            Speaker::TopFrontCenter,
            Speaker::TopFrontRight,
            Speaker::TopBackLeft,
            Speaker::TopBackCenter,
            Speaker::TopBackRight,
            Speaker::SpeakerAll,
        ];
        let mut seen = 0u32;
        for sp in all {
            let bit = sp.mask_bit();
            assert!(bit != 0, "{:?} mask bit must be non-zero", sp);
            assert_eq!(
                seen & bit,
                0,
                "{:?} bit 0x{:08X} overlaps a previous Speaker",
                sp,
                bit
            );
            seen |= bit;
            assert!(!sp.abbrev().is_empty(), "{:?} abbrev must be non-empty", sp);
        }
    }
}
