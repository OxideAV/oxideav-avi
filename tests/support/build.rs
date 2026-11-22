//! Hand-assembled AVI files for pin tests: every byte is laid out here
//! from the AVI 1.0 structure tables
//! (`docs/container/riff/avi-riff-file-reference.md`), independently of
//! the crate's muxer.

#![allow(dead_code)] // each includer uses its own subset

/// One RIFF chunk: id, little-endian size, body, WORD pad.
pub fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len() + 1);
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// A `LIST` (or `RIFF`) of `form` holding `children`.
pub fn list(kind: &[u8; 4], form: &[u8; 4], children: &[u8]) -> Vec<u8> {
    let mut body = form.to_vec();
    body.extend_from_slice(children);
    chunk(kind, &body)
}

/// The 56-byte `AVISTREAMHEADER` body.
#[derive(Clone, Copy)]
pub struct Strh {
    pub fcc_type: [u8; 4],
    pub handler: [u8; 4],
    pub scale: u32,
    pub rate: u32,
    pub start: u32,
    pub length: u32,
    pub sample_size: u32,
}

impl Strh {
    pub fn video(handler: &[u8; 4], scale: u32, rate: u32, length: u32) -> Self {
        Self {
            fcc_type: *b"vids",
            handler: *handler,
            scale,
            rate,
            start: 0,
            length,
            sample_size: 0,
        }
    }

    pub fn audio(scale: u32, rate: u32, length: u32, sample_size: u32) -> Self {
        Self {
            fcc_type: *b"auds",
            handler: [0; 4],
            scale,
            rate,
            start: 0,
            length,
            sample_size,
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(56);
        b.extend_from_slice(&self.fcc_type);
        b.extend_from_slice(&self.handler);
        b.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
        b.extend_from_slice(&0u16.to_le_bytes()); // wPriority
        b.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
        b.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
        b.extend_from_slice(&self.scale.to_le_bytes());
        b.extend_from_slice(&self.rate.to_le_bytes());
        b.extend_from_slice(&self.start.to_le_bytes());
        b.extend_from_slice(&self.length.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
        b.extend_from_slice(&u32::MAX.to_le_bytes()); // dwQuality
        b.extend_from_slice(&self.sample_size.to_le_bytes());
        b.extend_from_slice(&[0u8; 8]); // rcFrame
        b
    }
}

/// A `BITMAPINFOHEADER` followed by `extra` (color table, masks, codec
/// configuration).
pub fn bmih(
    width: i32,
    height: i32,
    compression: &[u8; 4],
    bit_count: u16,
    extra: &[u8],
) -> Vec<u8> {
    let mut b = Vec::with_capacity(40 + extra.len());
    b.extend_from_slice(&(40u32 + extra.len() as u32).to_le_bytes());
    b.extend_from_slice(&width.to_le_bytes());
    b.extend_from_slice(&height.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&bit_count.to_le_bytes());
    b.extend_from_slice(compression);
    b.extend_from_slice(&[0u8; 20]);
    b.extend_from_slice(extra);
    b
}

/// A `WAVEFORMATEX` with `extra` as its `cbSize` bytes.
pub fn wfx(
    tag: u16,
    channels: u16,
    rate: u32,
    avg_bytes: u32,
    block_align: u16,
    bits: u16,
    extra: &[u8],
) -> Vec<u8> {
    let mut b = Vec::with_capacity(18 + extra.len());
    b.extend_from_slice(&tag.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&avg_bytes.to_le_bytes());
    b.extend_from_slice(&block_align.to_le_bytes());
    b.extend_from_slice(&bits.to_le_bytes());
    b.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    b.extend_from_slice(extra);
    b
}

/// One stream of a hand-built file.
pub struct Track {
    pub strh: Vec<u8>,
    pub strf: Vec<u8>,
    /// Further `strl` children, already chunk-framed (`strn`, `vprp`…).
    pub extra: Vec<u8>,
}

/// One `movi` entry: stream number, two-character type, payload,
/// `idx1` flags (`None` = no index entry).
pub struct Item<'a> {
    pub stream: u8,
    pub kind: &'a [u8; 2],
    pub data: &'a [u8],
    pub flags: Option<u32>,
}

/// Assemble `RIFF AVI ( hdrl movi [idx1] )`. `idx1` is written when
/// `with_index`, with `movi`-relative offsets.
pub fn avi(tracks: &[Track], items: &[Item<'_>], with_index: bool) -> Vec<u8> {
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40_000u32.to_le_bytes()); // dwMicroSecPerFrame
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&(if with_index { 0x10u32 } else { 0 }).to_le_bytes());
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwTotalFrames
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih.extend_from_slice(&(tracks.len() as u32).to_le_bytes());
    avih.extend_from_slice(&[0u8; 28]);
    let mut hdrl = chunk(b"avih", &avih);
    for t in tracks {
        let mut strl = chunk(b"strh", &t.strh);
        strl.extend_from_slice(&chunk(b"strf", &t.strf));
        strl.extend_from_slice(&t.extra);
        hdrl.extend_from_slice(&list(b"LIST", b"strl", &strl));
    }
    let mut movi = Vec::new();
    let mut idx1 = Vec::new();
    for it in items {
        let id = [
            b'0' + it.stream / 10,
            b'0' + it.stream % 10,
            it.kind[0],
            it.kind[1],
        ];
        if let Some(flags) = it.flags {
            idx1.extend_from_slice(&id);
            idx1.extend_from_slice(&flags.to_le_bytes());
            idx1.extend_from_slice(&(4 + movi.len() as u32).to_le_bytes());
            idx1.extend_from_slice(&(it.data.len() as u32).to_le_bytes());
        }
        movi.extend_from_slice(&chunk(&id, it.data));
    }
    let mut body = list(b"LIST", b"hdrl", &hdrl);
    body.extend_from_slice(&list(b"LIST", b"movi", &movi));
    if with_index {
        body.extend_from_slice(&chunk(b"idx1", &idx1));
    }
    list(b"RIFF", b"AVI ", &body)
}
