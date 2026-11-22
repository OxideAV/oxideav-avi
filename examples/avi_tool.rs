//! Command-line inspector / remuxer for AVI files.
//!
//! ```text
//! avi_tool dump  <in.avi>                 # streams + one line per packet
//! avi_tool remux <in.avi> <out.avi> [avi10|opendml|opendml:<bytes>|strl]
//! avi_tool seek  <in.avi> <stream> <pts>  # seek, then print the next packet
//! avi_tool raw   <in.avi> <stream> <out>  # one stream's payloads, concatenated
//! ```
//!
//! `dump` prints one `S` line per stream and one `P` line per packet:
//!
//! ```text
//! S <index> <media> <codec id> tag=<tag> tb=<num>/<den> start=<n> dur=<n> ... extradata=<len>:<crc32>
//! P <index> <pts> <dts> <duration> <size> <K|_> <crc32>
//! ```
//!
//! The CRC is the IEEE 802.3 CRC-32 of the packet payload, the form a
//! black-box prober prints for the same packet, so the two listings can
//! be compared line by line.

#[path = "../tests/support/claims.rs"]
mod claims;

use std::io::Write;

use oxideav_core::{Demuxer, Error, Muxer, ReadSeek, WriteSeek};

use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("dump") if args.len() == 3 => dump(&args[2]),
        Some("remux") if args.len() >= 4 => remux(&args[2], &args[3], args.get(4)),
        Some("seek") if args.len() == 5 => seek(&args[2], &args[3], &args[4]),
        Some("raw") if args.len() == 5 => raw(&args[2], &args[3], &args[4]),
        _ => {
            eprintln!(
                "usage: avi_tool dump <in> | remux <in> <out> [avi10|opendml|opendml:<bytes>|strl] | seek <in> <stream> <pts> | raw <in> <stream> <out>"
            );
            2
        }
    };
    std::process::exit(code);
}

fn open(path: &str) -> Result<oxideav_avi::demuxer::AviDemuxer, Error> {
    let f = std::fs::File::open(path)?;
    let rs: Box<dyn ReadSeek> = Box::new(f);
    oxideav_avi::demuxer::open_avi(rs, &claims::registry())
}

fn dump(path: &str) -> i32 {
    let mut dmx = match open(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open failed: {e}");
            return 1;
        }
    };
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for s in dmx.streams() {
        let _ = writeln!(out, "{}", claims::stream_line(s));
    }
    for (k, v) in dmx.metadata() {
        let _ = writeln!(out, "M {k}={v}");
    }
    if let Some(d) = dmx.duration_micros() {
        let _ = writeln!(out, "D {d}");
    }
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                let _ = writeln!(out, "{}", claims::packet_line(&p));
            }
            Err(Error::Eof) => break,
            Err(e) => {
                let _ = writeln!(out, "E {e}");
                let _ = out.flush();
                eprintln!("demux error: {e}");
                return 1;
            }
        }
    }
    0
}

fn remux(input: &str, output: &str, mode: Option<&String>) -> i32 {
    let mut dmx = match open(input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open failed: {e}");
            return 1;
        }
    };
    let mut opts = AviMuxOptions::default();
    let kind = match mode.map(String::as_str) {
        None | Some("avi10") => AviKind::Avi10,
        Some("opendml") => AviKind::OpenDml(RiffSegmentLimit::OneGiB),
        Some("strl") => {
            opts = opts.with_strl_std_index(1 << 16);
            AviKind::OpenDml(RiffSegmentLimit::OneGiB)
        }
        Some(m) => match m.strip_prefix("opendml:").and_then(|n| n.parse().ok()) {
            Some(n) => AviKind::OpenDml(RiffSegmentLimit::Bytes(n)),
            None => {
                eprintln!("unknown mode {m}");
                return 2;
            }
        },
    };
    let streams = dmx.streams().to_vec();
    let f = match std::fs::File::create(output) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("create failed: {e}");
            return 1;
        }
    };
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = match oxideav_avi::muxer::open_avi(ws, &streams, kind, opts) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mux open failed: {e}");
            return 1;
        }
    };
    let meta = dmx.metadata().to_vec();
    let _ = mux.set_metadata(&meta);
    if let Err(e) = mux.write_header() {
        eprintln!("write_header failed: {e}");
        return 1;
    }
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                if let Err(e) = mux.write_packet(&p) {
                    eprintln!("write_packet failed: {e}");
                    return 1;
                }
            }
            Err(Error::Eof) => break,
            Err(e) => {
                eprintln!("demux error: {e}");
                return 1;
            }
        }
    }
    if let Err(e) = mux.write_trailer() {
        eprintln!("write_trailer failed: {e}");
        return 1;
    }
    0
}

fn seek(path: &str, stream: &str, pts: &str) -> i32 {
    let (Ok(stream), Ok(pts)) = (stream.parse::<u32>(), pts.parse::<i64>()) else {
        eprintln!("stream and pts must be integers");
        return 2;
    };
    let mut dmx = match open(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open failed: {e}");
            return 1;
        }
    };
    match dmx.seek_to(stream, pts) {
        Ok(landed) => println!("landed {landed}"),
        Err(e) => {
            eprintln!("seek failed: {e}");
            return 1;
        }
    }
    loop {
        match dmx.next_packet() {
            Ok(p) if p.stream_index == stream => {
                println!("{}", claims::packet_line(&p));
                return 0;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("no packet after seek: {e}");
                return 1;
            }
        }
    }
}

fn raw(path: &str, stream: &str, output: &str) -> i32 {
    let Ok(stream) = stream.parse::<u32>() else {
        eprintln!("stream must be an integer");
        return 2;
    };
    let mut dmx = match open(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open failed: {e}");
            return 1;
        }
    };
    let mut bytes = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) if p.stream_index == stream => bytes.extend_from_slice(&p.data),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => {
                eprintln!("demux error: {e}");
                return 1;
            }
        }
    }
    match std::fs::write(output, bytes) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("write failed: {e}");
            1
        }
    }
}
