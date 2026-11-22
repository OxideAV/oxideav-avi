//! Command-line inspector / remuxer for AVI files.
//!
//! ```text
//! avi_tool dump  <in.avi>                 # streams + one line per packet
//! avi_tool remux <in.avi> <out.avi> [avi10|opendml|opendml:<bytes>|strl]
//! avi_tool seek  <in.avi> <stream> <pts>  # seek, then print the next packet
//! avi_tool raw   <in.avi> <stream> <out>  # one stream's payloads, concatenated
//! avi_tool time  <in.avi>                 # open / full walk / 1000 seeks, timed
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
#[path = "../tests/support/remux.rs"]
mod remux;

use std::io::Write;

use oxideav_core::{Demuxer, Error, ReadSeek, WriteSeek};

use oxideav_avi::muxer::{AviKind, AviMuxOptions, RiffSegmentLimit};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("dump") if args.len() == 3 => dump(&args[2]),
        Some("remux") if args.len() >= 4 => remux(&args[2], &args[3], args.get(4)),
        Some("seek") if args.len() == 5 => seek(&args[2], &args[3], &args[4]),
        Some("raw") if args.len() == 5 => raw(&args[2], &args[3], &args[4]),
        Some("time") if args.len() == 3 => time(&args[2]),
        _ => {
            eprintln!(
                "usage: avi_tool dump <in> | remux <in> <out> [avi10|opendml|opendml:<bytes>|strl] | seek <in> <stream> <pts> | raw <in> <stream> <out> | time <in>"
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
    let mut opts = AviMuxOptions::default();
    // `<mode>+bare`: drop the demuxer's track records, so the headers
    // are built from the stream parameters alone.
    let bare = mode.is_some_and(|m| m.ends_with("+bare"));
    let mode = mode.map(|m| m.trim_end_matches("+bare").to_string());
    let kind = match mode.as_deref() {
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
    let run = || -> Result<(), Error> {
        let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(input)?);
        let ws: Box<dyn WriteSeek> = Box::new(std::fs::File::create(output)?);
        remux::remux_with(rs, ws, kind, opts, !bare)
    };
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("remux failed: {e}");
            1
        }
    }
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

fn time(path: &str) -> i32 {
    let t0 = std::time::Instant::now();
    let mut dmx = match open(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open failed: {e}");
            return 1;
        }
    };
    let opened = t0.elapsed();
    let t1 = std::time::Instant::now();
    let (mut packets, mut bytes) = (0u64, 0u64);
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                packets += 1;
                bytes += p.data.len() as u64;
            }
            Err(Error::Eof) => break,
            Err(e) => {
                eprintln!("demux error: {e}");
                return 1;
            }
        }
    }
    let walked = t1.elapsed();
    let span = dmx.streams()[0].duration.unwrap_or(0).max(1);
    let t2 = std::time::Instant::now();
    let mut landed = 0u32;
    for n in 0..1000i64 {
        // A fixed scatter of targets over the first stream.
        let target = (n * 7919) % span;
        if dmx.seek_to(0, target).is_ok() && dmx.next_packet().is_ok() {
            landed += 1;
        }
    }
    let sought = t2.elapsed();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "open {:.3} ms; walk {packets} packets, {mib:.1} MiB in {:.1} ms ({:.0} MiB/s); 1000 seeks + read ({landed} landed) {:.2} ms",
        opened.as_secs_f64() * 1e3,
        walked.as_secs_f64() * 1e3,
        mib / walked.as_secs_f64().max(1e-9),
        sought.as_secs_f64() * 1e3,
    );
    0
}
