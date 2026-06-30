//! Round-381: the remaining `BITMAPINFOHEADER` scalar fields the demuxer
//! parsed internally but never surfaced — `biSizeImage`,
//! `biXPelsPerMeter` / `biYPelsPerMeter`, `biClrUsed`, `biClrImportant`,
//! `biPlanes`.
//!
//! Per VfW `wingdi.h` §"BITMAPINFOHEADER":
//!   - `biSizeImage`   — the size, in bytes, of the image; *"may be set to
//!     zero for `BI_RGB` bitmaps"*.
//!   - `biXPelsPerMeter` / `biYPelsPerMeter` — the horizontal / vertical
//!     resolution, in pixels-per-meter, of the target device.
//!   - `biClrUsed`      — number of color indices used; `0` ⇒ the maximum
//!     for `biBitCount`.
//!   - `biClrImportant` — number of important color indices; `0` ⇒ all
//!     colors are important.
//!   - `biPlanes`       — the number of planes; *"must be set to 1."*
//!
//! Each accessor folds the field's documented "absent" sentinel to `None`
//! (`biPlanes` excepted — it has no absent value, so a non-conformant
//! stamp stays observable), and the matching `avi:vids.<n>.*` metadata key
//! is emitted only for the non-default value.
//!
//! Clean-room source:
//!   - `docs/container/riff/avi-riff-file-reference.md` §BITMAPINFOHEADER
//!   - `docs/container/riff/metadata/microsoft-riffmci.pdf`

use oxideav_avi::demuxer::open_avi;
use oxideav_core::{CodecRegistry, Demuxer, ReadSeek};

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

fn list(form: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(form);
    for c in children {
        body.extend_from_slice(c);
    }
    chunk(b"LIST", &body)
}

/// Build a minimal single-video-stream AVI carrying the supplied `strf`.
fn build_avi(strf: &[u8]) -> Vec<u8> {
    let mut avih = vec![0u8; 56];
    avih[24..28].copy_from_slice(&1u32.to_le_bytes()); // dwStreams = 1

    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"vids");
    strh[20..24].copy_from_slice(&1u32.to_le_bytes()); // dwScale = 1
    strh[24..28].copy_from_slice(&25u32.to_le_bytes()); // dwRate = 25

    let strl = list(b"strl", &[chunk(b"strh", &strh), chunk(b"strf", strf)]);
    let hdrl = list(b"hdrl", &[chunk(b"avih", &avih), strl]);

    let mut movi_body = Vec::new();
    movi_body.extend_from_slice(b"movi");
    movi_body.extend_from_slice(&chunk(b"00dc", &[0u8; 8]));
    let movi = chunk(b"LIST", &movi_body);

    let mut form_body = Vec::new();
    form_body.extend_from_slice(b"AVI ");
    form_body.extend_from_slice(&hdrl);
    form_body.extend_from_slice(&movi);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(form_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&form_body);
    out
}

/// A 40-byte BITMAPINFOHEADER with explicit scalar fields. `bit_count` of
/// 24 ⇒ compressed/truecolour; the compression FourCC is `[0;4]` (BI_RGB).
#[allow(clippy::too_many_arguments)]
fn bmih(
    width: u32,
    height: u32,
    planes: u16,
    bit_count: u16,
    size_image: u32,
    x_ppm: i32,
    y_ppm: i32,
    clr_used: u32,
    clr_important: u32,
) -> Vec<u8> {
    let mut b = vec![0u8; 40];
    b[0..4].copy_from_slice(&40u32.to_le_bytes());
    b[4..8].copy_from_slice(&width.to_le_bytes());
    b[8..12].copy_from_slice(&(height as i32).to_le_bytes());
    b[12..14].copy_from_slice(&planes.to_le_bytes());
    b[14..16].copy_from_slice(&bit_count.to_le_bytes());
    // biCompression [16..20] = BI_RGB (zero).
    b[20..24].copy_from_slice(&size_image.to_le_bytes());
    b[24..28].copy_from_slice(&x_ppm.to_le_bytes());
    b[28..32].copy_from_slice(&y_ppm.to_le_bytes());
    b[32..36].copy_from_slice(&clr_used.to_le_bytes());
    b[36..40].copy_from_slice(&clr_important.to_le_bytes());
    b
}

fn open(bytes: Vec<u8>) -> oxideav_avi::demuxer::AviDemuxer {
    let reg = CodecRegistry::new();
    let input: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    open_avi(input, &reg).expect("open_avi")
}

// --- biSizeImage ------------------------------------------------------

#[test]
fn size_image_surfaced_and_zero_is_none() {
    let dmx = open(build_avi(&bmih(64, 48, 1, 24, 9216, 0, 0, 0, 0)));
    assert_eq!(dmx.stream_size_image(0), Some(9216));

    let z = open(build_avi(&bmih(64, 48, 1, 24, 0, 0, 0, 0, 0)));
    assert_eq!(z.stream_size_image(0), None, "0 ⇒ may be zero ⇒ None");
}

#[test]
fn size_image_metadata_key() {
    let dmx = open(build_avi(&bmih(64, 48, 1, 24, 4096, 0, 0, 0, 0)));
    let md = dmx.metadata();
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:vids.0.size_image")
            .map(|(_, v)| v.as_str()),
        Some("4096")
    );

    let z = open(build_avi(&bmih(64, 48, 1, 24, 0, 0, 0, 0, 0)));
    assert!(
        !z.metadata()
            .iter()
            .any(|(k, _)| k == "avi:vids.0.size_image"),
        "no size_image key for the 0 default"
    );
}

// --- biX/YPelsPerMeter ------------------------------------------------

#[test]
fn pixels_per_meter_pair_and_zero_collapses() {
    let dmx = open(build_avi(&bmih(64, 48, 1, 24, 0, 3780, 3779, 0, 0)));
    assert_eq!(dmx.stream_pixels_per_meter(0), Some((3780, 3779)));

    // All-zero collapses to None.
    let z = open(build_avi(&bmih(64, 48, 1, 24, 0, 0, 0, 0, 0)));
    assert_eq!(z.stream_pixels_per_meter(0), None);

    // One axis non-zero still surfaces the whole pair.
    let one = open(build_avi(&bmih(64, 48, 1, 24, 0, 2835, 0, 0, 0)));
    assert_eq!(one.stream_pixels_per_meter(0), Some((2835, 0)));
}

#[test]
fn pixels_per_meter_metadata_keys() {
    let dmx = open(build_avi(&bmih(64, 48, 1, 24, 0, 1000, 2000, 0, 0)));
    let md = dmx.metadata();
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:vids.0.x_pels_per_meter")
            .map(|(_, v)| v.as_str()),
        Some("1000")
    );
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:vids.0.y_pels_per_meter")
            .map(|(_, v)| v.as_str()),
        Some("2000")
    );
}

// --- biClrUsed / biClrImportant ---------------------------------------

#[test]
fn clr_used_and_clr_important_surfaced() {
    // 8-bpp indexed: biClrUsed = 200, biClrImportant = 16.
    let dmx = open(build_avi(&bmih(8, 8, 1, 8, 0, 0, 0, 200, 16)));
    assert_eq!(dmx.stream_clr_used(0), Some(200));
    assert_eq!(dmx.stream_clr_important(0), Some(16));

    let z = open(build_avi(&bmih(8, 8, 1, 8, 0, 0, 0, 0, 0)));
    assert_eq!(z.stream_clr_used(0), None, "0 ⇒ max-for-depth ⇒ None");
    assert_eq!(z.stream_clr_important(0), None, "0 ⇒ all-important ⇒ None");
}

#[test]
fn clr_metadata_keys() {
    let dmx = open(build_avi(&bmih(8, 8, 1, 8, 0, 0, 0, 200, 16)));
    let md = dmx.metadata();
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:vids.0.clr_used")
            .map(|(_, v)| v.as_str()),
        Some("200")
    );
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "avi:vids.0.clr_important")
            .map(|(_, v)| v.as_str()),
        Some("16")
    );
}

// --- biPlanes ---------------------------------------------------------

#[test]
fn planes_conforming_is_one_no_metadata() {
    let dmx = open(build_avi(&bmih(64, 48, 1, 24, 0, 0, 0, 0, 0)));
    assert_eq!(dmx.stream_planes(0), Some(1), "conforming biPlanes == 1");
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k == "avi:vids.0.planes"),
        "no planes key for the mandated value 1"
    );
}

#[test]
fn planes_nonconforming_surfaced_with_metadata() {
    // A non-conformant writer that stamped biPlanes = 3.
    let dmx = open(build_avi(&bmih(64, 48, 3, 24, 0, 0, 0, 0, 0)));
    assert_eq!(dmx.stream_planes(0), Some(3));
    assert_eq!(
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == "avi:vids.0.planes")
            .map(|(_, v)| v.as_str()),
        Some("3"),
        "planes key fires only for non-conformant != 1"
    );

    // biPlanes = 0 is also non-conformant and still observable.
    let zero = open(build_avi(&bmih(64, 48, 0, 24, 0, 0, 0, 0, 0)));
    assert_eq!(zero.stream_planes(0), Some(0));
}

// --- non-video / out-of-range -----------------------------------------

#[test]
fn accessors_none_for_out_of_range_stream() {
    let dmx = open(build_avi(&bmih(64, 48, 1, 24, 100, 1, 1, 1, 1)));
    assert_eq!(dmx.stream_size_image(9), None);
    assert_eq!(dmx.stream_pixels_per_meter(9), None);
    assert_eq!(dmx.stream_clr_used(9), None);
    assert_eq!(dmx.stream_clr_important(9), None);
    assert_eq!(dmx.stream_planes(9), None);
}
