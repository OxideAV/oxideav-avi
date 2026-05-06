//! AVI muxer-side codec packaging.
//!
//! Builds the per-stream `strf` chunk (a `BITMAPINFOHEADER` for video or a
//! `WAVEFORMATEX` for audio) plus the metadata the muxer needs to write the
//! `strh` chunk and tag movi packets. Codec ↔ on-wire-tag resolution flows
//! through `oxideav_core::CodecResolver::tag_for_codec`: each codec crate
//! declares its FourCC / `WAVE_FORMAT_*` claims via
//! `CodecInfo::tags(...)`, the registry indexes them, and this module asks
//! the registry the inverse question — "which tag should I write for this
//! codec_id?". `NullCodecResolver` is supported as a default; the muxer
//! returns `Error::Unsupported` for codecs the registry can't resolve.
//!
//! The PCM family is the one place where bit-depth-aware tag synthesis is
//! still needed: integer PCM (`WaveFormat(0x0001)`) and IEEE float
//! (`WaveFormat(0x0003)`) are claimed by multiple `pcm_*` codecs sharing the
//! same wFormatTag — the muxer reads `params.sample_format` to pick the
//! depth that goes into the WAVEFORMATEX `wBitsPerSample` field.

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, CodecTagKind, Error, MediaType, Result,
    SampleFormat,
};

use crate::stream_format::{write_bitmap_info_header, write_waveformatex};

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
/// Returns `Error::Unsupported` when the resolver doesn't know which
/// FourCC / wFormatTag this `codec_id` writes to (i.e. the codec crate
/// hasn't registered its tags) AND the codec isn't an uncompressed
/// PCM family for which we synthesise the tag from `sample_format`.
pub(crate) fn build_strf(
    params: &CodecParameters,
    codecs: &dyn CodecResolver,
) -> Result<StrfEntry> {
    match params.media_type {
        MediaType::Video => build_video_strf(params, codecs),
        MediaType::Audio => build_audio_strf(params, codecs),
        _ => Err(Error::unsupported(format!(
            "avi muxer: media type {:?} not supported",
            params.media_type
        ))),
    }
}

/// Pick the wire FourCC for a video stream.
///
/// Resolution order:
/// 1. The first 4 bytes of `extradata` if they spell a printable FourCC
///    (used by codecs with multiple equivalent wire FourCCs that want
///    the caller to pick — e.g. mpeg4video can be `XVID` / `DIVX` /
///    `DX50` etc.).
/// 2. `CodecResolver::tag_for_codec(codec_id, Fourcc)` — the codec crate's
///    canonical FourCC declared via `CodecInfo::tags`.
/// 3. The `[0,0,0,0]` `BI_RGB` sentinel for `rgb24` (the one codec id we
///    can't ever round-trip through the registry because the
///    "FourCC" is all-zero bytes — a valid `CodecTag::Fourcc` value
///    but conventionally not registered).
fn video_fourcc(params: &CodecParameters, codecs: &dyn CodecResolver) -> Result<[u8; 4]> {
    if let Some(hint) = extradata_fourcc_hint(&params.extradata) {
        return Ok(hint);
    }
    if let Some(CodecTag::Fourcc(bytes)) =
        codecs.tag_for_codec(&params.codec_id, CodecTagKind::Fourcc)
    {
        return Ok(bytes);
    }
    if params.codec_id.as_str() == "rgb24" {
        return Ok([0, 0, 0, 0]);
    }
    Err(Error::unsupported(format!(
        "avi muxer: codec `{}` has no registered FourCC; \
         pre-fill `extradata`'s first 4 bytes with the desired FourCC \
         or register the codec via `CodecInfo::tags(...)`",
        params.codec_id
    )))
}

/// Pick the wFormatTag for an audio stream. Synthesises the tag for the
/// PCM families directly from `sample_format` because the same
/// `wFormatTag` value is shared by every depth in the integer-PCM and
/// IEEE-float-PCM groups; otherwise consults the registry's inverse
/// lookup.
fn audio_format_tag(params: &CodecParameters, codecs: &dyn CodecResolver) -> Result<u16> {
    if let Some(synth) = pcm_synth_format_tag(&params.codec_id) {
        return Ok(synth);
    }
    if let Some(CodecTag::WaveFormat(t)) =
        codecs.tag_for_codec(&params.codec_id, CodecTagKind::WaveFormat)
    {
        return Ok(t);
    }
    Err(Error::unsupported(format!(
        "avi muxer: codec `{}` has no registered WAVEFORMATEX wFormatTag",
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

fn build_video_strf(params: &CodecParameters, codecs: &dyn CodecResolver) -> Result<StrfEntry> {
    let width = params
        .width
        .ok_or_else(|| Error::invalid("avi muxer: video stream missing width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("avi muxer: video stream missing height"))?;
    let fourcc = video_fourcc(params, codecs)?;
    // bit_count: 24 for compressed bitstreams (the conventional advisory
    // value); for BI_RGB we use 24 too (24-bit packed RGB is the
    // canonical uncompressed AVI pixel format we package).
    let bit_count: u16 = 24;
    let strf = write_bitmap_info_header(width, height, fourcc, bit_count, &params.extradata);
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

fn build_audio_strf(params: &CodecParameters, codecs: &dyn CodecResolver) -> Result<StrfEntry> {
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("avi muxer: audio stream missing channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("avi muxer: audio stream missing sample_rate"))?;
    let format_tag = audio_format_tag(params, codecs)?;
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
    use oxideav_core::{CodecCapabilities, CodecInfo, CodecRegistry};

    fn registry_with(id: &str, tags: &[CodecTag]) -> CodecRegistry {
        let mut reg = CodecRegistry::new();
        let mut info = CodecInfo::new(CodecId::new(id)).capabilities(CodecCapabilities::audio(id));
        for t in tags {
            info = info.tag(t.clone());
        }
        reg.register(info);
        reg
    }

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
    fn video_fourcc_reads_extradata_hint() {
        // Codec is unregistered, but extradata hint wins.
        let reg = CodecRegistry::new();
        let mut p = CodecParameters::video(CodecId::new("magicyuv"));
        p.width = Some(64);
        p.height = Some(64);
        p.extradata = b"M8YA-extra".to_vec();
        let fc = video_fourcc(&p, &reg).unwrap();
        assert_eq!(&fc, b"M8YA");
    }

    #[test]
    fn video_fourcc_falls_through_to_registry() {
        let reg = registry_with("magicyuv", &[CodecTag::fourcc(b"M8RG")]);
        let mut p = CodecParameters::video(CodecId::new("magicyuv"));
        p.width = Some(64);
        p.height = Some(64);
        let fc = video_fourcc(&p, &reg).unwrap();
        assert_eq!(&fc, b"M8RG");
    }

    #[test]
    fn video_fourcc_unknown_codec_errors() {
        let reg = CodecRegistry::new();
        let mut p = CodecParameters::video(CodecId::new("noexist"));
        p.width = Some(64);
        p.height = Some(64);
        match video_fourcc(&p, &reg) {
            Err(Error::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn rgb24_uses_bi_rgb_sentinel() {
        // No registry claim, no extradata hint → the codec_id-side
        // synthetic for BI_RGB is a special case.
        let reg = CodecRegistry::new();
        let mut p = CodecParameters::video(CodecId::new("rgb24"));
        p.width = Some(64);
        p.height = Some(64);
        let fc = video_fourcc(&p, &reg).unwrap();
        assert_eq!(&fc, &[0, 0, 0, 0]);
    }

    #[test]
    fn pcm_format_tag_is_synthesised() {
        // PCM codecs share wFormatTag values, so the muxer doesn't
        // need a registered claim for the inverse direction.
        let reg = CodecRegistry::new();
        let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        let entry = build_strf(&p, &reg).unwrap();
        assert_eq!(&entry.strh_type, b"auds");
        assert_eq!(entry.sample_size, 4); // 2ch × 2B
    }

    #[test]
    fn compressed_audio_uses_registry_tag() {
        let reg = registry_with("mp3", &[CodecTag::wave_format(0x0055)]);
        let mut p = CodecParameters::audio(CodecId::new("mp3"));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        let entry = build_strf(&p, &reg).unwrap();
        assert_eq!(&entry.strh_type, b"auds");
        // First 2 bytes of the WAVEFORMATEX are the wFormatTag in LE.
        assert_eq!(&entry.strf[0..2], &0x0055u16.to_le_bytes());
    }

    #[test]
    fn unknown_audio_codec_errors() {
        let reg = CodecRegistry::new();
        let mut p = CodecParameters::audio(CodecId::new("noexist"));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        match build_strf(&p, &reg) {
            Err(Error::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn magicyuv_extradata_hint_overrides_registry() {
        // Registry says M8RG but extradata says M8YA.
        let reg = registry_with(
            "magicyuv",
            &[CodecTag::fourcc(b"M8RG"), CodecTag::fourcc(b"M8YA")],
        );
        let mut p = CodecParameters::video(CodecId::new("magicyuv"));
        p.width = Some(64);
        p.height = Some(64);
        p.extradata = b"M8YAtail".to_vec();
        let entry = build_strf(&p, &reg).unwrap();
        assert_eq!(&entry.handler_fourcc, b"M8YA");
        assert!(entry.strf.ends_with(b"M8YAtail"));
    }
}
