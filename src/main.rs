//! meetrs — terminal meeting recorder.
//!
//! Captures microphone + system audio into one synchronized stream, writes WAV
//! chunks split on natural pauses so a crash costs at most one chunk, and
//! transcribes each chunk locally as soon as it lands. State and a full-text
//! index over transcripts live in SQLite at `~/.meetrs/meetrs.db`.

mod capture;
mod chunk;
mod compress;
mod db;
mod lock;
mod model;
mod tcc;
mod transcribe;
mod types;
mod ui;
mod vad;

use anyhow::{Context, Result};
use std::sync::mpsc;
use std::time::Duration;

/// Ring buffer capacity in samples. At 48kHz × 4ch that is ~4 seconds of slack
/// between the realtime callback and the writer thread — generous, because the
/// cost of overrun is corrupt audio and the cost of the memory is 3MB.
const RING_SAMPLES: usize = SAMPLE_RATE_HINT * 4 * 4;
const SAMPLE_RATE_HINT: usize = 48_000;

fn main() -> Result<()> {
    // Read-only queries don't touch the audio device, so they run without the
    // single-instance lock — you can search while a recording is in progress.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--search") => return search(&args[1..].join(" ")),
        Some("--reindex") => return reindex(),
        _ => {}
    }

    // Before anything opens the audio device: TCC judges the *responsible*
    // process, which for a shell-launched program is the terminal, and terminals
    // have no NSAudioCaptureUsageDescription — so the tap is refused without a
    // prompt and the IOProc silently never runs. Re-exec so meetrs.app is the
    // subject instead. See src/tcc.rs. This replaces the process image, so it
    // has to happen before the lock and before the TUI.
    if args.first().map(String::as_str) != Some("--compress") {
        match tcc::adopt_own_identity()? {
            tcc::Identity::Own => {}
            tcc::Identity::Unbundled => eprintln!(
                "meetrs: running outside meetrs.app, so system audio will be denied \
                 (the microphone still works). Install with `just install` and run \
                 the installed `meetrs`."
            ),
        }
    }

    // Held for the whole process. Two instances would fight over the same audio
    // device and interleave writes into the same recordings tree, so this covers
    // `--check` too — it opens capture as well. `--compress` takes it as well:
    // it deletes audio files, which must never happen under a live recording.
    let _lock = lock::acquire()?;

    if args.first().map(String::as_str) == Some("--compress") {
        return compress_old(&args[1..]);
    }

    // whisper.cpp and ggml log to stderr by default, which would scribble over
    // the TUI's alternate screen. This reroutes them into the `log` crate, and
    // since no `log` backend is enabled they go nowhere. Must happen before any
    // WhisperContext is created.
    whisper_rs::install_logging_hooks();

    if std::env::args().any(|a| a == "--check") {
        return check();
    }

    // Fail before touching the terminal, so errors are readable.
    let started = chrono::Local::now();
    let dir = types::session_dir(started)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating session dir {}", dir.display()))?;

    // Fetch the model before the TUI takes the screen, so first-run download
    // progress is plain readable stdout rather than fighting the alternate
    // screen. Steady state is a checksum check and no network at all.
    let model = model::ensure(|done, total| {
        match done.saturating_mul(100).checked_div(total) {
            Some(pct) => eprint!("\rdownloading model… {pct}%"),
            // total==0 means the server sent no Content-Length.
            None => eprint!("\rdownloading model… {done} bytes"),
        }
    })
    .context("obtaining a Whisper model")?;
    eprintln!();

    // Create and migrate the index once, here, before any worker thread opens
    // it. Failing to index is not worth aborting a recording over, so this warns
    // rather than returning — but it warns to stdout, before the TUI exists.
    if let Err(e) = db::init() {
        eprintln!(
            "meetrs: search index unavailable ({e:#}); recording anyway, run --reindex later"
        );
    }

    let (info, mut consumer) = capture::start(RING_SAMPLES).context(
        "starting audio capture — if this is a permissions failure, see the README \
         section on TCC consent and codesigning",
    )?;

    // Before the TUI takes the screen, while errors are still readable: confirm
    // this session will actually capture something. Non-destructive, so the
    // recording still starts at t=0.
    if let Err(e) = preflight(&mut consumer, info) {
        // Tear the aggregate device back down and take the session directory with
        // it: nothing was recorded, and a refused launch should not leave an empty
        // directory behind every time. `remove_dir` only removes it when empty, so
        // this can never discard audio.
        capture::stop();
        let _ = std::fs::remove_dir(&dir);
        return Err(e);
    }

    let (tx, rx) = mpsc::channel();
    // Chunks are queued here the moment they are fsynced, so transcription of
    // chunk 0 overlaps recording of chunk 1 rather than waiting for the session
    // to end.
    let (jobs_tx, jobs_rx) = mpsc::channel();
    let stop = chunk::StopFlag::new();

    // Both worker threads report to the same UI channel.
    let asr_tx = tx.clone();

    let writer = {
        let dir = dir.clone();
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("meetrs-writer".into())
            .spawn(move || chunk::run(consumer, info, dir, tx, jobs_tx, stop))
            .context("spawning writer thread")?
    };

    let asr = {
        let dir = dir.clone();
        std::thread::Builder::new()
            .name("meetrs-transcribe".into())
            .spawn(move || transcribe::run(jobs_rx, model, dir, asr_tx))
            .context("spawning transcribe thread")?
    };

    let outcome = ui::run(rx, &dir, info, &stop);

    // Always stop capture and the writer and join, even if the UI failed, so
    // the aggregate device is torn down and the current chunk closes.
    capture::stop();
    stop.stop();
    let write_result = writer
        .join()
        .map_err(|_| anyhow::anyhow!("writer thread panicked"))?;

    // Restore the terminal before reporting anything.
    outcome?;
    let summary = write_result?;

    // The writer has dropped its jobs sender by now, so the transcriber will
    // finish its queue and exit. Wait for it: the audio is already safe, but
    // silently discarding queued transcription would be a surprise. Recording
    // is the guarantee, transcription is best-effort — so a failure here is
    // reported, not propagated.
    println!(
        "{} chunk(s), {:.1}s total — finishing transcription…",
        summary.chunks,
        summary.total.as_secs_f32()
    );
    let transcribed_cleanly = match asr.join() {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            eprintln!("transcription ended early: {e:#}");
            false
        }
        Err(_) => {
            eprintln!("transcription thread panicked; audio is still intact");
            false
        }
    };

    // Compress only what has actually been transcribed. If transcription bailed
    // out, some chunk may still need its float WAV for a retry, so the audio
    // stays exactly as recorded.
    if transcribed_cleanly {
        compress_session(&dir);
    } else {
        eprintln!("skipping compression: chunks may still need re-transcribing");
    }

    println!("{}", dir.display());
    Ok(())
}

/// `--compress [dir...]`: compress sessions recorded before this was automatic.
/// With no arguments it sweeps every session under `~/.meetrs/recordings`;
/// already-compressed sessions have no WAVs left and cost a directory listing.
///
/// Chunks that were never transcribed keep their WAVs — see `compress::run`.
fn compress_old(dirs: &[String]) -> Result<()> {
    let sessions: Vec<std::path::PathBuf> = if dirs.is_empty() {
        compress::sessions()?
    } else {
        dirs.iter().map(std::path::PathBuf::from).collect()
    };
    if sessions.is_empty() {
        println!("no recordings to compress");
        return Ok(());
    }

    let mut total = compress::Savings::default();
    for dir in &sessions {
        let mut warn = |msg: String| eprintln!("meetrs: {}: {msg}", dir.display());
        match compress::run(dir, &mut warn) {
            Ok(s) => {
                if s.files > 0 {
                    println!(
                        "{}: {} chunk(s), {:.1} MB → {:.1} MB ({:.0}% smaller)",
                        dir.display(),
                        s.files,
                        s.before as f64 / 1e6,
                        s.after as f64 / 1e6,
                        s.percent_saved()
                    );
                }
                total.files += s.files;
                total.before += s.before;
                total.after += s.after;
            }
            // One unreadable session must not abort the sweep.
            Err(e) => eprintln!("meetrs: {}: skipped: {e:#}", dir.display()),
        }
    }

    if total.files == 0 {
        println!(
            "nothing to compress ({} session(s) checked)",
            sessions.len()
        );
    } else {
        println!(
            "total: {} chunk(s) across {} session(s), {:.1} MB → {:.1} MB ({:.0}% smaller)",
            total.files,
            sessions.len(),
            total.before as f64 / 1e6,
            total.after as f64 / 1e6,
            total.percent_saved()
        );
    }
    Ok(())
}

/// Shrink the session's WAVs to FLAC. Recording and transcription are already
/// done and durable by this point, so nothing here is worth failing over — a
/// problem costs disk space, never audio.
fn compress_session(dir: &std::path::Path) {
    let mut warn = |msg: String| eprintln!("meetrs: {msg}");
    match compress::run(dir, &mut warn) {
        Ok(s) if s.files > 0 => println!(
            "compressed {} chunk(s) to FLAC: {:.1} MB → {:.1} MB ({:.0}% smaller)",
            s.files,
            s.before as f64 / 1e6,
            s.after as f64 / 1e6,
            s.percent_saved()
        ),
        Ok(_) => {}
        Err(e) => eprintln!("meetrs: compression skipped: {e:#}"),
    }
}

/// Small helper the UI uses to bound its redraw rate.
pub const FRAME: Duration = Duration::from_millis(50);

/// `--search <fts5 query>`: full-text search across every transcript.
fn search(query: &str) -> Result<()> {
    if query.trim().is_empty() {
        anyhow::bail!(
            "usage: meetrs --search <query>\n\
             supports FTS5 syntax: \"exact phrase\", term1 OR term2, budget*, NEAR(a b)"
        );
    }
    let db = db::open()?;
    let hits = db.search(query, 50)?;
    if hits.is_empty() {
        println!("no matches for {query:?}");
        return Ok(());
    }
    for h in &hits {
        println!(
            "{}  {}  chunk-{:03} [{}]\n  {}\n  {}\n",
            h.started_at,
            fmt_hms(h.start_secs),
            h.chunk_index,
            h.leg,
            h.snippet.replace('\n', " "),
            h.session_dir
        );
    }
    println!("{} match(es)", hits.len());
    Ok(())
}

/// `--reindex`: rebuild the SQLite index from the JSON on disk. The DB is a
/// derived artifact, so this is always safe — and it is the recovery path if the
/// DB is deleted, corrupted, or was unavailable while recording.
fn reindex() -> Result<()> {
    let mut db = db::open()?;
    let n = db.rebuild()?;
    println!("indexed {n} chunk(s)");
    Ok(())
}

fn fmt_hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// How long the launch preflight watches the stream. Long enough to see audio
/// flowing, short enough not to feel like a stall when nothing is playing.
const PREFLIGHT_WINDOW: Duration = Duration::from_millis(1000);

/// What the opening moments of the stream looked like, per leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Opening {
    frames: usize,
    mic_signal: bool,
    system_signal: bool,
}

/// Classify interleaved samples by leg, counting a leg as live once any sample on
/// it is non-zero. Bit-exact zero is the signal here: a denied mic or a refused
/// tap both deliver *digital* silence, which a real microphone never does.
fn summarize<'a>(samples: impl Iterator<Item = &'a f32>, info: types::CaptureInfo) -> Opening {
    let channels = info.channels as usize;
    let mut opening = Opening {
        frames: 0,
        mic_signal: false,
        system_signal: false,
    };
    let mut count = 0usize;
    for (i, sample) in samples.enumerate() {
        count = i + 1;
        if *sample == 0.0 {
            continue;
        }
        let channel = (i % channels) as u16;
        if channel >= info.mic_channels.0 && channel <= info.mic_channels.1 {
            opening.mic_signal = true;
        } else if channel >= info.system_channels.0 && channel <= info.system_channels.1 {
            opening.system_signal = true;
        }
    }
    opening.frames = count / channels;
    opening
}

/// Watch the start of the stream *without consuming it*, so the recording still
/// contains everything from t=0. Returns as soon as both legs have shown signal,
/// or when the window closes.
fn observe_opening(consumer: &mut rtrb::Consumer<f32>, info: types::CaptureInfo) -> Opening {
    let channels = info.channels as usize;
    let deadline = std::time::Instant::now() + PREFLIGHT_WINDOW;
    let mut opening = Opening {
        frames: 0,
        mic_signal: false,
        system_signal: false,
    };
    while std::time::Instant::now() < deadline {
        let frames = consumer.slots() / channels;
        if frames > 0 {
            // Re-read from the same position every pass: a ReadChunk that is
            // dropped without commit()/commit_all() leaves the data in the ring,
            // so the writer thread still gets these samples (see tests).
            if let Ok(chunk) = consumer.read_chunk(frames * channels) {
                let (a, b) = chunk.as_slices();
                opening = summarize(a.iter().chain(b.iter()), info);
                if opening.mic_signal && opening.system_signal {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    opening
}

/// Refuse to start a recording that is provably going to capture nothing, and
/// warn where the cause is genuinely ambiguous.
///
/// The ambiguity is unavoidable and worth spelling out: the aggregate is
/// tap-driven, so before a meeting starts — nothing playing — no callbacks arrive
/// at all, which looks identical to a refused tap. Aborting on that would make
/// meetrs impossible to launch *ahead* of a meeting, which is when you want it.
/// So only the provable cases abort.
fn preflight(consumer: &mut rtrb::Consumer<f32>, info: types::CaptureInfo) -> Result<()> {
    // The failure this exists to catch: with TCC attributing us to the terminal,
    // the tap is refused and the whole session is silence. Always fatal, and
    // cheap to check — no waiting on audio. See src/tcc.rs.
    if let Some((pid, path)) = tcc::foreign_responsible_process() {
        anyhow::bail!(
            "TCC is attributing this process to {} (pid {}) rather than to meetrs, so \
             system audio will be refused and the recording would be silent.\n\
             Run the installed bundle (`just install`, then `meetrs`) so meetrs is its \
             own subject.",
            path.display(),
            pid
        );
    }

    let opening = observe_opening(consumer, info);

    if opening.frames > 0 && !opening.mic_signal && !opening.system_signal {
        anyhow::bail!(
            "capture is running but every sample on both legs is bit-exact zero, so this \
             recording would contain nothing.\n\
             A real microphone never delivers digital silence: consent was denied, or the \
             mic is muted. Check System Settings > Privacy & Security > Microphone and \
             Screen & System Audio Recording, then run `just check` with audio playing."
        );
    }

    let warning = if opening.frames == 0 {
        Some(
            "no audio arrived in the first second. The tap only runs while some process \
             writes system audio, so this is expected before a meeting starts — but it \
             also looks exactly like a refused tap. `just check` with audio playing tells \
             them apart.",
        )
    } else if !opening.mic_signal {
        Some(
            "the microphone leg is bit-exact zero while system audio flows — the mic is \
             muted or its consent was denied. System audio will still be recorded.",
        )
    } else if !opening.system_signal {
        Some(
            "system audio is bit-exact zero while the mic flows. Expected if nothing is \
             playing yet; otherwise the tap was refused.",
        )
    } else {
        None
    };

    if let Some(warning) = warning {
        eprintln!("meetrs: {warning}");
        // The TUI clears the screen the instant it starts, so a warning printed
        // and immediately painted over is a warning nobody reads.
        std::thread::sleep(Duration::from_millis(2500));
    }
    Ok(())
}

/// `--check`: capture for a few seconds and report the negotiated layout plus
/// per-leg signal level, then exit. No TUI, so it runs without a tty.
///
/// This exists because Core Audio process taps have a documented failure mode
/// where the tap looks entirely healthy — correct timestamps, correct cadence —
/// and delivers pure silence. The only way to catch it is to look at the sample
/// values, and the TUI can't be driven from a script.
fn check() -> Result<()> {
    let (info, mut consumer) = capture::start(RING_SAMPLES)?;
    println!(
        "channels={} rate={} mic={:?} system={:?}",
        info.channels, info.sample_rate, info.mic_channels, info.system_channels
    );

    let ch = info.channels as usize;
    let (mut mic_sq, mut sys_sq, mut mic_peak, mut sys_peak, mut frames) =
        (0f64, 0f64, 0f32, 0f32, 0u64);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        while let Ok(chunk) = consumer.read_chunk(ch.min(consumer.slots())) {
            if chunk.len() < ch {
                break;
            }
            let (a, b) = chunk.as_slices();
            let frame: Vec<f32> = a.iter().chain(b.iter()).copied().collect();
            for (i, s) in frame.iter().enumerate().take(ch) {
                let i = i as u16;
                let v = s.abs();
                if i >= info.mic_channels.0 && i <= info.mic_channels.1 {
                    mic_sq += (s * s) as f64;
                    mic_peak = mic_peak.max(v);
                } else if i >= info.system_channels.0 && i <= info.system_channels.1 {
                    sys_sq += (s * s) as f64;
                    sys_peak = sys_peak.max(v);
                }
            }
            frames += 1;
            chunk.commit_all();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    capture::stop();

    if frames == 0 {
        // "Nothing arrived" has several very different causes that look the same
        // from here, so report which one it was instead of just the symptom.
        let io = capture::io_stats();
        let why = if io.calls == 0 {
            // Registration always passes a real serial dispatch queue and rejects
            // a null IOProc ID, so the macOS 26 nil-queue no-op is ruled out by
            // construction. Two causes remain, and they are not equally alarming:
            // the aggregate is tap-driven, so coreaudiod runs no cycle at all
            // until some process writes system audio — a silent machine yields
            // zero callbacks rather than zero-valued samples. The other is a TCC
            // denial of the tap (see src/tcc.rs), where the device still starts
            // and AudioDeviceStart still returns noErr. The coreaudiod log
            // distinguishes them outright, so hand over the exact strings.
            "the IOProc was never invoked. Either nothing was playing — the tap is only \
             driven while some process writes system audio, so retry with audio playing \
             — or the tap was denied. Tell those apart with:\n  \
             log show --last 2m --predicate 'process == \"coreaudiod\"' | grep -i tap\n  \
             \"Starting tap after waiting for writers\" = allowed; \
             \"Client is not granted access to the tap\" = denied"
        } else if io.shape_mismatch == io.calls {
            "every cycle was dropped because the device's buffer count disagreed with \
             the stream layout discovered at start()"
        } else if io.empty == io.calls {
            "the IOProc ran but every buffer was empty or null"
        } else {
            "the IOProc ran and produced buffers, but nothing reached the ring buffer"
        };
        anyhow::bail!(
            "no frames arrived at all — capture started but delivered nothing.\n\
             {why}.\n\
             ioproc calls={} shape-mismatch={} empty={} last mNumberBuffers={} (expected {}){}",
            io.calls,
            io.shape_mismatch,
            io.empty,
            io.last_n_buffers,
            io.expected_n_buffers,
            attribution_note(),
        );
    }
    let rms = |sq: f64, n: u64| (sq / n as f64).sqrt() as f32;
    println!(
        "frames={frames} dropped={}\nmic:    rms={:.6} peak={:.6}\nsystem: rms={:.6} peak={:.6}",
        capture::dropped_samples(),
        rms(mic_sq, frames),
        mic_peak,
        rms(sys_sq, frames),
        sys_peak
    );
    if sys_peak == 0.0 {
        // Three causes, cheapest first. Nothing playing is by far the most common
        // and is not a bug at all: the mic drives the cycles while the tap
        // contributes zero-filled buffers. A denied tap looks the same from here
        // (mic allowed, tap refused, because terminals carry
        // NSMicrophoneUsageDescription but not NSAudioCaptureUsageDescription),
        // and only once both are ruled out is this the Core Audio bug.
        println!(
            "\nWARNING: system leg is bit-exact zero across {frames} frames.\n\
             Most likely nothing was playing — retry with audio playing, since a silent \
             system tap contributes zero-filled buffers while the mic keeps the cycles \
             running.\n\
             If audio *was* playing, the tap was refused while the mic was allowed; if TCC \
             attribution is correct too, this is the process-tap zero-samples bug — see \
             docs/research/rust-audio-macos.md in git history.{}",
            attribution_note()
        );
    }
    Ok(())
}

/// Names the application TCC is actually asking about, when that is not meetrs.
/// Empty when we are our own subject, so it can be appended unconditionally.
fn attribution_note() -> String {
    match tcc::foreign_responsible_process() {
        Some((pid, path)) => format!(
            "\n\nTCC is attributing this process to {} (pid {}), not to meetrs, so that \
             application is the one being checked for permission — and it needs \
             NSAudioCaptureUsageDescription, which terminals do not ship. Run the \
             installed bundle (`just install`, then `meetrs`) so meetrs is its own subject.",
            path.display(),
            pid
        ),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Opening, summarize};
    use crate::types::CaptureInfo;

    /// 1 mic channel + 2 system channels, the layout this machine negotiates.
    fn info() -> CaptureInfo {
        CaptureInfo {
            channels: 3,
            sample_rate: 48_000,
            mic_channels: (0, 0),
            system_channels: (1, 2),
        }
    }

    #[test]
    fn digital_silence_shows_no_signal_on_either_leg() {
        let samples = [0.0f32; 3 * 10];
        assert_eq!(
            summarize(samples.iter(), info()),
            Opening {
                frames: 10,
                mic_signal: false,
                system_signal: false
            }
        );
    }

    #[test]
    fn a_live_mic_with_a_refused_tap_is_told_apart() {
        // channel 0 (mic) has signal, channels 1-2 (tap) are bit-exact zero:
        // exactly the shape a denied process tap produces.
        let samples: Vec<f32> = (0..10).flat_map(|_| [0.02, 0.0, 0.0]).collect();
        let opening = summarize(samples.iter(), info());
        assert_eq!(opening.frames, 10);
        assert!(opening.mic_signal);
        assert!(!opening.system_signal);
    }

    #[test]
    fn a_muted_mic_with_live_system_audio_is_told_apart() {
        let samples: Vec<f32> = (0..10).flat_map(|_| [0.0, -0.3, 0.4]).collect();
        let opening = summarize(samples.iter(), info());
        assert!(!opening.mic_signal);
        assert!(opening.system_signal);
    }

    #[test]
    fn a_partial_trailing_frame_is_not_counted() {
        // 3 whole frames plus one stray sample.
        let samples = [0.1f32; 3 * 3 + 1];
        assert_eq!(summarize(samples.iter(), info()).frames, 3);
    }

    /// The preflight reads the ring buffer and deliberately does not commit, so
    /// that the writer thread still receives the opening samples. That only holds
    /// because an uncommitted `ReadChunk` leaves the data in place — if rtrb ever
    /// changed that, the recording would silently lose its first second.
    #[test]
    fn reading_a_chunk_without_committing_consumes_nothing() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(16);
        for i in 0..9 {
            producer.push(i as f32).unwrap();
        }
        assert_eq!(consumer.slots(), 9);

        {
            let chunk = consumer.read_chunk(9).unwrap();
            let (a, b) = chunk.as_slices();
            assert_eq!(a.iter().chain(b.iter()).count(), 9);
            // dropped here, uncommitted
        }
        assert_eq!(consumer.slots(), 9, "uncommitted read must not consume");

        // And committing still works afterwards, so the writer is unaffected.
        let chunk = consumer.read_chunk(9).unwrap();
        chunk.commit_all();
        assert_eq!(consumer.slots(), 0);
    }
}
