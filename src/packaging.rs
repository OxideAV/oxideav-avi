//! AVI muxer-side codec packaging.
//!
//! Builds the per-stream `strf` chunk (a `BITMAPINFOHEADER` for video or a
//! `WAVEFORMATEX` for audio) plus the metadata the muxer needs to write the
//! `strh` chunk and tag movi packets. Codec ↔ on-wire-tag resolution reads
//! `CodecParameters::tag` directly — set by the demuxer at read-time
//! (round-trip preservation) or by the encoder via `output_params()` at
//! configure-time (multi-FourCC codecs like MagicYUV's 17 native v7
//! variants). The muxer no longer consults the codec registry for the
//! inverse direction; the registry's "first-declared tag for this codec_id"
//! answer is arbitrary on multi-tag codecs and was breaking round-trip.
//!
//! The PCM family is the one place where bit-depth-aware tag synthesis is
//! still done: integer PCM (`WaveFormat(0x0001)`) and IEEE float
//! (`WaveFormat(0x0003)`) are shared across every `pcm_*` codec, so when
//! `params.tag` is `None` and the codec id is in the PCM family the muxer
//! synthesises the right `wFormatTag` from the codec id; `params.sample_format`
//! still drives the WAVEFORMATEX `wBitsPerSample` field.

use oxideav_core::{CodecId, CodecParameters, CodecTag, Error, MediaType, Result, SampleFormat};

use crate::stream_format::{
    write_bitmap_info_header, write_bitmap_info_header_oriented, write_indexed_bitmap_info_header,
    write_waveformatex, write_waveformatextensible, Guid, RgbQuad, WAVE_FORMAT_EXTENSIBLE,
};

/// Result of building a stream-format chunk for the muxer.
#[derive(Debug)]
pub(crate) struct StrfEntry {
    /// Two-ASCII-digit FourCC suffix used for packet chunks in `movi`: `dc`
    /// for compressed video, `wb` for audio, `db` for uncompressed video.
    pub chunk_suffix: [u8; 2],
    /// 4-byte `fccHandler` field for the `strh` chunk.
    pub handler_fourcc: [u8; 4],
    /// Full `strf` payload (BITMAPINFOHEADER or WAVEFORMATEX).
    pub strf: Vec<u8>,
    /// ffmpeg-compatible four-char stream-type tag (`vids`/`auds`) for strh.
    pub strh_type: [u8; 4],
    /// Sample size hint for `strh.dwSampleSize` — 0 means "variable" (VBR).
    pub sample_size: u32,
    /// Scale / rate pair for `strh.dwScale / dwRate` (rate/scale = samples
    /// per second). For video we use frame_rate; for audio sample_rate/1.
    pub scale: u32,
    pub rate: u32,
}

/// Build the `strf` chunk + `strh` metadata for the given stream.
///
/// Returns `Error::Unsupported` when no wire tag can be derived: no
/// `params.tag`, no printable `extradata[0..4]` hint, and the codec
/// id isn't an uncompressed `rgb24` / PCM family for which the tag
/// is synthesised.
///
/// `top_down` (round-19 C1) is only honoured for video streams and
/// only when the resolved FourCC is the all-zero `BI_RGB` sentinel —
/// the spec REQUIRES positive `biHeight` for compressed FourCCs per
/// VfW §"biHeight sign rules". For audio / non-RGB video streams it
/// has no effect.
///
/// `extensible` (round-75) is only honoured for audio streams. When
/// `Some((channel_mask, valid_bps, subformat_guid))` the muxer emits
/// a 40-byte `WAVEFORMATEXTENSIBLE` `strf` payload with
/// `wFormatTag = 0xFFFE` per Microsoft `mmreg.h` §
/// "WAVEFORMATEXTENSIBLE", regardless of `params.tag`'s
/// `WaveFormat(...)` value (the extensible escape hatch is the whole
/// point — the SubFormat GUID is the canonical identifier when
/// `wFormatTag = 0xFFFE`). For video streams it has no effect.
pub(crate) fn build_strf(
    params: &CodecParameters,
    top_down: bool,
    extensible: Option<(u32, u16, Guid)>,
    indexed: Option<(u16, &[RgbQuad])>,
    bmih_overrides: BmihOverrides,
) -> Result<StrfEntry> {
    match params.media_type {
        MediaType::Video => {
            let mut entry = build_video_strf(params, top_down, indexed)?;
            apply_bmih_overrides(&mut entry.strf, bmih_overrides);
            Ok(entry)
        }
        MediaType::Audio => build_audio_strf(params, extensible),
        _ => Err(Error::unsupported(format!(
            "avi muxer: media type {:?} not supported",
            params.media_type
        ))),
    }
}

/// Optional caller overrides for the `BITMAPINFOHEADER` scalar fields the
/// muxer otherwise leaves at their writer-default `0` / `1` per VfW
/// `wingdi.h` §"BITMAPINFOHEADER". Each field is patched verbatim into
/// the 40-byte fixed header *after* the default strf is built, so an
/// override never perturbs `biWidth` / `biHeight` / `biCompression` / the
/// trailing extradata or color table. A `None` leaves the muxer's default
/// in place. Round-381.
#[derive(Clone, Copy, Debug, Default)]
pub struct BmihOverrides {
    /// `biSizeImage` (byte offset 20).
    pub size_image: Option<u32>,
    /// `biXPelsPerMeter` / `biYPelsPerMeter` (byte offsets 24 + 28).
    pub pels_per_meter: Option<(i32, i32)>,
    /// `biClrImportant` (byte offset 36).
    pub clr_important: Option<u32>,
    /// `biPlanes` (byte offset 12) — for callers reproducing a
    /// non-conformant writer that stamped a value other than `1`.
    pub planes: Option<u16>,
}

impl BmihOverrides {
    fn is_empty(&self) -> bool {
        self.size_image.is_none()
            && self.pels_per_meter.is_none()
            && self.clr_important.is_none()
            && self.planes.is_none()
    }
}

/// Patch the supplied `BmihOverrides` into a freshly-built `strf`
/// (BITMAPINFOHEADER) payload. No-op when the payload is shorter than the
/// 40-byte fixed header (it never is for a video strf) or when no override
/// is set. Note `biClrUsed` is deliberately NOT overridable here — it is
/// load-bearing for the indexed-DIB color-table length the demuxer reads
/// back, so it stays owned by `with_indexed_video`.
fn apply_bmih_overrides(strf: &mut [u8], ov: BmihOverrides) {
    if ov.is_empty() || strf.len() < 40 {
        return;
    }
    if let Some(planes) = ov.planes {
        strf[12..14].copy_from_slice(&planes.to_le_bytes());
    }
    if let Some(size_image) = ov.size_image {
        strf[20..24].copy_from_slice(&size_image.to_le_bytes());
    }
    if let Some((x, y)) = ov.pels_per_meter {
        strf[24..28].copy_from_slice(&x.to_le_bytes());
        strf[28..32].copy_from_slice(&y.to_le_bytes());
    }
    if let Some(clr_important) = ov.clr_important {
        strf[36..40].copy_from_slice(&clr_important.to_le_bytes());
    }
}

/// Pick the wire FourCC for a video stream.
///
/// Resolution order:
/// 1. **`params.tag` if `Some(CodecTag::Fourcc(...))`** — the
///    canonical primary path. Set by the demuxer at read-time (so
///    round-trip preserves the original FourCC byte-for-byte) or by
///    the encoder's `output_params()` at configure-time.
/// 2. The first 4 bytes of `extradata` if they spell a printable
///    FourCC — legacy fallback for callers that haven't migrated to
///    `params.tag` yet.
/// 3. The `[0,0,0,0]` `BI_RGB` sentinel for `rgb24` (the one codec
///    id whose "FourCC" is all-zero bytes).
fn video_fourcc(params: &CodecParameters) -> Result<[u8; 4]> {
    if let Some(CodecTag::Fourcc(bytes)) = &params.tag {
        return Ok(*bytes);
    }
    if let Some(hint) = extradata_fourcc_hint(&params.extradata) {
        return Ok(hint);
    }
    if params.codec_id.as_str() == "rgb24" {
        return Ok([0, 0, 0, 0]);
    }
    Err(Error::unsupported(format!(
        "avi muxer: codec `{}` has no FourCC; \
         set `params.tag = Some(CodecTag::fourcc(...))` (preferred), \
         or pre-fill `extradata`'s first 4 bytes with the desired FourCC",
        params.codec_id
    )))
}

/// Pick the wFormatTag for an audio stream.
///
/// Resolution order:
/// 1. **`params.tag` if `Some(CodecTag::WaveFormat(...))`** — the
///    canonical primary path.
/// 2. PCM-family synthesis: integer PCM and IEEE-float-PCM share
///    one wFormatTag value across every depth, so the muxer derives
///    `0x0001` / `0x0003` from the codec id directly. This applies
///    regardless of whether `params.tag` is set.
fn audio_format_tag(params: &CodecParameters) -> Result<u16> {
    if let Some(CodecTag::WaveFormat(t)) = &params.tag {
        return Ok(*t);
    }
    if let Some(synth) = pcm_synth_format_tag(&params.codec_id) {
        return Ok(synth);
    }
    Err(Error::unsupported(format!(
        "avi muxer: codec `{}` has no WAVEFORMATEX wFormatTag; \
         set `params.tag = Some(CodecTag::wave_format(...))`",
        params.codec_id
    )))
}

/// PCM codecs use a fixed wFormatTag (0x0001 integer or 0x0003 float)
/// regardless of bit depth — registering them via `CodecTag::WaveFormat`
/// would map every PCM depth onto the same tag, which is fine for the
/// forward (resolve-tag) direction (probes pick the right depth) but
/// breaks the inverse direction. Synthesise here so we don't depend on
/// the order of PCM registrations.
fn pcm_synth_format_tag(codec_id: &CodecId) -> Option<u16> {
    match codec_id.as_str() {
        "pcm_u8" | "pcm_s16le" | "pcm_s24le" | "pcm_s32le" => Some(0x0001),
        "pcm_f32le" | "pcm_f64le" => Some(0x0003),
        _ => None,
    }
}

/// Inspect the first 4 bytes of `extradata`. If they're a printable
/// alphanumeric/space ASCII FourCC, return them upper-cased; otherwise
/// `None`. Used by codecs (e.g. magicyuv with its 17 native v7
/// variants) where the caller selects the wire FourCC by populating
/// the leading bytes of `extradata`.
fn extradata_fourcc_hint(extradata: &[u8]) -> Option<[u8; 4]> {
    if extradata.len() < 4 {
        return None;
    }
    let mut hint = [0u8; 4];
    hint.copy_from_slice(&extradata[..4]);
    if !hint.iter().all(|&b| b.is_ascii_alphanumeric() || b == b' ') {
        return None;
    }
    for b in hint.iter_mut() {
        *b = b.to_ascii_uppercase();
    }
    Some(hint)
}

fn build_video_strf(
    params: &CodecParameters,
    top_down: bool,
    indexed: Option<(u16, &[RgbQuad])>,
) -> Result<StrfEntry> {
    let width = params
        .width
        .ok_or_else(|| Error::invalid("avi muxer: video stream missing width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("avi muxer: video stream missing height"))?;
    let fourcc = video_fourcc(params)?;
    // Round-377: an indexed (palettised) BI_RGB DIB override. When the
    // caller registered the stream via `with_indexed_video`, emit a
    // BITMAPINFOHEADER with `biBitCount` of 1/4/8, `biClrUsed` =
    // palette length, and the `RGBQUAD` color table appended verbatim —
    // the write-side complement of the demuxer's `stream_palette`
    // accessor. This forces `BI_RGB` (indexed DIBs are uncompressed) so
    // it only applies to streams the caller deliberately marks indexed.
    if let Some((bit_count, palette)) = indexed {
        let strf = write_indexed_bitmap_info_header(width, height, bit_count, palette);
        let (scale, rate) = video_scale_rate(params);
        return Ok(StrfEntry {
            chunk_suffix: *b"db",
            handler_fourcc: [0, 0, 0, 0],
            strf,
            strh_type: *b"vids",
            sample_size: 0,
            scale,
            rate,
        });
    }
    // bit_count: 24 for compressed bitstreams (the conventional advisory
    // value); for BI_RGB we use 24 too (24-bit packed RGB is the
    // canonical uncompressed AVI pixel format we package).
    let bit_count: u16 = 24;
    // Round-19 C1: a top-down DIB is only legal for `BI_RGB` (and
    // `BI_BITFIELDS`, which we don't currently emit) per VfW
    // §"biHeight sign rules". Silently drop the flag for compressed
    // FourCCs rather than producing an out-of-spec file.
    let allow_top_down = fourcc == [0, 0, 0, 0];
    let strf = if top_down && allow_top_down {
        write_bitmap_info_header_oriented(width, height, fourcc, bit_count, &params.extradata, true)
    } else {
        write_bitmap_info_header(width, height, fourcc, bit_count, &params.extradata)
    };
    let (scale, rate) = video_scale_rate(params);
    // BI_RGB streams use `db` chunks; everything else `dc`.
    let chunk_suffix = if fourcc == [0, 0, 0, 0] {
        *b"db"
    } else {
        *b"dc"
    };
    Ok(StrfEntry {
        chunk_suffix,
        handler_fourcc: fourcc,
        strf,
        strh_type: *b"vids",
        sample_size: 0,
        scale,
        rate,
    })
}

fn build_audio_strf(
    params: &CodecParameters,
    extensible: Option<(u32, u16, Guid)>,
) -> Result<StrfEntry> {
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("avi muxer: audio stream missing channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("avi muxer: audio stream missing sample_rate"))?;

    // Round-75: WAVEFORMATEXTENSIBLE override. When the caller
    // registered the stream via `with_extensible_audio`, the muxer
    // emits a 40-byte WAVEFORMATEXTENSIBLE strf with `wFormatTag =
    // 0xFFFE` regardless of `params.tag`. The PCM container size
    // (`wBitsPerSample`) still comes from the codec id /
    // sample_format so byte-stream layout stays correct; the
    // `valid_bps` argument is the precision (which may be smaller).
    if let Some((channel_mask, valid_bps, subformat)) = extensible {
        let id = params.codec_id.as_str();
        let container_bits = pcm_bits_per_sample(id, params.sample_format).unwrap_or_else(|| {
            // Non-PCM extensible streams (rare; e.g. extensible-AC3
            // by GUID). Round up to the nearest byte from valid_bps;
            // for VBR codecs an 8-bit container is the sentinel
            // legacy WAVEFORMATEX would use.
            valid_bps.max(8).next_multiple_of(8)
        });
        let block_align = channels * (container_bits / 8).max(1);
        let avg_bytes_per_sec = sample_rate * block_align as u32;
        let strf = write_waveformatextensible(
            channels,
            sample_rate,
            avg_bytes_per_sec,
            block_align,
            container_bits,
            valid_bps,
            channel_mask,
            &subformat,
        );
        // PCM-family extensible streams remain CBR; non-PCM
        // extensible streams (rare) follow the parent codec's
        // VBR/CBR rules — but since the parent codec id determined
        // `container_bits` above via `pcm_bits_per_sample`, the
        // safer default is `block_align` as sample_size whenever we
        // got a real PCM depth; otherwise fall back to 0 (VBR).
        let sample_size = if pcm_bits_per_sample(id, params.sample_format).is_some() {
            block_align as u32
        } else {
            0
        };
        return Ok(StrfEntry {
            chunk_suffix: *b"wb",
            handler_fourcc: *b"\0\0\0\0",
            strf,
            strh_type: *b"auds",
            sample_size,
            scale: 1,
            rate: sample_rate,
        });
    }
    // The remaining (legacy WAVEFORMATEX) paths reject
    // `params.tag = WaveFormat(0xFFFE)` indirectly: `audio_format_tag`
    // would happily stamp it, but the legacy 18-byte strf can't carry
    // the SubFormat GUID. Callers using EXTENSIBLE must go through
    // `with_extensible_audio` above.
    let format_tag = audio_format_tag(params)?;
    if format_tag == WAVE_FORMAT_EXTENSIBLE {
        return Err(Error::invalid(
            "avi muxer: WAVE_FORMAT_EXTENSIBLE (0xFFFE) requires \
             AviMuxOptions::with_extensible_audio(channel_mask, valid_bps, subformat_guid)",
        ));
    }
    let id = params.codec_id.as_str();

    // PCM family: choose bit_depth from sample_format (or codec_id), and
    // pack a fixed-size frame.
    if let Some(bits) = pcm_bits_per_sample(id, params.sample_format) {
        let block_align = channels * (bits / 8).max(1);
        let avg_bytes_per_sec = sample_rate * block_align as u32;
        let strf = write_waveformatex(
            format_tag,
            channels,
            sample_rate,
            avg_bytes_per_sec,
            block_align,
            bits,
            &[],
        );
        return Ok(StrfEntry {
            chunk_suffix: *b"wb",
            handler_fourcc: *b"\0\0\0\0",
            strf,
            strh_type: *b"auds",
            sample_size: block_align as u32,
            scale: 1,
            rate: sample_rate,
        });
    }

    // Companded PCM (G.711 a-law / mu-law): 8-bit fixed.
    if matches!(id, "pcm_alaw" | "pcm_mulaw") {
        let block_align = channels;
        let avg_bytes_per_sec = sample_rate * block_align as u32;
        let strf = write_waveformatex(
            format_tag,
            channels,
            sample_rate,
            avg_bytes_per_sec,
            block_align,
            8,
            &[],
        );
        return Ok(StrfEntry {
            chunk_suffix: *b"wb",
            handler_fourcc: *b"\0\0\0\0",
            strf,
            strh_type: *b"auds",
            sample_size: block_align as u32,
            scale: 1,
            rate: sample_rate,
        });
    }

    // Compressed audio (mp2 / mp3 / aac / ac3 / eac3 / flac / …): VBR-friendly,
    // sample_size = 0 → each chunk is one frame.
    let avg_bytes_per_sec = params.bit_rate.map(|b| (b / 8) as u32).unwrap_or(0);
    let block_align: u16 = 1;
    let strf = write_waveformatex(
        format_tag,
        channels,
        sample_rate,
        avg_bytes_per_sec,
        block_align,
        0,
        &params.extradata,
    );
    Ok(StrfEntry {
        chunk_suffix: *b"wb",
        handler_fourcc: *b"\0\0\0\0",
        strf,
        strh_type: *b"auds",
        sample_size: 0,
        scale: 1,
        rate: sample_rate,
    })
}

/// Width of one PCM sample in bits, derived from `codec_id` (always
/// authoritative when set) or `sample_format` (for codecs that share a
/// `pcm_*` family but encode the depth on the parameter side).
fn pcm_bits_per_sample(codec_id: &str, sample_format: Option<SampleFormat>) -> Option<u16> {
    match codec_id {
        "pcm_u8" => Some(8),
        "pcm_s16le" | "pcm_s16be" => Some(16),
        "pcm_s24le" => Some(24),
        "pcm_s32le" => Some(32),
        "pcm_f32le" => Some(32),
        "pcm_f64le" => Some(64),
        _ => sample_format.map(|f| (f.bytes_per_sample() as u16) * 8),
    }
}

fn video_scale_rate(params: &CodecParameters) -> (u32, u32) {
    if let Some(fr) = params.frame_rate {
        let num = fr.num.max(1) as u32;
        let den = fr.den.max(1) as u32;
        return (den, num);
    }
    (1, 25)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extradata_hint_picks_uppercase_printable() {
        assert_eq!(extradata_fourcc_hint(b"M8RGtail"), Some(*b"M8RG"));
        assert_eq!(extradata_fourcc_hint(b"m8rgtail"), Some(*b"M8RG"));
        // Non-printable bytes → no hint.
        assert!(extradata_fourcc_hint(&[0, 1, 2, 3]).is_none());
        // Too short.
        assert!(extradata_fourcc_hint(b"abc").is_none());
    }

    #[test]
    fn video_fourcc_reads_params_tag() {
        // Canonical primary path: `params.tag` carries the wire FourCC.
        let mut p =
            CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
        p.width = Some(64);
        p.height = Some(64);
        let fc = video_fourcc(&p).unwrap();
        assert_eq!(&fc, b"M8RG");
    }

    #[test]
    fn video_fourcc_params_tag_wins_over_extradata_hint() {
        // When both are set, `params.tag` is authoritative — the
        // demuxer / encoder is the canonical source of truth.
        let mut p =
            CodecParameters::video(CodecId::new("magicyuv")).with_tag(CodecTag::fourcc(b"M8RG"));
        p.width = Some(64);
        p.height = Some(64);
        p.extradata = b"M8YAtail".to_vec();
        let fc = video_fourcc(&p).unwrap();
        assert_eq!(&fc, b"M8RG");
    }

    #[test]
    fn video_fourcc_falls_back_to_extradata_hint() {
        // Legacy fallback: no `params.tag` set, extradata's first 4
        // bytes spell a printable FourCC.
        let mut p = CodecParameters::video(CodecId::new("magicyuv"));
        p.width = Some(64);
        p.height = Some(64);
        p.extradata = b"M8YA-extra".to_vec();
        let fc = video_fourcc(&p).unwrap();
        assert_eq!(&fc, b"M8YA");
    }

    #[test]
    fn video_fourcc_unknown_codec_errors() {
        let mut p = CodecParameters::video(CodecId::new("noexist"));
        p.width = Some(64);
        p.height = Some(64);
        match video_fourcc(&p) {
            Err(Error::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn rgb24_uses_bi_rgb_sentinel() {
        // No params.tag, no extradata hint → the codec_id-side
        // synthetic for BI_RGB is a special case.
        let mut p = CodecParameters::video(CodecId::new("rgb24"));
        p.width = Some(64);
        p.height = Some(64);
        let fc = video_fourcc(&p).unwrap();
        assert_eq!(&fc, &[0, 0, 0, 0]);
    }

    #[test]
    fn pcm_format_tag_is_synthesised() {
        // PCM codecs share wFormatTag values, so the muxer derives
        // them from the codec id without needing `params.tag`.
        let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        let entry = build_strf(&p, false, None, None, BmihOverrides::default()).unwrap();
        assert_eq!(&entry.strh_type, b"auds");
        assert_eq!(entry.sample_size, 4); // 2ch × 2B
    }

    #[test]
    fn compressed_audio_uses_params_tag() {
        let mut p =
            CodecParameters::audio(CodecId::new("mp3")).with_tag(CodecTag::wave_format(0x0055));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        let entry = build_strf(&p, false, None, None, BmihOverrides::default()).unwrap();
        assert_eq!(&entry.strh_type, b"auds");
        // First 2 bytes of the WAVEFORMATEX are the wFormatTag in LE.
        assert_eq!(&entry.strf[0..2], &0x0055u16.to_le_bytes());
    }

    #[test]
    fn unknown_audio_codec_errors() {
        let mut p = CodecParameters::audio(CodecId::new("noexist"));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        match build_strf(&p, false, None, None, BmihOverrides::default()) {
            Err(Error::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn top_down_only_honoured_for_bi_rgb() {
        // Round-19 C1: top_down=true on a non-BI_RGB stream silently
        // drops the flag (compressed FourCCs MUST use positive
        // biHeight per VfW §"biHeight sign rules").
        let mut p = CodecParameters::video(CodecId::new("mjpeg"))
            .with_tag(oxideav_core::CodecTag::fourcc(b"MJPG"));
        p.width = Some(320);
        p.height = Some(240);
        let entry = build_strf(&p, true, None, None, BmihOverrides::default()).unwrap();
        // biHeight offset 8..12 in the BMIH; must be positive 240.
        let h = i32::from_le_bytes([entry.strf[8], entry.strf[9], entry.strf[10], entry.strf[11]]);
        assert_eq!(h, 240, "compressed FourCCs MUST use positive biHeight");

        // BI_RGB (rgb24) does honour top_down.
        let mut p = CodecParameters::video(CodecId::new("rgb24"));
        p.width = Some(320);
        p.height = Some(240);
        let entry = build_strf(&p, true, None, None, BmihOverrides::default()).unwrap();
        let h = i32::from_le_bytes([entry.strf[8], entry.strf[9], entry.strf[10], entry.strf[11]]);
        assert_eq!(h, -240, "BI_RGB + top_down ⇒ negative biHeight");
    }
}
