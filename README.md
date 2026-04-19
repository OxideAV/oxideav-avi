# oxideav-avi

Pure-Rust **AVI (RIFF)** container — demuxer + muxer for the legacy
Microsoft AVI 1.0 format with a wide FourCC / WAVEFORMATEX mapping table
into stable oxideav codec ids. Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-container = "0.0"
oxideav-avi = "0.0"
```

## Feature matrix

| Feature                                          | Demux | Mux  |
|--------------------------------------------------|:-----:|:----:|
| RIFF/AVI 1.0 (hdrl/strl/movi/idx1)               | yes   | yes  |
| Multi-stream (video + audio + ...)               | yes   | yes  |
| `LIST INFO` metadata (title/artist/album/...)    | yes   | no   |
| Duration from `avih` (microseconds-per-frame)    | yes   | n/a  |
| `idx1` legacy index — parse for keyframe seek    | yes   | yes  |
| `idx1` offsets — file-absolute + movi-relative   | yes   | yes  |
| `LIST rec ` packet grouping inside `movi`        | yes   | no   |
| OpenDML (AVI 2.0) `indx` / `ix##` / > 2 GiB      | no    | no   |
| Uncompressed `db` video chunks                   | yes   | yes  |
| Variable stream interleave                       | yes   | yes  |
| Palette-change (`pc`) chunks                     | skip  | no   |

OpenDML is explicitly out of scope. The muxer refuses to grow past
~2 GiB and returns `Error::Unsupported`.

## Codec mapping

AVI identifies video streams by a 4-byte `biCompression` FourCC in the
`strf`/BITMAPINFOHEADER, and audio streams by a 16-bit `wFormatTag` in
the `strf`/WAVEFORMATEX. `oxideav-avi` translates these both ways into
the stable codec ids used everywhere else in oxideav.

FourCC matching is case-insensitive. Unknown tags round-trip as
`avi:<fourcc>` (video) or `avi:tag_xxxx` (audio) rather than failing —
so a caller can still walk an unrecognised stream, it just won't find a
decoder.

### Video FourCC to codec id

| Codec id       | FourCCs accepted (case-insensitive)                                                             |
|----------------|-------------------------------------------------------------------------------------------------|
| `mjpeg`        | `MJPG`, `AVRN`, `LJPG`, `JPGL`                                                                  |
| `ffv1`         | `FFV1`                                                                                          |
| `h263`         | `H263`, `U263`, `M263`, `ILVR`, `VX1K`, `VIV1`, `X263`, `T263`, `S263`, `L263`                  |
| `h264`         | `H264`, `AVC1`, `X264`, `VSSH`, `DAVC`, `PAVC`, `AVC2`, `AVC3`, AVC-Intra `AI13`/`AI15`/...     |
| `h265`         | `HEVC`, `H265`, `HVC1`, `HEV1`, `X265`, `DXHE`                                                  |
| `mpeg1video`   | `MPG1`, `MPEG`                                                                                  |
| `mpeg2video`   | `MPG2`, `MP2V`, `EM2V`, HDV flavours `HDV1`..`HDV9`                                             |
| `mpeg4video`   | `XVID`, `DIVX`, `DX50`, `DX40`, `MP4V`, `FMP4`, `DIV3`..`DIV6`, `3IV2`, `M4S2`, `MP4S`, `BLZ0`, `DXGM`, `RMP4`, `SMP4`, `UMP4`, `XVIX`, `WV1F`, `DIVF`, `MP43` |
| `vp6`          | `VP60`, `VP61`, `VP62`, `VP6F`, `VP6A`                                                          |
| `vp8` / `vp9`  | `VP80`/`VP8 `, `VP90`/`VP9 `                                                                    |
| `av1`          | `AV01`                                                                                          |
| `theora`       | `THEO`                                                                                          |
| `huffyuv` / `ffvhuff` | `HFYU`, `FFVH`                                                                           |
| `utvideo`      | `UTVS`, `ULRG`, `ULRA`, `ULY0`/`ULY2`/`ULY4`, `ULH0`/`ULH2`/`ULH4`                              |
| `prores`       | `APCH`, `APCN`, `APCS`, `APCO`, `AP4H`, `AP4X`                                                  |
| `dv`           | `DVSD`, `DV25`, `DV50`, `DVC `, `DVCP`, `DVHD`, `DVH1`                                          |
| `wmv1`/`wmv2`/`wmv3` | `WMV1`, `WMV2`, `WMV3`                                                                    |
| `vc1`          | `WVC1`, `WMVA`                                                                                  |
| `cinepak`      | `CVID`                                                                                          |
| `indeo3`/`indeo4`/`indeo5` | `IV31`/`IV32`, `IV41`/`IV42`, `IV50`                                                  |
| `svq1` / `svq3`| `SVQ1`, `SVQ3`                                                                                  |
| `flv1`         | `FLV1`                                                                                          |
| `rgb24`        | `BI_RGB` (all-zero FourCC), `DIB `, `RGB `, `RAW `                                              |
| `yuyv422` / `uyvy422` | `YUY2`/`YUYV`/`V422`/`YUNV`, `UYVY`/`Y422`/`UYNV`                                        |
| `nv12` / `nv21`| `NV12`, `NV21`                                                                                  |
| `yuv420p`      | `I420`, `IYUV`, `YV12`                                                                          |
| `yuv411p`      | `Y41P`                                                                                          |
| `yuv444p10le` / `v210` / `r210` | `V410`, `V210`, `R210`/`R10K`                                                  |

### Audio wFormatTag to codec id

| Codec id       | WAVEFORMATEX tag(s)                                            |
|----------------|----------------------------------------------------------------|
| `pcm_u8`       | `0x0001` with `wBitsPerSample = 8`                             |
| `pcm_s16le`    | `0x0001` with `wBitsPerSample = 16`                            |
| `pcm_s24le`    | `0x0001` with `wBitsPerSample = 24`                            |
| `pcm_s32le`    | `0x0001` with `wBitsPerSample = 32`                            |
| `pcm_f32le` / `pcm_f64le` | `0x0003` with 32 / 64 bits per sample               |
| `pcm_alaw`     | `0x0006`                                                       |
| `pcm_mulaw`    | `0x0007`                                                       |
| `adpcm_ms`     | `0x0002`                                                       |
| `adpcm_ima_wav`| `0x0011`                                                       |
| `adpcm_yamaha` | `0x0020`                                                       |
| `adpcm_ima_dk4`/`adpcm_ima_dk3` | `0x0061`, `0x0062`                            |
| `g722`         | `0x0028`                                                       |
| `g723_1`       | `0x0014`                                                       |
| `gsm_ms`       | `0x0031`, `0x0032`                                             |
| `mp2`          | `0x0050`                                                       |
| `mp3`          | `0x0055`                                                       |
| `aac`          | `0x00FF`, `0x706D`, `0x4143`, `0xA106`                         |
| `wmav1`/`wmav2`/`wmapro`/`wmalossless` | `0x0160`, `0x0161`, `0x0162`, `0x0163` |
| `ac3`          | `0x2000`, `0x6AC3`                                             |
| `eac3`         | `0x2001`, `0xEAC3`                                             |
| `dts`          | `0x2002`, `0x0008`                                             |
| `opus`         | `0x4F70`, `0x704F`, `0x7075`                                   |
| `vorbis`       | `0x674F`/`0x6750`/`0x6751`/`0x676F`/`0x6770`/`0x6771`          |
| `flac`         | `0xF1AC`                                                       |

Muxer-side packaging (`strf` + `strh`) is implemented for a subset of
the above: `mjpeg`, `ffv1`, `mpeg1video`, `mpeg2video`, `mpeg4video`,
`h263`, `h264`, `h265`, `vp8`, `vp9`, `av1`, `rgb24`, all six raw PCM
variants (`pcm_u8`/`s16le`/`s24le`/`s32le`/`f32le`/`f64le`),
`pcm_alaw`, `pcm_mulaw`, `mp2`, `mp3`, `aac`, `ac3`, `eac3`, `flac`.
Other codec ids return `Error::Unsupported` at `open()`.

## Quick use

### Demux

```rust
use oxideav_container::{ContainerRegistry, ReadSeek};

let mut containers = ContainerRegistry::new();
oxideav_avi::register(&mut containers);

let input: Box<dyn ReadSeek> =
    Box::new(std::io::Cursor::new(std::fs::read("capture.avi")?));
let mut dmx = containers.open_demuxer("avi", input)?;

for s in dmx.streams() {
    eprintln!("stream {}: {} ({:?})", s.index, s.params.codec_id, s.params.media_type);
}

loop {
    match dmx.next_packet() {
        Ok(pkt) => {
            // feed into the matching decoder
        }
        Err(oxideav_core::Error::Eof) => break,
        Err(e) => return Err(e.into()),
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`seek_to(stream, pts)` lands on the nearest keyframe at or before the
target, using the `idx1` index. Files without `idx1` (streamed AVI,
half-written recordings) return `Error::Unsupported` for seek; linear
decode still works.

### Mux

```rust
use oxideav_container::{ContainerRegistry, WriteSeek};
use oxideav_core::{CodecId, CodecParameters, MediaType, Packet,
                   Rational, SampleFormat, StreamInfo, TimeBase};

let mut containers = ContainerRegistry::new();
oxideav_avi::register(&mut containers);

// One video + one audio stream.
let mut v = CodecParameters::video(CodecId::new("mjpeg"));
v.width = Some(640);
v.height = Some(480);
v.frame_rate = Some(Rational::new(25, 1));

let mut a = CodecParameters::audio(CodecId::new("pcm_s16le"));
a.channels = Some(2);
a.sample_rate = Some(48_000);
a.sample_format = Some(SampleFormat::S16);

let streams = [
    StreamInfo { index: 0, time_base: TimeBase::new(1, 25),
                 duration: None, start_time: Some(0), params: v },
    StreamInfo { index: 1, time_base: TimeBase::new(1, 48_000),
                 duration: None, start_time: Some(0), params: a },
];

let out: Box<dyn WriteSeek> =
    Box::new(std::fs::File::create("out.avi")?);
let mut mux = containers.open_muxer("avi", out, &streams)?;
mux.write_header()?;
// mux.write_packet(&pkt)? for each encoded packet, interleaved
mux.write_trailer()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The muxer always emits a legacy `idx1` index at the end of the file so
the resulting AVI is seekable. The 2 GiB ceiling is enforced strictly —
if `write_packet` would push the file past ~2 GiB, it returns
`Error::Unsupported` so the caller can roll a new segment.

### Container id

- Container: `"avi"`, matches `.avi` by extension and the
  `RIFF....AVI ` magic.
- Registered via `oxideav_avi::register(&mut ContainerRegistry)`.

## License

MIT — see [LICENSE](LICENSE).
