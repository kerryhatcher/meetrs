//! Shared contracts between the capture, chunk, and ui modules.
//!
//! This file is the interface agreement. Changing anything here means changing
//! all three consumers, so don't.

use std::path::PathBuf;
use std::time::Duration;

/// Every stage runs at this rate. The aggregate device is configured to it and
/// the WAV headers declare it. If the hardware refuses, capture fails loudly
/// rather than silently resampling.
pub const SAMPLE_RATE: u32 = 48_000;

/// Close the current chunk after this much continuous below-threshold audio.
/// A natural conversational pause, not an inter-word gap.
pub const SILENCE_TO_CUT: Duration = Duration::from_secs(2);

/// Force a cut regardless of silence, so a monologue still bounds crash loss.
pub const MAX_CHUNK: Duration = Duration::from_secs(300);

/// Below this RMS (linear, not dB) a block counts as silence.
///
/// This has to sit above your room's noise floor and below speech, and that gap
/// is hardware-specific: a measured ambient floor of 0.0105 RMS on the author's
/// MacBook mic defeated the original 0.004 default entirely — every chunk stayed
/// open forever because the room alone read as sound. Run `meetrs --check` to
/// measure yours; it prints per-leg RMS with no audio playing.
pub const DEFAULT_SILENCE_RMS: f32 = 0.02;

/// Env var to override the silence threshold without a rebuild.
///
/// ponytail: env var rather than a CLI flag because the threshold has to reach
/// the writer thread, and a flag would mean threading it through `chunk::run`'s
/// signature. Promote it to a real flag if anything else ever needs configuring.
pub const SILENCE_RMS_ENV: &str = "MEETRS_SILENCE_RMS";

/// Silence threshold, honoring `MEETRS_SILENCE_RMS` if it parses as a positive
/// float. A malformed value is ignored rather than fatal — a typo in an env var
/// should not stop a recording that is about to start.
pub fn silence_threshold() -> f32 {
    std::env::var(SILENCE_RMS_ENV)
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v > 0.0 && v.is_finite())
        .unwrap_or(DEFAULT_SILENCE_RMS)
}

/// A chunk shorter than this is discarded rather than written — a stray cough
/// between two long pauses isn't worth a file.
pub const MIN_CHUNK: Duration = Duration::from_millis(750);

/// What the capture backend negotiated with the hardware. Returned once at
/// startup; the sample stream's shape does not change afterward.
#[derive(Debug, Clone, Copy)]
pub struct CaptureInfo {
    /// Total interleaved channels arriving from the aggregate device.
    pub channels: u16,
    pub sample_rate: u32,
    /// Which interleaved channel indices carry system/output audio.
    pub system_channels: (u16, u16),
    /// Which interleaved channel indices carry the microphone.
    pub mic_channels: (u16, u16),
}

/// Progress reported from the recording thread to the UI. Send-only, lossy by
/// design: the UI dropping a Level update is fine, so the channel never blocks
/// the writer.
///
// Several fields are written by the writer thread and not (yet) read by the UI —
// they exist so the recording side reports its full state rather than only what
// today's screen happens to draw, and they all show up in `{:?}` when debugging.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Status {
    /// Emitted ~20x/sec for the meters.
    Level {
        system_rms: f32,
        system_peak: f32,
        mic_rms: f32,
        mic_peak: f32,
    },
    /// A new chunk file was opened.
    ChunkOpened { path: PathBuf, index: u32 },
    /// A chunk was closed and fsynced. This is the durability signal.
    ChunkClosed {
        path: PathBuf,
        index: u32,
        duration: Duration,
        bytes: u64,
    },
    /// A chunk was closed and then discarded for being under MIN_CHUNK.
    ChunkDiscarded { index: u32, duration: Duration },
    /// Silence-detector state changed. Drives the "listening / paused" indicator.
    Silence { in_silence: bool },
    /// The ring buffer overflowed — the writer could not keep up and samples
    /// were lost. Surfaced prominently because it means corrupt output.
    Overrun { dropped_samples: u64 },
    /// Non-fatal problem worth showing.
    Warning(String),
    /// Recording has stopped and all files are flushed.
    Finished { total_chunks: u32, total: Duration },

    /// Transcription of a chunk began.
    TranscribeStarted { index: u32 },
    /// Transcription finished. `took` vs the chunk's audio duration is the
    /// realtime factor, which is what tells you whether transcription can keep
    /// up with recording.
    TranscribeDone {
        index: u32,
        took: Duration,
        audio: Duration,
        words: usize,
    },
    /// Polished transcript lines for one chunk, already formatted for display
    /// (`[hh:mm:ss] leg: text`). Sent alongside TranscribeDone so the UI can
    /// stream the words themselves, not just the throughput stats.
    Transcript { index: u32, lines: Vec<String> },
    /// Transcription failed for one chunk. Never fatal — the audio is already
    /// safely on disk, which is the guarantee that matters.
    TranscribeFailed { index: u32, err: String },
    /// Chunks closed but not yet transcribed. Non-zero and growing means
    /// transcription is falling behind recording.
    TranscribeBacklog { pending: usize },
}

/// A closed chunk handed to the transcription worker. Sent as soon as the chunk
/// is fsynced, so transcription of chunk 0 overlaps recording of chunk 1.
#[derive(Debug, Clone)]
pub struct Job {
    pub path: PathBuf,
    pub index: u32,
    /// Offset of this chunk from session start, so transcript timestamps can be
    /// made session-relative rather than chunk-relative.
    pub offset: Duration,
}

/// Where the Whisper model is cached: `~/.meetrs/models/`.
pub fn models_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate ~/.meetrs"))?;
    Ok(PathBuf::from(home).join(".meetrs").join("models"))
}

/// Where every session lives: `~/.meetrs/recordings/`.
pub fn recordings_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate ~/.meetrs"))?;
    Ok(PathBuf::from(home).join(".meetrs").join("recordings"))
}

/// Where a session's files live: `~/.meetrs/recordings/<rfc3339-ish timestamp>/`.
pub fn session_dir(started: chrono::DateTime<chrono::Local>) -> anyhow::Result<PathBuf> {
    Ok(recordings_dir()?.join(started.format("%Y-%m-%dT%H-%M-%S").to_string()))
}
