//! meetrs — terminal meeting recorder.
//!
//! Captures microphone + system audio into one synchronized stream, writes WAV
//! chunks split on natural pauses so a crash costs at most one chunk.
//! Recording only; transcription is out of scope.

mod capture;
mod chunk;
mod lock;
mod model;
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
    // Held for the whole process. Two instances would fight over the same audio
    // device and interleave writes into the same recordings tree, so this covers
    // `--check` too — it opens capture as well.
    let _lock = lock::acquire()?;

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

    let (info, consumer) = capture::start(RING_SAMPLES).context(
        "starting audio capture — if this is a permissions failure, see the README \
         section on TCC consent and codesigning",
    )?;

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
    let write_result = writer.join().map_err(|_| anyhow::anyhow!("writer thread panicked"))?;

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
    match asr.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("transcription ended early: {e:#}"),
        Err(_) => eprintln!("transcription thread panicked; audio is still intact"),
    }
    println!("{}", dir.display());
    Ok(())
}

/// Small helper the UI uses to bound its redraw rate.
pub const FRAME: Duration = Duration::from_millis(50);

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
    let (mut mic_sq, mut sys_sq, mut mic_peak, mut sys_peak, mut frames) = (0f64, 0f64, 0f32, 0f32, 0u64);

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
        anyhow::bail!("no frames arrived at all — capture started but delivered nothing");
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
        println!(
            "\nWARNING: system leg is bit-exact zero across {frames} frames.\n\
             That is the signature of the process-tap zero-samples bug, not quiet audio.\n\
             See docs/research/rust-audio-macos.md"
        );
    }
    Ok(())
}
