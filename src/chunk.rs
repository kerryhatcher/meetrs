//! Silence-driven chunking and WAV writing.
//!
//! CONTRACT (do not change):
//!   pub struct StopFlag; impl StopFlag { fn new() -> Self; fn stop(&self); fn stopped(&self) -> bool; }
//!   impl Clone for StopFlag
//!   pub struct Summary { pub chunks: u32, pub total: std::time::Duration, pub sessions: Vec<PathBuf> }
//!   pub fn run(consumer, info, dir, tx: mpsc::Sender<Status>, jobs: mpsc::Sender<Job>, stop: StopFlag) -> Result<Summary>

use crate::types::{
    CaptureInfo, Job, MAX_CHUNK, MIN_CHUNK, SILENCE_TO_CUT, Status, next_midnight, session_dir,
    start_of_day,
};
use crate::vad::Vad;
use anyhow::Result;
use serde::Serialize;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
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
    /// Kept chunks and audio across every session of the run, not just the last.
    pub chunks: u32,
    pub total: Duration,
    /// Every session directory the run wrote, in order. A recording left running
    /// past midnight spans more than one.
    pub sessions: Vec<PathBuf>,
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

/// Start a session in `dir`: create it, write an empty `meta.json` (the
/// transcriber reads the channel map from it before any chunk exists), and
/// register it in the index. Used for the first session and for each midnight
/// rollover, so both come out identical.
///
/// The index is best-effort — `meta.json` is the source of truth — so a failure
/// there warns and degrades rather than failing the session.
fn begin_session(
    dir: &Path,
    started: chrono::DateTime<chrono::Local>,
    info: &CaptureInfo,
    threshold: f32,
    detector: &str,
    db: Option<&mut crate::db::Db>,
    tx: &Sender<Status>,
) -> Result<Meta> {
    std::fs::create_dir_all(dir)?;
    let meta = Meta {
        started: started.to_rfc3339(),
        sample_rate: info.sample_rate,
        channels: info.channels,
        system_channels: info.system_channels,
        mic_channels: info.mic_channels,
        silence_threshold_rms: threshold,
        detector: detector.to_string(),
        chunks: Vec::new(),
    };
    write_meta_atomic(dir, &meta)?;
    if let Some(db) = db
        && let Err(e) = db.start_session(dir, info, &meta.detector, &meta.started)
    {
        let _ = tx.send(Status::Warning(format!("db: {e:#}")));
    }
    Ok(meta)
}

pub fn run(
    consumer: rtrb::Consumer<f32>,
    info: CaptureInfo,
    dir: PathBuf,
    tx: Sender<Status>,
    jobs: Sender<Job>,
    stop: StopFlag,
) -> Result<Summary> {
    // The DB is a derived index; meta.json is the source of truth. So every DB
    // call is best-effort: a failure warns and degrades to "no index", it never
    // costs a recording. `db::rebuild()` can repopulate later.
    let db = match crate::db::open() {
        Ok(d) => Some(d),
        Err(e) => {
            let _ = tx.send(Status::Warning(format!(
                "db unavailable, not indexing: {e:#}"
            )));
            None
        }
    };
    let mut clock = chrono::Local::now;
    let mut new_dir_for = |day_start| session_dir(start_of_day(day_start));
    record(
        consumer,
        info,
        dir,
        db,
        &mut clock,
        &mut new_dir_for,
        tx,
        jobs,
        stop,
    )
}

/// The writer loop behind [`run`], with the outside world passed in: `clock`
/// says what time it is, `new_dir_for` picks the directory for the session that
/// starts at the given time, and `db` is the index. That is what lets a test
/// carry a recording across midnight without waiting for one or touching `$HOME`.
#[allow(clippy::too_many_arguments)]
fn record(
    mut consumer: rtrb::Consumer<f32>,
    info: CaptureInfo,
    first_dir: PathBuf,
    mut db: Option<crate::db::Db>,
    clock: &mut dyn FnMut() -> chrono::DateTime<chrono::Local>,
    new_dir_for: &mut dyn FnMut(chrono::DateTime<chrono::Local>) -> Result<PathBuf>,
    tx: Sender<Status>,
    jobs: Sender<Job>,
    stop: StopFlag,
) -> Result<Summary> {
    let mut dir = first_dir;

    // Threshold is vestigial for chunking now (kept for the meta.json field and
    // as the fallback gate below); the VAD drives the actual cut decision.
    let threshold = crate::types::silence_threshold();
    let started = clock();

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

    let mut meta = begin_session(
        &dir,
        started,
        &info,
        threshold,
        &detector_name,
        db.as_mut(),
        &tx,
    )?;

    // Each calendar day is its own session: a recording left running for days
    // would otherwise mix every conversation into one log. See the rollover
    // check at the top of the loop.
    let mut rollover_at = next_midnight(started);
    let mut sessions = vec![dir.clone()];
    // Totals for sessions already closed by a rollover; `kept_chunks`/`total`
    // below describe only the current session.
    let mut closed_chunks: u32 = 0;
    let mut closed_total = Duration::ZERO;

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

        // Midnight: close out the day's session and start the next one in its own
        // directory, so this batch (and everything after) lands in the new day.
        // The new session is created before the old one is touched, so a failure
        // (disk full, say) leaves the recording running in the old directory
        // rather than losing audio; it retries a minute later.
        let now = clock();
        if now >= rollover_at {
            let begun = new_dir_for(now).and_then(|new_dir| {
                let new_meta = begin_session(
                    &new_dir,
                    now,
                    &info,
                    threshold,
                    &detector_name,
                    db.as_mut(),
                    &tx,
                )?;
                Ok((new_dir, new_meta))
            });
            match begun {
                Err(e) => {
                    let _ = tx.send(Status::Warning(format!(
                        "could not start the new day's session, still recording into {}: {e:#}",
                        dir.display()
                    )));
                    rollover_at = now + chrono::Duration::minutes(1);
                }
                Ok((new_dir, new_meta)) => {
                    if let Some(mut oc) = current.take() {
                        if !hold.is_empty() {
                            write_samples(&mut oc, &hold)?;
                        }
                        finish_chunk(
                            oc,
                            &dir,
                            &mut meta,
                            &mut Sinks {
                                tx: &tx,
                                jobs: &jobs,
                                db: db.as_mut(),
                            },
                            &mut kept_chunks,
                            &mut total,
                        )?;
                    }
                    hold.clear();
                    preroll.clear();
                    if let Some(db) = db.as_mut()
                        && let Err(e) = db.finish_session(&dir, kept_chunks, total.as_secs_f64())
                    {
                        let _ = tx.send(Status::Warning(format!("db: {e:#}")));
                    }
                    closed_chunks += kept_chunks;
                    closed_total += total;

                    dir = new_dir;
                    meta = new_meta;
                    sessions.push(dir.clone());
                    next_index = 0;
                    kept_chunks = 0;
                    total = Duration::ZERO;
                    session_frames = 0;
                    // The chunk just closed is over; if speech carries on, the
                    // chunker reopens one in the new directory this same batch.
                    chunker.reset_after_cut();
                    rollover_at = next_midnight(now);
                    let _ = tx.send(Status::SessionStarted { dir: dir.clone() });
                }
            }
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
                finish_chunk(
                    oc,
                    &dir,
                    &mut meta,
                    &mut Sinks {
                        tx: &tx,
                        jobs: &jobs,
                        db: db.as_mut(),
                    },
                    &mut kept_chunks,
                    &mut total,
                )?;
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
        finish_chunk(
            oc,
            &dir,
            &mut meta,
            &mut Sinks {
                tx: &tx,
                jobs: &jobs,
                db: db.as_mut(),
            },
            &mut kept_chunks,
            &mut total,
        )?;
    }

    if let Some(db) = db.as_mut()
        && let Err(e) = db.finish_session(&dir, kept_chunks, total.as_secs_f64())
    {
        let _ = tx.send(Status::Warning(format!("db: {e:#}")));
    }

    let chunks = closed_chunks + kept_chunks;
    let total = closed_total + total;
    let _ = tx.send(Status::Finished {
        total_chunks: chunks,
        total,
    });

    Ok(Summary {
        chunks,
        total,
        sessions,
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

/// Where a closed chunk gets reported: the UI, the transcription queue, and the
/// (optional, best-effort) SQLite index.
struct Sinks<'a> {
    tx: &'a Sender<Status>,
    jobs: &'a Sender<Job>,
    db: Option<&'a mut crate::db::Db>,
}

fn finish_chunk(
    oc: OpenChunk,
    dir: &Path,
    meta: &mut Meta,
    sinks: &mut Sinks<'_>,
    kept_chunks: &mut u32,
    total: &mut Duration,
) -> Result<()> {
    let tx = sinks.tx;
    let jobs = sinks.jobs;
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

    if let Some(db) = sinks.db.as_mut() {
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Err(e) = db.record_chunk(
            dir,
            index,
            &file,
            duration.as_secs_f64(),
            started_offset.as_secs_f64(),
            bytes,
        ) {
            let _ = tx.send(Status::Warning(format!("db: {e:#}")));
        }
    }

    // Queue for transcription only after meta.json is on disk — the worker reads
    // the channel map from it, so sending earlier would race.
    let _ = jobs.send(Job {
        path: path.clone(),
        index,
        offset: started_offset,
    });

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
    let (s0, s1) = (
        info.system_channels.0 as usize,
        info.system_channels.1 as usize,
    );
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

    fn test_info() -> CaptureInfo {
        CaptureInfo {
            channels: 4,
            sample_rate: SR,
            system_channels: (0, 1),
            mic_channels: (2, 3),
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meetrs-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Both the first session and every midnight rollover go through here, so
    /// the new day's directory has to come out complete: created, with a
    /// `meta.json` the transcriber can read before any chunk exists.
    #[test]
    fn begin_session_creates_the_directory_with_an_empty_meta() {
        let dir = scratch_dir("begin").join("2026-10-20T00-00-00");
        let started = chrono::Local::now();
        let (tx, _rx) = std::sync::mpsc::channel();

        let meta = begin_session(&dir, started, &test_info(), 0.02, "earshot", None, &tx).unwrap();

        assert_eq!(meta.started, started.to_rfc3339());
        assert!(meta.chunks.is_empty());
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(on_disk["started"], started.to_rfc3339());
        assert_eq!(on_disk["channels"], 4);
        assert_eq!(on_disk["chunks"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn begin_session_registers_the_session_in_the_index() {
        let root = scratch_dir("begin-db");
        std::fs::create_dir_all(&root).unwrap();
        let mut db = crate::db::open_at(&root.join("test.db")).unwrap();
        let dir = root.join("2026-10-20T00-00-00");
        let (tx, rx) = std::sync::mpsc::channel();

        begin_session(&dir, chrono::Local::now(), &test_info(), 0.02, "earshot", Some(&mut db), &tx)
            .unwrap();

        // A closed chunk can only be recorded against a registered session.
        db.record_chunk(&dir, 0, "chunk-000.wav", 1.0, 0.0, 10).unwrap();
        assert!(rx.try_recv().is_err(), "no warning expected");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn local(day: u32, h: u32, m: u32, s: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 1, day)
            .unwrap()
            .and_hms_opt(h, m, s)
            .unwrap();
        chrono::Local.from_local_datetime(&naive).unwrap()
    }

    /// Drive `record` over three batches of silence on a clock that reads
    /// 23:59:59 for the start and first batch, then 00:00:01 the next day.
    /// `new_dir_for` is the next-session directory chooser under test.
    fn record_across_midnight(
        root: &Path,
        db: crate::db::Db,
        new_dir_for: &mut dyn FnMut(chrono::DateTime<chrono::Local>) -> Result<PathBuf>,
    ) -> (Summary, Vec<Status>) {
        let info = test_info();
        let batch_samples = 4096 * info.channels as usize;
        let (mut producer, consumer) = rtrb::RingBuffer::new(batch_samples * 3);
        for _ in 0..batch_samples * 3 {
            producer.push(0.0).unwrap();
        }
        let stop = StopFlag::new();
        stop.stop(); // drain what is queued, then finish
        let (tx, rx) = std::sync::mpsc::channel();
        let (jobs, _jobs_rx) = std::sync::mpsc::channel();

        let mut calls = 0;
        let mut clock = move || {
            calls += 1;
            if calls <= 2 {
                local(15, 23, 59, 59)
            } else {
                local(16, 0, 0, 1)
            }
        };
        let summary = record(
            consumer,
            info,
            root.join("day1"),
            Some(db),
            &mut clock,
            new_dir_for,
            tx,
            jobs,
            stop,
        )
        .unwrap();
        (summary, rx.try_iter().collect())
    }

    #[test]
    fn crossing_midnight_starts_a_second_session_in_its_own_directory() {
        let root = scratch_dir("midnight");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("test.db");
        let day2 = root.join("day2");
        let mut chose = None;
        let mut new_dir_for = |t: chrono::DateTime<chrono::Local>| {
            chose = Some(t);
            Ok(day2.clone())
        };

        let (summary, statuses) =
            record_across_midnight(&root, crate::db::open_at(&db_path).unwrap(), &mut new_dir_for);

        assert_eq!(summary.sessions, vec![root.join("day1"), day2.clone()]);
        assert_eq!(chose, Some(local(16, 0, 0, 1)));
        for dir in &summary.sessions {
            assert!(dir.join("meta.json").is_file(), "{} has no meta.json", dir.display());
        }
        let day2_meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(day2.join("meta.json")).unwrap()).unwrap();
        assert_eq!(day2_meta["started"], local(16, 0, 0, 1).to_rfc3339());
        let rolled: Vec<_> = statuses
            .iter()
            .filter_map(|s| match s {
                Status::SessionStarted { dir } => Some(dir.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(rolled, vec![day2.clone()]);

        // Both sessions were registered in the index.
        let mut db = crate::db::open_at(&db_path).unwrap();
        for dir in &summary.sessions {
            db.record_chunk(dir, 0, "chunk-000.wav", 1.0, 0.0, 10).unwrap();
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A failed rollover (disk full, say) must cost nothing: keep recording into
    /// the old directory and say so, rather than aborting a multi-day recording.
    #[test]
    fn a_failed_midnight_rollover_keeps_recording_into_the_old_session() {
        let root = scratch_dir("midnight-fail");
        std::fs::create_dir_all(&root).unwrap();
        let mut new_dir_for = |_| Err(anyhow::anyhow!("disk full"));

        let (summary, statuses) = record_across_midnight(
            &root,
            crate::db::open_at(&root.join("test.db")).unwrap(),
            &mut new_dir_for,
        );

        assert_eq!(summary.sessions, vec![root.join("day1")]);
        assert!(statuses.iter().any(
            |s| matches!(s, Status::Warning(m) if m.contains("still recording") && m.contains("disk full"))
        ));
        assert!(!statuses.iter().any(|s| matches!(s, Status::SessionStarted { .. })));
        let _ = std::fs::remove_dir_all(&root);
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
        assert!(
            duration < MIN_CHUNK,
            "expected runt duration, got {duration:?}"
        );
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
