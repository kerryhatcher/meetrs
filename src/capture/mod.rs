//! Platform system-audio + microphone capture.
//!
//! Each platform provides the same contract (see the CONTRACT block in its
//! backend module); the rest of the crate imports only from here and never
//! names a backend directly.
//!
//! - macOS: `macos.rs`, Core Audio process tap + aggregate device.
//! - everything else: `linux.rs`, PipeWire (mic + default sink monitor).

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
#[allow(unused_imports)] // named only via io_stats()'s return type; kept as public API
pub use macos::IoStats;
#[cfg(target_os = "macos")]
pub use macos::{dropped_samples, io_stats, start, stop};

/// macOS has no post-startup stream fault reporting yet. Its device loss shows
/// up as silence in the leg's level meter instead.
#[cfg(target_os = "macos")]
pub fn take_stream_fault() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
mod linux;
#[cfg(not(target_os = "macos"))]
#[allow(unused_imports)] // named only via io_stats()'s return type; kept as public API
pub use linux::IoStats;
#[cfg(not(target_os = "macos"))]
pub use linux::{dropped_samples, io_stats, start, stop, take_stream_fault};
