//! Silence-driven chunking and WAV writing.
//!
//! CONTRACT (do not change):
//!   pub struct StopFlag; impl StopFlag { fn new() -> Self; fn stop(&self); fn stopped(&self) -> bool; }
//!   impl Clone for StopFlag
//!   pub struct Summary { pub chunks: u32, pub total: std::time::Duration }
//!   pub fn run(consumer, info, dir, tx: mpsc::Sender<Status>, stop: StopFlag) -> Result<Summary>

use crate::types::{CaptureInfo, MAX_CHUNK, MIN_CHUNK, SILENCE_TO_CUT, Status};
use crate::vad::Vad;
use anyhow::Result;
use serde::Serialize;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct StopFlag(Arc<AtomicBool>);

impl StopFlag {
    pub fn new() -> Self {
        StopFlag(Arc::new(AtomicBool::new(false)))
    }
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct Summary {
    pub chunks: u32,
    pub total: Duration,
}

/// Result of feeding one frame-window into the silence state machine.
#[derive(Debug, PartialEq, Eq)]
enum Cut {
    /// Silence exceeded SILENCE_TO_CUT: close the current chunk (if any speech was captured).
    Silence,
    /// MAX_CHUNK reached: force a cut regardless of silence state.
    HardCap,
}

/// Pure decision logic for when to open/close a chunk, driven by a per-window
/// speech/no-speech decision (from the VAD, or the RMS-threshold fallback).
/// No file I/O, no ring buffer — this is what the tests exercise directly.
struct Chunker {
    silence_to_cut: Duration,
    max_chunk: Duration,
    /// Frames of continuous silence seen since the last non-silent frame.
    silence_frames: u64,
    /// Frames written (or pending) in the current open chunk, silence included.
    chunk_frames: u64,
    /// Speech-only frames in the current open chunk (excludes silence runs) —
    /// this plus a bounded trailing-silence tail is what actually ends up on
    /// disk, and is what the MIN_CHUNK runt check is really measuring.
    speech_frames: u64,
    /// Whether we've seen speech since the current chunk (or gap) started.
    has_speech: bool,
    /// Whether a chunk is currently "open" (speech has started, not yet cut).
    open: bool,
    sample_rate: u32,
}

impl Chunker {
    fn new(sample_rate: u32) -> Self {
        Chunker {
            silence_to_cut: SILENCE_TO_CUT,
            max_chunk: MAX_CHUNK,
            silence_frames: 0,
            chunk_frames: 0,
            speech_frames: 0,
            has_speech: false,
            open: false,
            sample_rate,
        }
    }

    fn frames_to_duration(&self, frames: u64) -> Duration {
        Duration::from_secs_f64(frames as f64 / self.sample_rate as f64)
    }

    /// Feed `frames` frames' worth of a speech/no-speech decision. Returns
    /// Some(Cut) when the caller should close (and finalize) the current chunk.
    fn feed(&mut self, speech: bool, frames: u64) -> Option<Cut> {
        let silent = !speech;

        if silent {
            self.silence_frames += frames;
        } else {
            self.silence_frames = 0;
            self.has_speech = true;
            self.open = true;
            self.speech_frames += frames;
        }

        if self.open {
            self.chunk_frames += frames;
        }

        if self.open && self.frames_to_duration(self.chunk_frames) >= self.max_chunk {
            return Some(Cut::HardCap);
        }

        if self.open
            && self.has_speech
            && self.frames_to_duration(self.silence_frames) >= self.silence_to_cut
        {
            return Some(Cut::Silence);
        }

        None
    }

    /// Reset state after a chunk closes, ready for the next one. `carry_silence`
    /// preserves already-accumulated silence frames (the gap continues, it doesn't
    /// restart) so a long silence doesn't reopen/reclose repeatedly.
    fn reset_after_cut(&mut self) {
        self.chunk_frames = 0;
        self.speech_frames = 0;
        self.has_speech = false;
        self.open = false;
        // silence_frames carries forward: we're still in the same silence gap.
    }

    #[cfg(test)]
    /// Frames actually expected to land on disk for the chunk about to close:
    /// speech plus a bounded trailing-silence tail (mirrors the hold-buffer
    /// trim in `run`). This is what MIN_CHUNK should be compared against.
    fn kept_frames(&self, tail_keep_frames: u64) -> u64 {
        self.speech_frames + self.silence_frames.min(tail_keep_frames)
    }
}

#[derive(Serialize)]
struct MetaChunk {
    index: u32,
    file: String,
    duration_secs: f64,
    started_offset_secs: f64,
}

#[derive(Serialize)]
struct Meta {
    started: String,
    sample_rate: u32,
    channels: u16,
    system_channels: (u16, u16),
    mic_channels: (u16, u16),
    /// Vestigial since the VAD took over chunking: kept populated for
    /// compatibility with any reader that expects the field, but see
    /// `detector` for what actually produced these chunks.
    silence_threshold_rms: f32,
    /// Names what decided speech vs. silence for this session: the VAD, or
    /// (only if it failed to initialize) the RMS-threshold fallback.
    detector: String,
    chunks: Vec<MetaChunk>,
}

fn write_meta_atomic(dir: &Path, meta: &Meta) -> Result<()> {
    let tmp = dir.join(".meta.json.tmp");
    let final_path = dir.join("meta.json");
    std::fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
    std::fs::rename(tmp, final_path)?;
    Ok(())
}

/// Open chunk state: the hound writer plus the file handle (needed to fsync,
/// since `WavWriter::finalize` consumes the writer without giving it back).
struct OpenChunk {
    writer: hound::WavWriter<std::io::BufWriter<File>>,
    path: PathBuf,
    index: u32,
    frames_written: u64,
    started_offset: Duration,
}

fn open_chunk(dir: &Path, index: u32, info: &CaptureInfo, offset: Duration) -> Result<OpenChunk> {
    let path = dir.join(format!("chunk-{:03}.wav", index));
    let spec = hound::WavSpec {
        channels: info.channels,
        sample_rate: info.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let writer = hound::WavWriter::create(&path, spec)?;
    Ok(OpenChunk {
        writer,
        path,
        index,
        frames_written: 0,
        started_offset: offset,
    })
}

pub fn run(
    mut consumer: rtrb::Consumer<f32>,
    info: CaptureInfo,
    dir: PathBuf,
    tx: Sender<Status>,
    stop: StopFlag,
) -> Result<Summary> {
    std::fs::create_dir_all(&dir)?;

    // Threshold is vestigial for chunking now (kept for the meta.json field and
    // as the fallback gate below); the VAD drives the actual cut decision.
    let threshold = crate::types::silence_threshold();
    let started = chrono::Local::now();

    // earshot's construction is pure computation (embedded model, no I/O), so
    // it shouldn't fail — but losing a whole recording because a VAD wouldn't
    // init would be far worse than degraded RMS-threshold chunking, so guard
    // it anyway and fall back rather than aborting.
    let mut vad = match std::panic::catch_unwind(Vad::new) {
        Ok(v) => Some(v),
        Err(_) => {
            let _ = tx.send(Status::Warning(
                "VAD failed to initialize; falling back to RMS-threshold chunking".into(),
            ));
            None
        }
    };
    let detector_name = if vad.is_some() {
        "earshot (16kHz mono, 256-sample frames, per-leg)".to_string()
    } else {
        "rms-threshold-fallback".to_string()
    };

    let mut meta = Meta {
        started: started.to_rfc3339(),
        sample_rate: info.sample_rate,
        channels: info.channels,
        system_channels: info.system_channels,
        mic_channels: info.mic_channels,
        silence_threshold_rms: threshold,
        detector: detector_name,
        chunks: Vec::new(),
    };
    write_meta_atomic(&dir, &meta)?;

    let mut chunker = Chunker::new(info.sample_rate);
    // Last known per-leg speech decision — carried across batches so a batch
    // too small to complete a VAD frame doesn't spuriously read as silence.
    let mut last_mic_speech = false;
    let mut last_sys_speech = false;
    let mut next_index: u32 = 0;
    let mut kept_chunks: u32 = 0;
    let mut total = Duration::ZERO;

    let mut current: Option<OpenChunk> = None;
    let mut session_frames: u64 = 0; // frames since session start, for started_offset_secs
    let mut in_silence = false; // for Status::Silence transitions

    // Pre-roll ring: keep last ~200ms of frames so speech onset isn't clipped.
    // Bounded: capacity = 200ms worth of interleaved samples.
    let preroll_frames_cap = (info.sample_rate as u64 * 200 / 1000).max(1);
    let ch = info.channels as usize;
    let mut preroll: Vec<f32> = Vec::with_capacity(preroll_frames_cap as usize * ch);

    // Trailing-silence hold: while a chunk is open and we're in a silence run
    // that might end in a cut, samples go here instead of straight to the file,
    // capped at tail_keep worth of frames. On cut, only this bounded tail is
    // flushed (the rest of the silence is dropped). If speech resumes first,
    // the whole hold flushes — it was just a mid-chunk pause, not trailing silence.
    // Bound: 300ms of frames, same cap style as the pre-roll.
    let tail_keep_frames_cap = (info.sample_rate as u64 * 300 / 1000).max(1);
    let mut hold: Vec<f32> = Vec::new();

    let mut last_dropped: u64 = 0;
    let mut last_level_emit = std::time::Instant::now();
    let level_interval = Duration::from_millis(50); // ~20Hz

    // accumulate level stats between emits
    let mut level_acc = LevelAcc::default();

    let batch_frames: usize = 4096;
    let mut buf: Vec<f32> = Vec::with_capacity(batch_frames * ch);

    loop {
        let available = consumer.slots();
        if available == 0 {
            if stop.stopped() {
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
            check_overrun(&tx, &mut last_dropped);
            continue;
        }

        let take = available.min(batch_frames * ch);
        // Round down to whole frames.
        let take = take - (take % ch);
        if take == 0 {
            std::thread::sleep(Duration::from_millis(8));
            continue;
        }

        buf.clear();
        {
            let chunk_read = consumer.read_chunk(take)?;
            for &s in chunk_read.as_slices().0 {
                buf.push(s);
            }
            for &s in chunk_read.as_slices().1 {
                buf.push(s);
            }
            chunk_read.commit_all();
        }

        let frames = buf.len() / ch;
        if frames == 0 {
            continue;
        }

        let (sys_rms, sys_peak, mic_rms, mic_peak) = compute_levels(&buf, ch, &info);
        level_acc.add(sys_rms, sys_peak, mic_rms, mic_peak);
        if last_level_emit.elapsed() >= level_interval {
            let (s_rms, s_peak, m_rms, m_peak) = level_acc.take();
            let _ = tx.send(Status::Level {
                system_rms: s_rms,
                system_peak: s_peak,
                mic_rms: m_rms,
                mic_peak: m_peak,
            });
            last_level_emit = std::time::Instant::now();
        }

        // Speech on either leg keeps the chunk open — run mic and system legs
        // independently through the VAD (or, if it failed to init, the old
        // RMS threshold) rather than merging into one signal.
        if let Some(vad) = vad.as_mut() {
            let (mic_speech, sys_speech) = vad.feed(&buf, ch, &info);
            if let Some(v) = mic_speech {
                last_mic_speech = v;
            }
            if let Some(v) = sys_speech {
                last_sys_speech = v;
            }
        } else {
            last_mic_speech = mic_rms >= threshold;
            last_sys_speech = sys_rms >= threshold;
        }
        let speech = last_mic_speech || last_sys_speech;
        let cut = chunker.feed(speech, frames as u64);

        let now_silent = !speech;
        if now_silent != in_silence {
            in_silence = now_silent;
            let _ = tx.send(Status::Silence { in_silence });
        }

        // Maintain pre-roll buffer with the most recent preroll_frames_cap frames,
        // used to seed a newly-opened chunk with ~200ms of audio before speech onset.
        if current.is_none() {
            push_preroll(&mut preroll, &buf, ch, preroll_frames_cap);
        }

        // Open a chunk the moment speech is present and none is open yet.
        if chunker.open && current.is_none() {
            let offset = Duration::from_secs_f64(session_frames as f64 / info.sample_rate as f64);
            let mut oc = open_chunk(&dir, next_index, &info, offset)?;
            // Seed with pre-roll so we don't clip the first syllable.
            if !preroll.is_empty() {
                write_samples(&mut oc, &preroll)?;
            }
            let _ = tx.send(Status::ChunkOpened {
                path: oc.path.clone(),
                index: oc.index,
            });
            current = Some(oc);
        }

        // Write this batch into the open chunk. During a silence run, samples are
        // held (bounded, see `hold` above) rather than written immediately, so a
        // long trailing silence never bloats the file; speech resumes flush it.
        if let Some(oc) = current.as_mut() {
            if now_silent {
                push_preroll(&mut hold, &buf, ch, tail_keep_frames_cap);
            } else {
                if !hold.is_empty() {
                    write_samples(oc, &hold)?;
                    hold.clear();
                }
                write_samples(oc, &buf)?;
            }
        }

        session_frames += frames as u64;

        if let Some(cut_kind) = cut {
            if let Some(mut oc) = current.take() {
                if cut_kind == Cut::Silence && !hold.is_empty() {
                    // Flush the bounded trailing-silence tail so it's part of the file.
                    write_samples(&mut oc, &hold)?;
                }
                hold.clear();
                finish_chunk(oc, &dir, &mut meta, &tx, &mut kept_chunks, &mut total)?;
                next_index += 1;
            }
            chunker.reset_after_cut();
            preroll.clear();
        }

        check_overrun(&tx, &mut last_dropped);

        if stop.stopped() && consumer.slots() == 0 {
            break;
        }
    }

    // Session ended: close whatever chunk is still open. Flush any held tail
    // silence too — it's the true end of the session, not a trimmed cut.
    if let Some(mut oc) = current.take() {
        if !hold.is_empty() {
            write_samples(&mut oc, &hold)?;
        }
        finish_chunk(oc, &dir, &mut meta, &tx, &mut kept_chunks, &mut total)?;
    }

    let _ = tx.send(Status::Finished {
        total_chunks: kept_chunks,
        total,
    });

    Ok(Summary {
        chunks: kept_chunks,
        total,
    })
}

fn write_samples(oc: &mut OpenChunk, samples: &[f32]) -> Result<()> {
    for &s in samples {
        oc.writer.write_sample(s)?;
    }
    oc.frames_written += (samples.len() / oc.writer.spec().channels as usize) as u64;
    Ok(())
}

fn push_preroll(preroll: &mut Vec<f32>, batch: &[f32], ch: usize, cap_frames: u64) {
    preroll.extend_from_slice(batch);
    let cap_samples = cap_frames as usize * ch;
    if preroll.len() > cap_samples {
        let drop = preroll.len() - cap_samples;
        preroll.drain(0..drop);
    }
}

fn finish_chunk(
    oc: OpenChunk,
    dir: &Path,
    meta: &mut Meta,
    tx: &Sender<Status>,
    kept_chunks: &mut u32,
    total: &mut Duration,
) -> Result<()> {
    let index = oc.index;
    let started_offset = oc.started_offset;
    let frames_written = oc.frames_written;
    let path = oc.path.clone();
    let sample_rate = oc.writer.spec().sample_rate;

    oc.writer.finalize()?;
    let file = File::open(&path)?;
    file.sync_all()?;
    let bytes = file.metadata()?.len();

    let duration = Duration::from_secs_f64(frames_written as f64 / sample_rate as f64);

    if duration < MIN_CHUNK {
        std::fs::remove_file(&path)?;
        let _ = tx.send(Status::ChunkDiscarded { index, duration });
        return Ok(());
    }

    *kept_chunks += 1;
    *total += duration;

    meta.chunks.push(MetaChunk {
        index,
        file: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        duration_secs: duration.as_secs_f64(),
        started_offset_secs: started_offset.as_secs_f64(),
    });
    write_meta_atomic(dir, meta)?;

    let _ = tx.send(Status::ChunkClosed {
        path,
        index,
        duration,
        bytes,
    });
    Ok(())
}

fn check_overrun(tx: &Sender<Status>, last_dropped: &mut u64) {
    let dropped = crate::capture::dropped_samples();
    if dropped > *last_dropped {
        let _ = tx.send(Status::Overrun {
            dropped_samples: dropped - *last_dropped,
        });
        *last_dropped = dropped;
    }
}

#[derive(Default)]
struct LevelAcc {
    sys_rms_sum: f64,
    mic_rms_sum: f64,
    sys_peak: f32,
    mic_peak: f32,
    n: u32,
}

impl LevelAcc {
    fn add(&mut self, sys_rms: f32, sys_peak: f32, mic_rms: f32, mic_peak: f32) {
        self.sys_rms_sum += sys_rms as f64;
        self.mic_rms_sum += mic_rms as f64;
        self.sys_peak = self.sys_peak.max(sys_peak);
        self.mic_peak = self.mic_peak.max(mic_peak);
        self.n += 1;
    }
    fn take(&mut self) -> (f32, f32, f32, f32) {
        let n = self.n.max(1) as f64;
        let out = (
            (self.sys_rms_sum / n) as f32,
            self.sys_peak,
            (self.mic_rms_sum / n) as f32,
            self.mic_peak,
        );
        *self = LevelAcc::default();
        out
    }
}

/// Compute RMS/peak for the system leg and mic leg from an interleaved f32 buffer.
fn compute_levels(buf: &[f32], ch: usize, info: &CaptureInfo) -> (f32, f32, f32, f32) {
    let frames = buf.len() / ch;
    let mut sys_sq = 0f64;
    let mut mic_sq = 0f64;
    let mut sys_peak = 0f32;
    let mut mic_peak = 0f32;
    let (s0, s1) = (info.system_channels.0 as usize, info.system_channels.1 as usize);
    let (m0, m1) = (info.mic_channels.0 as usize, info.mic_channels.1 as usize);

    for f in 0..frames {
        let base = f * ch;
        for &c in &[s0, s1] {
            if c < ch {
                let v = buf[base + c];
                sys_sq += (v as f64) * (v as f64);
                sys_peak = sys_peak.max(v.abs());
            }
        }
        for &c in &[m0, m1] {
            if c < ch {
                let v = buf[base + c];
                mic_sq += (v as f64) * (v as f64);
                mic_peak = mic_peak.max(v.abs());
            }
        }
    }

    let sys_rms = if frames > 0 {
        (sys_sq / (frames as f64 * 2.0)).sqrt() as f32
    } else {
        0.0
    };
    let mic_rms = if frames > 0 {
        (mic_sq / (frames as f64 * 2.0)).sqrt() as f32
    } else {
        0.0
    };
    (sys_rms, sys_peak, mic_rms, mic_peak)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn silent_frames(c: &mut Chunker, secs: f64) -> Option<Cut> {
        let frames = (SR as f64 * secs) as u64;
        c.feed(false, frames)
    }

    fn loud_frames(c: &mut Chunker, secs: f64) -> Option<Cut> {
        let frames = (SR as f64 * secs) as u64;
        c.feed(true, frames)
    }

    #[test]
    fn short_silence_does_not_cut() {
        let mut c = Chunker::new(SR);
        assert!(loud_frames(&mut c, 1.0).is_none());
        // silence shorter than SILENCE_TO_CUT (2s)
        assert!(silent_frames(&mut c, 1.0).is_none());
    }

    #[test]
    fn long_silence_cuts() {
        let mut c = Chunker::new(SR);
        assert!(loud_frames(&mut c, 1.0).is_none());
        let cut = silent_frames(&mut c, 2.5);
        assert_eq!(cut, Some(Cut::Silence));
    }

    #[test]
    fn no_cut_without_speech_during_long_silence() {
        // Pure silence from the start: chunk never opens, so nothing to cut,
        // and no empty chunk should ever be created by the caller.
        let mut c = Chunker::new(SR);
        let cut = silent_frames(&mut c, 10.0);
        assert!(cut.is_none());
        assert!(!c.open);
    }

    #[test]
    fn hard_cap_fires_with_no_silence() {
        let mut c = Chunker::new(SR);
        assert!(loud_frames(&mut c, 1.0).is_none());
        // Keep feeding loud audio past MAX_CHUNK (300s) with zero silence.
        let cut = loud_frames(&mut c, 300.0);
        assert_eq!(cut, Some(Cut::HardCap));
    }

    #[test]
    fn runt_chunk_duration_is_detected_as_under_min() {
        // A short burst of speech (100ms) followed by a cut: what actually lands
        // on disk is speech + a bounded 300ms trailing-silence tail (mirrors the
        // hold-buffer trim in `run`), not the full silence run — that combined
        // duration should fall under MIN_CHUNK so the caller discards it.
        let mut c = Chunker::new(SR);
        assert!(loud_frames(&mut c, 0.1).is_none()); // 100ms of speech
        let cut = silent_frames(&mut c, 2.5);
        assert_eq!(cut, Some(Cut::Silence));
        let tail_keep_frames = (SR as u64 * 300) / 1000;
        let duration = c.frames_to_duration(c.kept_frames(tail_keep_frames));
        assert!(duration < MIN_CHUNK, "expected runt duration, got {duration:?}");
    }

    #[test]
    fn reset_after_cut_allows_reopening() {
        let mut c = Chunker::new(SR);
        assert!(loud_frames(&mut c, 1.0).is_none());
        assert_eq!(silent_frames(&mut c, 2.5), Some(Cut::Silence));
        c.reset_after_cut();
        assert!(!c.open);
        assert!(!c.has_speech);
        // Speech resumes and a fresh chunk can open again.
        assert!(loud_frames(&mut c, 0.5).is_none());
        assert!(c.open);
    }
}
