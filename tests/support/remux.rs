//! Demux one AVI and mux it back through the public API — the remux a
//! caller would write. Shared by the `avi_tool` example and the
//! black-box matrix. Expects a sibling `claims` module.

#![allow(dead_code)] // each includer uses its own subset

use oxideav_core::{Demuxer, Error, Muxer, ReadSeek, Result, WriteSeek};

use oxideav_avi::muxer::{AviKind, AviMuxOptions};

/// Copy every stream, the text metadata, every packet and every
/// palette change of `input` into a new file of the given kind.
pub fn remux(
    input: Box<dyn ReadSeek>,
    output: Box<dyn WriteSeek>,
    kind: AviKind,
    options: AviMuxOptions,
) -> Result<()> {
    remux_with(input, output, kind, options, true)
}

/// [`remux`], optionally without the demuxer's track records
/// (`avi.*` options): the muxer then builds every stream header from
/// the parameters alone, as it does for a stream an encoder describes.
pub fn remux_with(
    input: Box<dyn ReadSeek>,
    output: Box<dyn WriteSeek>,
    kind: AviKind,
    options: AviMuxOptions,
    keep_records: bool,
) -> Result<()> {
    let mut dmx = oxideav_avi::demuxer::open_avi(input, &super::claims::registry())?;
    let mut streams = dmx.streams().to_vec();
    if !keep_records {
        for s in &mut streams {
            let kept: Vec<(String, String)> = s
                .params
                .options
                .iter()
                .filter(|(k, _)| !k.starts_with("avi."))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            s.params.options = oxideav_core::CodecOptions::new();
            for (k, v) in kept {
                s.params.options.insert(k, v);
            }
        }
    }
    let mut mux = oxideav_avi::muxer::open_avi(output, &streams, kind, options)?;
    mux.set_metadata(dmx.metadata())?;
    mux.write_header()?;
    // Palette changes (`##pc`) are not packets: the demuxer keeps them
    // on the side with the number of frames that precede each one, and
    // they are written back at the same place.
    let mut palette: Vec<Vec<(u64, Vec<u8>)>> = (0..streams.len() as u32)
        .map(|s| {
            let at = dmx.palette_change_packet_positions(s);
            let data = dmx.palette_change_data(s);
            at.iter().copied().zip(data.iter().cloned()).rev().collect()
        })
        .collect();
    let mut written = vec![0u64; streams.len()];
    loop {
        let packet = match dmx.next_packet() {
            Ok(p) => p,
            Err(Error::Eof) => break,
            Err(e) => return Err(e),
        };
        let s = packet.stream_index as usize;
        while palette[s].last().is_some_and(|(at, _)| *at <= written[s]) {
            if let Some((_, body)) = palette[s].pop() {
                mux.write_palette_change(packet.stream_index, &body)?;
            }
        }
        written[s] += 1;
        mux.write_packet(&packet)?;
    }
    mux.write_trailer()
}
