//! AVI ↔ oxideav codec id mapping.
//!
//! The demuxer reads the 4-byte `biCompression` FourCC for video streams and
//! the 16-bit `wFormatTag` for audio streams, and maps them to the stable
//! oxideav `codec_id` strings used by the rest of the pipeline.
//!
//! The muxer goes the other way: given a `CodecParameters`, it returns the
//! payload bytes to place in a `strf` chunk (a `BITMAPINFOHEADER` for video
//! or a `WAVEFORMATEX` for audio) plus the 4-byte chunk suffix used to tag
//! its packets inside the `movi` list (`dc` for video, `wb` for audio).
//!
//! This is the one and only codec-aware module in the crate.

use oxideav_core::{CodecId, CodecParameters, Error, MediaType, Result};

use crate::stream_format::{write_bitmap_info_header, write_waveformatex};

/// FourCC → codec_id for video streams. Uppercase keys to normalise the
/// case-insensitive FourCCs some encoders emit (e.g. `mjpg`/`MJPG`/`MJpg`).
pub fn video_codec_id(fourcc: &[u8; 4]) -> CodecId {
    let upper = uppercase4(fourcc);
    let name = match &upper {
        b"MJPG" | b"AVRN" | b"LJPG" | b"JPGL" => "mjpeg",
        b"FFV1" => "ffv1",
        // MPEG-4 Part 2 / ASP (ISO/IEC 14496-2) — every non-trivial MP4/AVI
        // encoder that targets the real ISO spec emits one of these FourCCs.
        // Handled by oxideav-mpeg4video.
        b"XVID" | b"DIVX" | b"DX50" | b"MP4V" | b"FMP4" | b"3IV2" | b"M4S2" | b"MP4S" | b"DIVF"
        | b"BLZ0" | b"DX40" | b"RMP4" | b"SMP4" | b"UMP4" | b"WV1F" | b"XVIX" | b"DXGM" => {
            "mpeg4video"
        }
        // Microsoft MPEG-4 family — the pre-standard Windows Media / DivX ;-)
        // codecs. **Not** the same bitstream as MPEG-4 Part 2 despite the
        // similar FourCCs — different VLC tables, no VOS/VOL headers, different
        // slice layout. Handled by oxideav-msmpeg4.
        //
        // Note: `DIV3` / `DIV4` / `MP43` and friends are the most-mislabelled
        // FourCCs in the wild — files often claim DIV3 but carry an actual
        // MPEG-4 Part 2 (XVID/DX50) stream. The oxideav-msmpeg4 decoder probes
        // the bitstream in send_packet and returns Unsupported with a
        // "dispatch to oxideav-mpeg4video" hint when it sees an ISO start code.
        b"DIV3" | b"DIV4" | b"DIV5" | b"DIV6" | b"MP43" | b"MPG3" | b"AP41" => "msmpeg4v3",
        b"MP42" => "msmpeg4v2",
        b"MP41" | b"MPG4" => "msmpeg4v1",
        // H.264 / AVC — `H264`, `AVC1`, plus Sony/JVC variants and a few
        // widely-seen DVR FourCCs. We accept both in-stream (annex-B) and
        // `avcC`-prefixed bitstreams; downstream decoders adapt.
        b"H264" | b"AVC1" | b"X264" | b"VSSH" | b"DAVC" | b"PAVC" | b"AVC2" | b"AVC3"
        | b"AI5Q" | b"AI55" | b"AI15" | b"AI13" | b"AI12" | b"AI1Q" | b"AI5P" | b"AI53" => "h264",
        // HEVC / H.265.
        b"HEVC" | b"H265" | b"HVC1" | b"HEV1" | b"X265" | b"DXHE" => "h265",
        // ITU-T H.263 baseline / H.263+. `U263` (UB Video), `M263`, `ILVR`,
        // `VX1K` and `viv1` (VivoActive) all pack an H.263 bitstream.
        b"H263" | b"U263" | b"M263" | b"ILVR" | b"VX1K" | b"VIV1" | b"X263" | b"T263"
        | b"S263" | b"L263" => "h263",
        // MPEG-1 video. `MPG1` is the most common AVI tag; `MPEG` appears in a
        // few legacy files. `mpg1`/`mpeg` fall through via uppercase4.
        b"MPG1" | b"MPEG" => "mpeg1video",
        // MPEG-2 video.
        b"MPG2" | b"MP2V" | b"EM2V" | b"HDV1" | b"HDV2" | b"HDV3" | b"HDV4" | b"HDV5"
        | b"HDV6" | b"HDV7" | b"HDV8" | b"HDV9" => "mpeg2video",
        // VP8 / VP9 / AV1.
        b"VP80" | b"VP8 " => "vp8",
        b"VP90" | b"VP9 " => "vp9",
        b"AV01" => "av1",
        b"THEO" => "theora",
        // Huffyuv / FFV1-adjacent lossless intermediates.
        b"HFYU" => "huffyuv",
        b"FFVH" => "ffvhuff",
        b"UTVS" | b"ULRG" | b"ULRA" | b"ULY0" | b"ULY2" | b"ULY4" | b"ULH0" | b"ULH2"
        | b"ULH4" => "utvideo",
        // ProRes (rare in AVI but seen in post-production workflows).
        b"APCH" | b"APCN" | b"APCS" | b"APCO" | b"AP4H" | b"AP4X" => "prores",
        // DV video.
        b"DVSD" | b"DV25" | b"DV50" | b"DVC " | b"DVCP" | b"DVHD" | b"DVH1" => "dv",
        // Windows Media Video family.
        b"WMV1" => "wmv1",
        b"WMV2" => "wmv2",
        b"WMV3" => "wmv3",
        b"WVC1" | b"WMVA" => "vc1",
        // Cinepak / Indeo / Sorenson — old MS/Intel/Apple codecs that still
        // turn up in legacy AVI captures.
        b"CVID" => "cinepak",
        b"IV31" | b"IV32" => "indeo3",
        b"IV41" | b"IV42" => "indeo4",
        b"IV50" => "indeo5",
        b"SVQ1" => "svq1",
        b"SVQ3" => "svq3",
        b"FLV1" => "flv1",
        b"VP60" | b"VP61" | b"VP62" | b"VP6F" | b"VP6A" => "vp6",
        // Raw 4:2:2 / 4:2:0 packed or planar formats. BI_RGB (0) has FourCC
        // all-zeros. 24-bit RGB is the classic uncompressed AVI payload.
        [0, 0, 0, 0] | b"DIB " | b"RGB " | b"RAW " => "rgb24",
        b"YUY2" | b"YUYV" | b"V422" | b"YUNV" => "yuyv422",
        b"UYVY" | b"Y422" | b"UYNV" => "uyvy422",
        b"NV12" => "nv12",
        b"NV21" => "nv21",
        b"I420" | b"IYUV" | b"YV12" => "yuv420p",
        b"Y41P" => "yuv411p",
        b"V410" => "yuv444p10le",
        b"V210" => "v210",
        b"R210" | b"R10K" => "r210",
        other => {
            let s = std::str::from_utf8(other).unwrap_or("????");
            return CodecId::new(format!("avi:{s}"));
        }
    };
    CodecId::new(name)
}

/// WAVEFORMATEX wFormatTag → codec_id. For raw PCM (0x0001) and IEEE-float
/// (0x0003), the mapping also depends on `bits_per_sample`: see
/// [`audio_codec_id_full`] for the depth-aware flavour.
pub fn audio_codec_id(format_tag: u16) -> CodecId {
    audio_codec_id_full(format_tag, 0)
}

/// Depth-aware form of [`audio_codec_id`]. Uncompressed and float-PCM tags
/// branch on `bits_per_sample` to pick `pcm_u8` / `pcm_s16le` / `pcm_s24le`
/// / `pcm_s32le` / `pcm_f32le` / `pcm_f64le`. Other tags ignore the depth.
pub fn audio_codec_id_full(format_tag: u16, bits_per_sample: u16) -> CodecId {
    let name = match format_tag {
        // WAVE_FORMAT_PCM — pick integer flavour by depth.
        0x0001 => match bits_per_sample {
            0 | 16 => "pcm_s16le",
            8 => "pcm_u8",
            24 => "pcm_s24le",
            32 => "pcm_s32le",
            _ => "pcm_s16le",
        },
        // WAVE_FORMAT_ADPCM (Microsoft ADPCM).
        0x0002 => "adpcm_ms",
        // WAVE_FORMAT_IEEE_FLOAT — 32 or 64-bit native float.
        0x0003 => match bits_per_sample {
            64 => "pcm_f64le",
            _ => "pcm_f32le",
        },
        // ITU-T G.711 companded PCM.
        0x0006 => "pcm_alaw",
        0x0007 => "pcm_mulaw",
        // DVI / IMA ADPCM (bit-packed 4-bit) + Yamaha variant.
        0x0011 => "adpcm_ima_wav",
        0x0020 => "adpcm_yamaha",
        // G.723.1 6.3 kbit/s.
        0x0014 => "g723_1",
        // GSM 6.10.
        0x0031 | 0x0032 => "gsm_ms",
        // ITU-T G.722 (16-bit, SB-ADPCM).
        0x0028 => "g722",
        // MPEG-1 Layer II / Layer I container tag.
        0x0050 => "mp2",
        0x0055 => "mp3",
        // WAVE_FORMAT_DK4_ADPCM / DK3 — DVI-variant ADPCM used by Sega CDs.
        0x0061 => "adpcm_ima_dk4",
        0x0062 => "adpcm_ima_dk3",
        // AAC: several historical tags in the wild.
        0x00FF | 0x706D | 0x4143 | 0xA106 => "aac",
        // Windows Media Audio lineage.
        0x0160 => "wmav1",
        0x0161 => "wmav2",
        0x0162 => "wmapro",
        0x0163 => "wmalossless",
        // AC-3 / E-AC-3 / DTS.
        0x2000 | 0x6AC3 => "ac3",
        0x2001 => "eac3",
        0x2002 | 0x0008 => "dts",
        // Opus / Vorbis / FLAC (non-standard but seen in AVI).
        0x4F70 | 0x704F | 0x7075 => "opus",
        0x674F | 0x6750 | 0x6751 | 0x676F | 0x6770 | 0x6771 => "vorbis",
        0xF1AC => "flac",
        // TrueHD / RealAudio passthrough tags occasionally seen in AVI.
        0xEAC3 => "eac3",
        other => return CodecId::new(format!("avi:tag_{other:04x}")),
    };
    CodecId::new(name)
}

/// Result of building a stream-format chunk for the muxer.
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

/// Build the `strf` chunk + `strh` metadata for the given stream. Errors with
/// `Unsupported` if the codec has no AVI packaging in our table.
pub(crate) fn build_strf(params: &CodecParameters) -> Result<StrfEntry> {
    match params.codec_id.as_str() {
        "mjpeg" => mjpeg_entry(params),
        "ffv1" => ffv1_entry(params),
        "mpeg4video" => video_entry(params, *b"XVID", 24),
        "h264" => video_entry(params, *b"H264", 24),
        "h265" => video_entry(params, *b"HEVC", 24),
        "h263" => video_entry(params, *b"H263", 24),
        "mpeg1video" => video_entry(params, *b"MPG1", 24),
        "mpeg2video" => video_entry(params, *b"MPG2", 24),
        "vp8" => video_entry(params, *b"VP80", 24),
        "vp9" => video_entry(params, *b"VP90", 24),
        "av1" => video_entry(params, *b"AV01", 24),
        "rgb24" => rgb24_entry(params),
        "pcm_u8" => pcm_int_entry(params, 0x0001, 8),
        "pcm_s16le" => pcm_int_entry(params, 0x0001, 16),
        "pcm_s24le" => pcm_int_entry(params, 0x0001, 24),
        "pcm_s32le" => pcm_int_entry(params, 0x0001, 32),
        "pcm_f32le" => pcm_int_entry(params, 0x0003, 32),
        "pcm_f64le" => pcm_int_entry(params, 0x0003, 64),
        "pcm_alaw" => pcm_companded_entry(params, 0x0006),
        "pcm_mulaw" => pcm_companded_entry(params, 0x0007),
        "mp2" => mp_audio_entry(params, 0x0050),
        "mp3" => mp_audio_entry(params, 0x0055),
        "aac" => aac_entry(params),
        "ac3" => compressed_audio_entry(params, 0x2000),
        "eac3" => compressed_audio_entry(params, 0x2001),
        "flac" => compressed_audio_entry(params, 0xF1AC),
        other => Err(Error::unsupported(format!(
            "avi muxer: no packaging for codec {other}"
        ))),
    }
}

fn mjpeg_entry(params: &CodecParameters) -> Result<StrfEntry> {
    if params.media_type != MediaType::Video {
        return Err(Error::invalid("avi muxer: mjpeg must be video"));
    }
    let width = params
        .width
        .ok_or_else(|| Error::invalid("avi muxer: mjpeg requires width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("avi muxer: mjpeg requires height"))?;
    let strf = write_bitmap_info_header(width, height, *b"MJPG", 24, &params.extradata);
    let (scale, rate) = video_scale_rate(params);
    Ok(StrfEntry {
        chunk_suffix: *b"dc",
        handler_fourcc: *b"MJPG",
        strf,
        strh_type: *b"vids",
        sample_size: 0,
        scale,
        rate,
    })
}

fn ffv1_entry(params: &CodecParameters) -> Result<StrfEntry> {
    if params.media_type != MediaType::Video {
        return Err(Error::invalid("avi muxer: ffv1 must be video"));
    }
    let width = params
        .width
        .ok_or_else(|| Error::invalid("avi muxer: ffv1 requires width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("avi muxer: ffv1 requires height"))?;
    let strf = write_bitmap_info_header(width, height, *b"FFV1", 24, &params.extradata);
    let (scale, rate) = video_scale_rate(params);
    Ok(StrfEntry {
        chunk_suffix: *b"dc",
        handler_fourcc: *b"FFV1",
        strf,
        strh_type: *b"vids",
        sample_size: 0,
        scale,
        rate,
    })
}

/// Generic video `strf` builder: writes a BITMAPINFOHEADER with the given
/// FourCC and bit depth. Extradata from `params` is appended.
fn video_entry(
    params: &CodecParameters,
    fourcc: [u8; 4],
    bit_count: u16,
) -> Result<StrfEntry> {
    if params.media_type != MediaType::Video {
        return Err(Error::invalid("avi muxer: video codec on non-video stream"));
    }
    let width = params
        .width
        .ok_or_else(|| Error::invalid("avi muxer: video stream missing width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("avi muxer: video stream missing height"))?;
    let strf = write_bitmap_info_header(width, height, fourcc, bit_count, &params.extradata);
    let (scale, rate) = video_scale_rate(params);
    Ok(StrfEntry {
        chunk_suffix: *b"dc",
        handler_fourcc: fourcc,
        strf,
        strh_type: *b"vids",
        sample_size: 0,
        scale,
        rate,
    })
}

/// Uncompressed 24-bit RGB (`BI_RGB`). Payload chunks are tagged `db`.
fn rgb24_entry(params: &CodecParameters) -> Result<StrfEntry> {
    if params.media_type != MediaType::Video {
        return Err(Error::invalid("avi muxer: rgb24 must be video"));
    }
    let width = params
        .width
        .ok_or_else(|| Error::invalid("avi muxer: rgb24 requires width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("avi muxer: rgb24 requires height"))?;
    let strf = write_bitmap_info_header(width, height, [0, 0, 0, 0], 24, &params.extradata);
    let (scale, rate) = video_scale_rate(params);
    Ok(StrfEntry {
        chunk_suffix: *b"db",
        handler_fourcc: [0, 0, 0, 0],
        strf,
        strh_type: *b"vids",
        sample_size: 0,
        scale,
        rate,
    })
}

/// Integer or float PCM: `dwSampleSize = block_align` so any integer number
/// of frames fits in a chunk.
fn pcm_int_entry(
    params: &CodecParameters,
    format_tag: u16,
    bits_per_sample: u16,
) -> Result<StrfEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("avi muxer: pcm must be audio"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("avi muxer: pcm requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("avi muxer: pcm requires sample_rate"))?;
    let block_align = channels * (bits_per_sample / 8);
    let avg_bytes_per_sec = sample_rate * block_align as u32;
    let strf = write_waveformatex(
        format_tag,
        channels,
        sample_rate,
        avg_bytes_per_sec,
        block_align,
        bits_per_sample,
        &[],
    );
    Ok(StrfEntry {
        chunk_suffix: *b"wb",
        handler_fourcc: *b"\0\0\0\0",
        strf,
        strh_type: *b"auds",
        sample_size: block_align as u32,
        scale: 1,
        rate: sample_rate,
    })
}

/// G.711 μ-law / A-law: 1 byte per sample, so `block_align = channels`.
fn pcm_companded_entry(params: &CodecParameters, format_tag: u16) -> Result<StrfEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("avi muxer: companded pcm must be audio"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("avi muxer: pcm requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("avi muxer: pcm requires sample_rate"))?;
    let block_align = channels; // 1 byte per sample
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
    Ok(StrfEntry {
        chunk_suffix: *b"wb",
        handler_fourcc: *b"\0\0\0\0",
        strf,
        strh_type: *b"auds",
        sample_size: block_align as u32,
        scale: 1,
        rate: sample_rate,
    })
}

/// MPEG Layer II / III: VBR-capable, so `dwSampleSize = 0` and AVI players
/// treat each chunk as one frame.
fn mp_audio_entry(params: &CodecParameters, format_tag: u16) -> Result<StrfEntry> {
    compressed_audio_entry(params, format_tag)
}

/// AAC in AVI. Uses the `mp4a` (0x706D) tag pair widely by lavf/ffmpeg;
/// extradata carries AudioSpecificConfig when the encoder populates it.
fn aac_entry(params: &CodecParameters) -> Result<StrfEntry> {
    compressed_audio_entry(params, 0x00FF)
}

/// Shared audio packaging helper for VBR / compressed codecs (ac3, eac3,
/// flac, aac, mp2/mp3). One chunk per encoded frame — `sample_size = 0`
/// tells decoders to treat chunk boundaries as frame boundaries.
fn compressed_audio_entry(params: &CodecParameters, format_tag: u16) -> Result<StrfEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("avi muxer: audio codec on non-audio stream"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("avi muxer: audio stream missing channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("avi muxer: audio stream missing sample_rate"))?;
    // avg_bytes_per_sec is advisory; use bit_rate / 8 if set, otherwise zero.
    let avg_bytes_per_sec = params.bit_rate.map(|b| (b / 8) as u32).unwrap_or(0);
    // Fabricate a plausible block_align (1 = "variable" per Microsoft docs).
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

fn video_scale_rate(params: &CodecParameters) -> (u32, u32) {
    // dwRate / dwScale = frames per second.
    if let Some(fr) = params.frame_rate {
        let num = fr.num.max(1) as u32;
        let den = fr.den.max(1) as u32;
        return (den, num);
    }
    (1, 25) // default 25 fps if unknown
}

fn uppercase4(s: &[u8; 4]) -> [u8; 4] {
    let mut out = *s;
    for b in out.iter_mut() {
        if b.is_ascii_lowercase() {
            *b -= 32;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_mapping() {
        assert_eq!(video_codec_id(b"MJPG").as_str(), "mjpeg");
        assert_eq!(video_codec_id(b"mjpg").as_str(), "mjpeg");
        assert_eq!(video_codec_id(b"FFV1").as_str(), "ffv1");
        assert_eq!(video_codec_id(&[0, 0, 0, 0]).as_str(), "rgb24");
        // MPEG-4 Part 2 FourCCs — case-insensitive.
        assert_eq!(video_codec_id(b"XVID").as_str(), "mpeg4video");
        assert_eq!(video_codec_id(b"xvid").as_str(), "mpeg4video");
        assert_eq!(video_codec_id(b"DIVX").as_str(), "mpeg4video");
        assert_eq!(video_codec_id(b"divx").as_str(), "mpeg4video");
        assert_eq!(video_codec_id(b"DX50").as_str(), "mpeg4video");
        assert_eq!(video_codec_id(b"MP4V").as_str(), "mpeg4video");
        assert_eq!(video_codec_id(b"FMP4").as_str(), "mpeg4video");
        assert_eq!(video_codec_id(b"fmp4").as_str(), "mpeg4video");
        // H.263 variants.
        assert_eq!(video_codec_id(b"H263").as_str(), "h263");
        assert_eq!(video_codec_id(b"h263").as_str(), "h263");
        assert_eq!(video_codec_id(b"U263").as_str(), "h263");
        assert_eq!(video_codec_id(b"M263").as_str(), "h263");
        // MPEG-1 video.
        assert_eq!(video_codec_id(b"MPG1").as_str(), "mpeg1video");
        assert_eq!(video_codec_id(b"mpg1").as_str(), "mpeg1video");
        assert_eq!(video_codec_id(b"MPEG").as_str(), "mpeg1video");
    }

    #[test]
    fn video_mapping_extended() {
        // H.264 / H.265.
        assert_eq!(video_codec_id(b"H264").as_str(), "h264");
        assert_eq!(video_codec_id(b"avc1").as_str(), "h264");
        assert_eq!(video_codec_id(b"HEVC").as_str(), "h265");
        assert_eq!(video_codec_id(b"hev1").as_str(), "h265");
        // VP8 / VP9 / AV1.
        assert_eq!(video_codec_id(b"VP80").as_str(), "vp8");
        assert_eq!(video_codec_id(b"VP90").as_str(), "vp9");
        assert_eq!(video_codec_id(b"AV01").as_str(), "av1");
        // MPEG-2.
        assert_eq!(video_codec_id(b"MPG2").as_str(), "mpeg2video");
        assert_eq!(video_codec_id(b"mp2v").as_str(), "mpeg2video");
        // Raw / uncompressed YUV variants.
        assert_eq!(video_codec_id(b"YUY2").as_str(), "yuyv422");
        assert_eq!(video_codec_id(b"UYVY").as_str(), "uyvy422");
        assert_eq!(video_codec_id(b"NV12").as_str(), "nv12");
        assert_eq!(video_codec_id(b"I420").as_str(), "yuv420p");
        assert_eq!(video_codec_id(b"YV12").as_str(), "yuv420p");
        // DV / ProRes / WMV.
        assert_eq!(video_codec_id(b"DVSD").as_str(), "dv");
        assert_eq!(video_codec_id(b"APCH").as_str(), "prores");
        assert_eq!(video_codec_id(b"WMV3").as_str(), "wmv3");
        assert_eq!(video_codec_id(b"WVC1").as_str(), "vc1");
        // Cinepak / Indeo / Sorenson.
        assert_eq!(video_codec_id(b"CVID").as_str(), "cinepak");
        assert_eq!(video_codec_id(b"IV41").as_str(), "indeo4");
        assert_eq!(video_codec_id(b"SVQ3").as_str(), "svq3");
        assert_eq!(video_codec_id(b"FLV1").as_str(), "flv1");
    }

    #[test]
    fn audio_mapping() {
        assert_eq!(audio_codec_id(0x0001).as_str(), "pcm_s16le");
        assert_eq!(audio_codec_id(0x0055).as_str(), "mp3");
    }

    #[test]
    fn audio_mapping_extended() {
        // PCM variants by bit depth.
        assert_eq!(audio_codec_id_full(0x0001, 8).as_str(), "pcm_u8");
        assert_eq!(audio_codec_id_full(0x0001, 16).as_str(), "pcm_s16le");
        assert_eq!(audio_codec_id_full(0x0001, 24).as_str(), "pcm_s24le");
        assert_eq!(audio_codec_id_full(0x0001, 32).as_str(), "pcm_s32le");
        // IEEE float.
        assert_eq!(audio_codec_id_full(0x0003, 32).as_str(), "pcm_f32le");
        assert_eq!(audio_codec_id_full(0x0003, 64).as_str(), "pcm_f64le");
        // Companded PCM.
        assert_eq!(audio_codec_id(0x0006).as_str(), "pcm_alaw");
        assert_eq!(audio_codec_id(0x0007).as_str(), "pcm_mulaw");
        // ADPCM.
        assert_eq!(audio_codec_id(0x0002).as_str(), "adpcm_ms");
        assert_eq!(audio_codec_id(0x0011).as_str(), "adpcm_ima_wav");
        // Compressed audio.
        assert_eq!(audio_codec_id(0x0050).as_str(), "mp2");
        assert_eq!(audio_codec_id(0x0055).as_str(), "mp3");
        assert_eq!(audio_codec_id(0x00FF).as_str(), "aac");
        assert_eq!(audio_codec_id(0x2000).as_str(), "ac3");
        assert_eq!(audio_codec_id(0x2001).as_str(), "eac3");
        // WMA family.
        assert_eq!(audio_codec_id(0x0160).as_str(), "wmav1");
        assert_eq!(audio_codec_id(0x0161).as_str(), "wmav2");
        // Unknown tag → avi:tag_XXXX.
        assert_eq!(audio_codec_id(0xABCD).as_str(), "avi:tag_abcd");
    }

    #[test]
    fn unsupported_codec() {
        let p = CodecParameters::audio(CodecId::new("opus"));
        assert!(build_strf(&p).is_err());
    }

    #[test]
    fn mux_pcm_variants() {
        use oxideav_core::Rational;
        for id in &[
            "pcm_u8",
            "pcm_s16le",
            "pcm_s24le",
            "pcm_s32le",
            "pcm_f32le",
            "pcm_f64le",
            "pcm_alaw",
            "pcm_mulaw",
        ] {
            let mut p = CodecParameters::audio(CodecId::new(*id));
            p.channels = Some(2);
            p.sample_rate = Some(48_000);
            let e = build_strf(&p).expect(id);
            assert_eq!(&e.strh_type, b"auds", "{id}");
            assert!(e.rate == 48_000, "{id}");
        }
        // Video codecs.
        for id in &["mpeg4video", "h264", "h265", "h263", "vp8", "vp9", "av1"] {
            let mut p = CodecParameters::video(CodecId::new(*id));
            p.width = Some(320);
            p.height = Some(240);
            p.frame_rate = Some(Rational::new(25, 1));
            let e = build_strf(&p).expect(id);
            assert_eq!(&e.strh_type, b"vids", "{id}");
            assert_eq!(&e.chunk_suffix, b"dc", "{id}");
        }
    }
}
