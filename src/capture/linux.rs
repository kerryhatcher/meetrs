//! Linux system-audio + microphone capture. STUB.
//!
//! Exposes the same public API as `macos.rs` so the rest of the crate builds
//! unchanged on Linux. The PipeWire backend replaces `start` and friends; until
//! then `start` refuses, so nothing can be recorded on Linux yet.
//!
//! CONTRACT (do not change):
//!   pub fn start(ring_samples: usize) -> Result<(CaptureInfo, rtrb::Consumer<f32>)>
//!   pub fn stop()

use anyhow::{Result, anyhow};

use crate::types::CaptureInfo;

/// Mirrors the macOS `IoStats`. The stub never runs a callback, so every
/// counter stays zero.
#[derive(Debug, Clone, Copy)]
pub struct IoStats {
    pub calls: u64,
    pub shape_mismatch: u64,
    pub empty: u64,
    pub last_n_buffers: u64,
    pub expected_n_buffers: u64,
}

pub fn dropped_samples() -> u64 {
    0
}

pub fn io_stats() -> IoStats {
    IoStats {
        calls: 0,
        shape_mismatch: 0,
        empty: 0,
        last_n_buffers: 0,
        expected_n_buffers: 0,
    }
}

pub fn start(_ring_samples: usize) -> Result<(CaptureInfo, rtrb::Consumer<f32>)> {
    Err(anyhow!("Linux capture backend not implemented yet"))
}

pub fn stop() {}
