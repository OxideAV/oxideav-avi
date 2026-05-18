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
}
