//! Shared by the black-box matrix test and the `avi_tool` example: a
//! codec registry holding the tag claims the codec crates make (the
//! README "Codec mapping" tables), and the one-line listing forms for a
//! stream and a packet.

#![allow(dead_code)] // each includer uses its own subset

use oxideav_core::{CodecId, CodecInfo, CodecRegistry, CodecTag, MediaType, Packet, StreamInfo};

/// Video codec id → FourCCs claimed for it.
pub const VIDEO_CLAIMS: &[(&str, &[&[u8; 4]])] = &[
    ("mjpeg", &[b"MJPG", b"AVRN", b"LJPG", b"JPGL"]),
    ("ffv1", &[b"FFV1"]),
    ("h261", &[b"H261"]),
    ("h263", &[b"H263", b"U263", b"M263", b"S263", b"L263"]),
    ("h264", &[b"H264", b"AVC1", b"X264", b"DAVC"]),
    ("h265", &[b"HEVC", b"H265", b"HVC1", b"HEV1", b"X265"]),
    ("mpeg1video", &[b"MPG1", b"MPEG"]),
    ("mpeg2video", &[b"MPG2", b"MP2V"]),
    (
        "mpeg4video",
        &[b"XVID", b"DIVX", b"DX50", b"MP4V", b"FMP4", b"M4S2"],
    ),
    ("msmpeg4v1", &[b"MPG4"]),
    ("msmpeg4v2", &[b"MP42"]),
    ("msmpeg4v3", &[b"MP43", b"DIV3"]),
    ("vp6", &[b"VP60", b"VP61", b"VP62"]),
    ("vp8", &[b"VP80"]),
    ("vp9", &[b"VP90"]),
    ("av1", &[b"AV01"]),
    ("theora", &[b"THEO"]),
    ("huffyuv", &[b"HFYU"]),
    ("ffvhuff", &[b"FFVH"]),
    ("lagarith", &[b"LAGS"]),
    (
        "utvideo",
        &[
            b"ULRG", b"ULRA", b"ULY0", b"ULY2", b"ULY4", b"ULH0", b"ULH2", b"ULH4",
        ],
    ),
    (
        "magicyuv",
        &[
            b"MAGY", b"M8RG", b"M8RA", b"M8Y4", b"M8Y2", b"M8Y0", b"M8G0",
        ],
    ),
    ("prores", &[b"APCH", b"APCN", b"APCS", b"APCO", b"AP4H"]),
    ("dv", &[b"DVSD", b"DV25", b"DV50", b"DVHD"]),
    ("wmv1", &[b"WMV1"]),
    ("wmv2", &[b"WMV2"]),
    ("cinepak", &[b"CVID"]),
    ("indeo3", &[b"IV31", b"IV32"]),
    ("svq1", &[b"SVQ1"]),
    ("flv1", &[b"FLV1"]),
];

/// Audio codec id → `wFormatTag`s claimed for it.
pub const AUDIO_CLAIMS: &[(&str, &[u16])] = &[
    ("pcm_alaw", &[0x0006]),
    ("pcm_mulaw", &[0x0007]),
    ("adpcm_ms", &[0x0002]),
    ("adpcm_ima_wav", &[0x0011]),
    ("adpcm_yamaha", &[0x0020]),
    ("g723_1", &[0x0014]),
    ("gsm_ms", &[0x0031]),
    ("mp2", &[0x0050]),
    ("mp3", &[0x0055]),
    ("aac", &[0x00FF, 0x706D]),
    ("wmav1", &[0x0160]),
    ("wmav2", &[0x0161]),
    ("ac3", &[0x2000]),
    ("eac3", &[0x2001]),
    ("dts", &[0x2002]),
    ("vorbis", &[0x566F, 0x674F, 0x6750, 0x6751]),
    ("flac", &[0xF1AC]),
];

/// A registry holding every claim above (no codec implementations).
pub fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    for (id, fourccs) in VIDEO_CLAIMS {
        reg.register(
            CodecInfo::new(CodecId::new(*id)).tags(fourccs.iter().map(|f| CodecTag::fourcc(f))),
        );
    }
    for (id, tags) in AUDIO_CLAIMS {
        reg.register(
            CodecInfo::new(CodecId::new(*id)).tags(tags.iter().map(|t| CodecTag::wave_format(*t))),
        );
    }
    reg
}

/// IEEE 802.3 CRC-32 (reflected, polynomial 0xEDB88320).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_string(), |v| v.to_string())
}

pub fn tag_text(tag: &Option<CodecTag>) -> String {
    match tag {
        Some(CodecTag::Fourcc(f)) => {
            if f.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
                String::from_utf8_lossy(f).replace(' ', "_")
            } else {
                format!("0x{:08x}", u32::from_le_bytes(*f))
            }
        }
        Some(CodecTag::WaveFormat(t)) => format!("0x{t:04x}"),
        Some(other) => format!("{other:?}"),
        None => "-".to_string(),
    }
}

pub fn media_text(m: MediaType) -> &'static str {
    match m {
        MediaType::Audio => "audio",
        MediaType::Video => "video",
        MediaType::Subtitle => "subtitle",
        MediaType::Data => "data",
        _ => "unknown",
    }
}

pub fn stream_line(s: &StreamInfo) -> String {
    let p = &s.params;
    let mut line = format!(
        "S {} {} {} tag={} tb={}/{} start={} dur={}",
        s.index,
        media_text(p.media_type),
        p.codec_id.as_str(),
        tag_text(&p.tag),
        s.time_base.0.num,
        s.time_base.0.den,
        opt(s.start_time),
        opt(s.duration),
    );
    match p.media_type {
        MediaType::Video => {
            line.push_str(&format!(
                " size={}x{} pix={} fr={}",
                opt(p.width),
                opt(p.height),
                p.pixel_format
                    .map_or_else(|| "-".to_string(), |f| format!("{f:?}")),
                p.frame_rate
                    .map_or_else(|| "-".to_string(), |r| format!("{}/{}", r.num, r.den)),
            ));
        }
        MediaType::Audio => {
            line.push_str(&format!(
                " rate={} ch={} fmt={}",
                opt(p.sample_rate),
                opt(p.channels),
                p.sample_format
                    .map_or_else(|| "-".to_string(), |f| format!("{f:?}")),
            ));
        }
        _ => {}
    }
    line.push_str(&format!(
        " br={} lang={} extradata={}:{:08x}",
        opt(p.bit_rate),
        p.language.as_deref().unwrap_or("-"),
        p.extradata.len(),
        crc32(&p.extradata),
    ));
    line
}

pub fn packet_line(p: &Packet) -> String {
    format!(
        "P {} {} {} {} {} {} {:08x}",
        p.stream_index,
        opt(p.pts),
        opt(p.dts),
        opt(p.duration),
        p.data.len(),
        if p.flags.keyframe { "K" } else { "_" },
        crc32(&p.data),
    )
}
