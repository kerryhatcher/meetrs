//! Worker thread: transcribes closed chunks with whisper.cpp (via whisper-rs) as
//! soon as they land, so transcription of chunk N overlaps recording of chunk N+1.
//!
//! CONTRACT (do not change):
//!   pub fn run(rx: mpsc::Receiver<Job>, model: PathBuf, dir: PathBuf, tx: mpsc::Sender<Status>) -> Result<()>

use crate::types::{Job, Status};
use anyhow::{Context, Result};
use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const WHISPER_SAMPLE_RATE: u32 = 16_000;

/// Just the bits of `chunk.rs`'s `meta.json` this module cares about. Extra
/// fields (chunks, detector, ...) are ignored by serde rather than mirrored.
#[derive(Deserialize)]
struct SessionMeta {
    channels: u16,
    system_channels: (u16, u16),
    mic_channels: (u16, u16),
}

fn load_session_meta(dir: &Path) -> Result<SessionMeta> {
    let raw = std::fs::read_to_string(dir.join("meta.json")).context("reading meta.json")?;
    serde_json::from_str(&raw).context("parsing meta.json")
}

/// Downmix one leg (a stereo channel pair) of an interleaved buffer to mono by
/// averaging the pair. Pure function so it's testable without any audio I/O.
fn downmix_leg(interleaved: &[f32], channels: usize, leg: (u16, u16)) -> Result<Vec<f32>> {
    let (c0, c1) = (leg.0 as usize, leg.1 as usize);
    anyhow::ensure!(
        c0 < channels && c1 < channels,
        "leg channels ({c0}, {c1}) out of range for {channels}-channel audio"
    );
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * channels;
        out.push((interleaved[base + c0] + interleaved[base + c1]) * 0.5);
    }
    Ok(out)
}

/// Resample mono f32 audio from `from_rate` to 16kHz with a proper sinc filter
/// (aliasing here costs ASR accuracy, unlike the VAD's box-filter shortcut).
fn resample_to_whisper_rate(mono: &[f32], from_rate: u32) -> Result<Vec<f32>> {
    if mono.is_empty() {
        return Ok(Vec::new());
    }
    let ratio = WHISPER_SAMPLE_RATE as f64 / from_rate as f64;
    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        1.0, // fixed ratio: never adjusted at runtime
        &SincInterpolationParameters::default(),
        1024,
        1, // mono
        FixedAsync::Input,
    )
    .context("constructing resampler")?;
    let input = InterleavedOwned::new_from(mono.to_vec(), 1, mono.len())
        .map_err(|e| anyhow::anyhow!("building resampler input buffer: {e}"))?;
    let output = resampler
        .process_all(&input, mono.len(), None)
        .context("resampling")?;
    Ok(output.take_data())
}

/// One transcribed segment, session-relative in time.
#[derive(Serialize, Clone)]
struct SegmentOut {
    leg: &'static str,
    start_secs: f64,
    end_secs: f64,
    text: String,
    no_speech_prob: f32,
}

#[derive(Serialize)]
struct ChunkTranscript {
    index: u32,
    offset_secs: f64,
    audio_secs: f64,
    segments: Vec<SegmentOut>,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Session-relative start time for a chunk-local timestamp. Whisper's
/// centisecond timestamps are chunk-relative; `offset` is where the chunk
/// itself sits in the session.
fn session_relative_secs(offset: Duration, chunk_local_secs: f64) -> f64 {
    offset.as_secs_f64() + chunk_local_secs
}

fn whisper_threads() -> i32 {
    // Leave at least one core for the audio callback + writer thread — an
    // overrun there is far worse than transcription running a bit slower.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cores.saturating_sub(1).max(1) as i32
}

fn full_params() -> FullParams<'static, 'static> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 5 });
    params.set_n_threads(whisper_threads());
    params.set_language(Some("en"));
    params.set_translate(false);
    // These would corrupt the TUI by writing straight to stdout/stderr.
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params
}

/// Transcribe one mono 16kHz leg, returning session-relative segments.
fn transcribe_leg(
    ctx: &WhisperContext,
    mono_16k: &[f32],
    leg: &'static str,
    offset: Duration,
) -> Result<Vec<SegmentOut>> {
    if mono_16k.is_empty() {
        return Ok(Vec::new());
    }
    let mut state = ctx.create_state().context("creating whisper state")?;
    state
        .full(full_params(), mono_16k)
        .context("running whisper full()")?;

    let n = state.full_n_segments();
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Some(seg) = state.get_segment(i) else {
            continue;
        };
        let text = seg.to_str_lossy().unwrap_or_default().trim().to_string();
        // Whisper emits bracketed non-speech markers for silence. One leg is
        // quiet in most chunks (nobody talks over the whole meeting), so without
        // this the transcript is mostly "[BLANK_AUDIO]".
        if is_non_speech_marker(&text) {
            continue;
        }
        // Timestamps are centiseconds (10ms units).
        let start = session_relative_secs(offset, seg.start_timestamp() as f64 * 0.01);
        let end = session_relative_secs(offset, seg.end_timestamp() as f64 * 0.01);
        out.push(SegmentOut {
            leg,
            start_secs: start,
            end_secs: end,
            text,
            no_speech_prob: seg.no_speech_probability(),
        });
    }
    Ok(out)
}

/// True for empty text and for whisper's bracketed/parenthesised non-speech
/// annotations: `[BLANK_AUDIO]`, `(silence)`, `[ Silence ]`, `[MUSIC]`, and so on.
fn is_non_speech_marker(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    let inner = t
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .or_else(|| t.strip_prefix('(').and_then(|s| s.strip_suffix(')')));
    match inner {
        // Only treat a bracketed span as a marker if it's the entire segment and
        // has no sentence punctuation — real speech can contain parentheticals.
        Some(i) => !i.contains(['.', '?', '!']),
        None => false,
    }
}

/// Everything needed to append a chunk's transcript to the human-readable log.
struct Processed {
    segments: Vec<SegmentOut>,
    audio_secs: f64,
    words: usize,
}

fn process_job(ctx: &WhisperContext, job: &Job, meta: &SessionMeta) -> Result<Processed> {
    let mut reader = hound::WavReader::open(&job.path).context("opening chunk wav")?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let interleaved: Vec<f32> = reader
        .samples::<f32>()
        .collect::<std::result::Result<_, _>>()
        .context("reading wav samples")?;
    let total_frames = interleaved.len() / channels.max(1);
    let audio_secs = total_frames as f64 / spec.sample_rate as f64;

    let mic_mono = downmix_leg(&interleaved, meta.channels as usize, meta.mic_channels)?;
    let sys_mono = downmix_leg(&interleaved, meta.channels as usize, meta.system_channels)?;
    let mic_16k = resample_to_whisper_rate(&mic_mono, spec.sample_rate)?;
    let sys_16k = resample_to_whisper_rate(&sys_mono, spec.sample_rate)?;

    let mut segments = transcribe_leg(ctx, &mic_16k, "mic", job.offset)?;
    segments.extend(transcribe_leg(ctx, &sys_16k, "system", job.offset)?);
    segments.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));

    let words = segments
        .iter()
        .map(|s| s.text.split_whitespace().count())
        .sum();
    Ok(Processed {
        segments,
        audio_secs,
        words,
    })
}

fn fmt_timestamp(secs: f64) -> String {
    let total_ms = (secs * 1000.0).round() as i64;
    let h = total_ms / 3_600_000;
    let m = (total_ms / 60_000) % 60;
    let s = (total_ms / 1000) % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

pub fn run(rx: Receiver<Job>, model: PathBuf, dir: PathBuf, tx: Sender<Status>) -> Result<()> {
    // Own connection for this thread (WAL makes that safe). Best-effort: the
    // chunk-NNN.json files are authoritative, so a DB failure costs the index,
    // never a transcript. `meetrs --reindex` can repopulate.
    let mut db = match crate::db::open() {
        Ok(d) => Some(d),
        Err(e) => {
            let _ = tx.send(Status::Warning(format!(
                "db unavailable, not indexing: {e:#}"
            )));
            None
        }
    };

    let ctx = WhisperContext::new_with_params(&model, WhisperContextParameters::default())
        .context("loading whisper model")?;

    let mut meta: Option<SessionMeta> = None;
    let mut transcript_md = String::new();
    let mut queue: VecDeque<Job> = VecDeque::new();

    loop {
        while let Ok(job) = rx.try_recv() {
            queue.push_back(job);
        }
        let job = match queue.pop_front() {
            Some(job) => job,
            None => match rx.recv() {
                Ok(job) => job,
                Err(_) => break, // sender gone, queue drained: done
            },
        };

        let _ = tx.send(Status::TranscribeStarted { index: job.index });
        let started = Instant::now();

        let session_meta = match &meta {
            Some(m) => m,
            None => match load_session_meta(&dir) {
                Ok(m) => {
                    meta = Some(m);
                    meta.as_ref().unwrap()
                }
                Err(e) => {
                    let _ = tx.send(Status::TranscribeFailed {
                        index: job.index,
                        err: format!("meta.json not ready: {e}"),
                    });
                    let _ = tx.send(Status::TranscribeBacklog {
                        pending: queue.len(),
                    });
                    continue;
                }
            },
        };

        match process_job(&ctx, &job, session_meta) {
            Ok(processed) => {
                let took = started.elapsed();
                let chunk_json = ChunkTranscript {
                    index: job.index,
                    offset_secs: job.offset.as_secs_f64(),
                    audio_secs: processed.audio_secs,
                    segments: processed.segments.clone(),
                };
                let json_path = job
                    .path
                    .with_file_name(format!("chunk-{:03}.json", job.index));
                if let Err(e) = serde_json::to_vec_pretty(&chunk_json)
                    .context("serializing chunk json")
                    .and_then(|bytes| write_atomic(&json_path, &bytes))
                {
                    let _ = tx.send(Status::Warning(format!(
                        "chunk {} transcribed but JSON write failed: {e}",
                        job.index
                    )));
                }

                for seg in &processed.segments {
                    transcript_md.push_str(&format!(
                        "**[{}] {}:** {}\n\n",
                        fmt_timestamp(seg.start_secs),
                        seg.leg,
                        seg.text
                    ));
                }
                if let Err(e) = write_atomic(&dir.join("transcript.md"), transcript_md.as_bytes()) {
                    let _ = tx.send(Status::Warning(format!(
                        "transcript.md write failed after chunk {}: {e}",
                        job.index
                    )));
                }

                if let Some(db) = db.as_mut() {
                    let rows: Vec<crate::db::SegmentIn> = processed
                        .segments
                        .iter()
                        .map(|s| crate::db::SegmentIn {
                            leg: s.leg.to_string(),
                            start_secs: s.start_secs,
                            end_secs: s.end_secs,
                            text: s.text.clone(),
                            no_speech_prob: s.no_speech_prob,
                        })
                        .collect();
                    if let Err(e) = db.record_segments(&dir, job.index, &rows) {
                        let _ = tx.send(Status::Warning(format!("db: {e:#}")));
                    }
                }

                let _ = tx.send(Status::TranscribeDone {
                    index: job.index,
                    took,
                    audio: Duration::from_secs_f64(processed.audio_secs),
                    words: processed.words,
                });
            }
            Err(e) => {
                if let Some(db) = db.as_mut()
                    && let Err(e2) = db.mark_chunk_failed(&dir, job.index, &e.to_string())
                {
                    let _ = tx.send(Status::Warning(format!("db: {e2:#}")));
                }
                let _ = tx.send(Status::TranscribeFailed {
                    index: job.index,
                    err: e.to_string(),
                });
            }
        }

        let _ = tx.send(Status::TranscribeBacklog {
            pending: queue.len(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_leg_averages_the_right_pair() {
        // 3 channels, 2 frames: [c0,c1,c2, c0,c1,c2]
        let buf = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let leg = downmix_leg(&buf, 3, (0, 1)).unwrap();
        assert_eq!(leg, vec![1.5, 4.5]);
        let leg2 = downmix_leg(&buf, 3, (2, 2)).unwrap();
        assert_eq!(leg2, vec![3.0, 6.0]);
    }

    #[test]
    fn downmix_leg_rejects_out_of_range_channels() {
        let buf = vec![1.0, 2.0];
        assert!(downmix_leg(&buf, 2, (0, 5)).is_err());
    }

    #[test]
    fn resample_48k_to_16k_shrinks_by_three() {
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = resample_to_whisper_rate(&input, 48_000).unwrap();
        let expected = input.len() / 3;
        let diff = (out.len() as i64 - expected as i64).unsigned_abs();
        assert!(diff < 8, "expected ~{expected} frames, got {}", out.len());
    }

    #[test]
    fn resample_empty_input_is_empty_output() {
        assert!(resample_to_whisper_rate(&[], 48_000).unwrap().is_empty());
    }

    #[test]
    fn session_relative_secs_adds_offset() {
        let offset = Duration::from_secs_f64(12.5);
        assert!((session_relative_secs(offset, 1.25) - 13.75).abs() < 1e-9);
    }

    #[test]
    fn session_meta_parses_chunk_metadata() {
        let raw = r#"{
            "started": "2026-01-01T00:00:00Z",
            "sample_rate": 48000,
            "channels": 4,
            "system_channels": [0, 1],
            "mic_channels": [2, 3],
            "silence_threshold_rms": 0.02,
            "detector": "earshot",
            "chunks": []
        }"#;
        let meta: SessionMeta = serde_json::from_str(raw).unwrap();
        assert_eq!(meta.channels, 4);
        assert_eq!(meta.system_channels, (0, 1));
        assert_eq!(meta.mic_channels, (2, 3));
    }

    #[test]
    fn non_speech_markers_are_filtered() {
        for m in [
            "[BLANK_AUDIO]",
            "(silence)",
            "[ Silence ]",
            "[MUSIC]",
            "",
            "   ",
        ] {
            assert!(is_non_speech_marker(m), "should filter {m:?}");
        }
        // Real speech must survive, including legitimate parentheticals.
        for s in [
            "The quick brown fox.",
            "(laughs) that's the point.",
            "[inaudible] but we shipped it!",
            "hello",
        ] {
            assert!(!is_non_speech_marker(s), "should keep {s:?}");
        }
    }

    #[test]
    fn fmt_timestamp_formats_hh_mm_ss() {
        assert_eq!(fmt_timestamp(0.0), "00:00:00");
        assert_eq!(fmt_timestamp(65.4), "00:01:05");
        assert_eq!(fmt_timestamp(3661.0), "01:01:01");
    }
}
