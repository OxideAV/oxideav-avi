//! Pure-Rust AVI (RIFF/AVI) container: demuxer + muxer.
//!
//! AVI is Microsoft's legacy RIFF-based container, still ubiquitous for
//! Motion-JPEG output from security cameras and older capture hardware. This
//! crate parses and emits AVI 1.0 files. OpenDML extensions (`ix##`,
//! super-index, files > 2 GiB) are explicitly out of scope — see
//! `muxer::write_packet` which returns an error if the output approaches
//! 2 GiB.

pub mod codec_map;
pub mod demuxer;
pub mod muxer;
pub mod riff;
pub mod stream_format;

use oxideav_core::ContainerRegistry;

pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("avi", demuxer::open);
    reg.register_muxer("avi", muxer::open);
    reg.register_extension("avi", "avi");
    reg.register_probe("avi", probe);
}

/// Install the AVI container into a [`oxideav_core::RuntimeContext`].
///
/// Convenience wrapper around [`register_containers`] that matches the
/// uniform `register(&mut RuntimeContext)` entry point every sibling
/// crate exposes.
///
/// Also auto-registered into [`oxideav_core::REGISTRARS`] via the
/// [`oxideav_core::register!`] macro below so consumers calling
/// [`oxideav_core::RuntimeContext::with_all_features`] pick AVI up
/// without any explicit umbrella plumbing.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("avi", register);

/// `RIFF....AVI ` — RIFF chunk with form type AVI (note the trailing space).
fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 12 && &p.buf[0..4] == b"RIFF" && &p.buf[8..12] == b"AVI " {
        100
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_via_runtime_context_installs_container() {
        let mut ctx = oxideav_core::RuntimeContext::new();
        register(&mut ctx);
        assert_eq!(ctx.containers.container_for_extension("avi"), Some("avi"));
    }
}
