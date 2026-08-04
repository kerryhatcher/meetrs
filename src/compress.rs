//! Post-session audio compression: WAV -> FLAC.
//!
//! Runs once, at the end of a session, after both worker threads have joined.
//! That placement is the whole design: at that point nothing else holds the
//! chunk files or `meta.json`, so there is no writer to race and no chunk that
//! has yet to be transcribed. Doing this per-chunk inside the transcribe thread
//! would mean two threads rewriting `meta.json`.
//!
//! FLAC via `afconvert(1)`, which ships with macOS — no new dependency, no
//! bundled encoder. It is the only royalty-free codec the built-in tooling can
//! actually write here: `afconvert` refuses Opus and Vorbis for our channel
//! counts (verified 1-4ch, all fail with 'fmt?'), while FLAC works for all of
//! them.
//!
//! The SQLite index is deliberately not updated: `chunks.file` is written but
//! never read back by any query, and `--reindex` recomputes it from the
//! `meta.json` this module rewrites. Adding an UPDATE here would buy nothing.
//!
//! Lossless with one documented exception: `afconvert` encodes our float32
//! source as 24-bit FLAC, so samples outside +/-1.0 clamp to full scale. Those
//! samples are already clipped as far as every consumer is concerned (Whisper,
//! playback, any int format), but a mic running that hot loses information here
//! that it would keep in the float WAV.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a session's compression pass did, for the closing summary.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Savings {
    pub files: usize,
    pub before: u64,
    pub after: u64,
}

impl Savings {
    /// Percent of the original bytes reclaimed. 0 when nothing was compressed.
    pub fn percent_saved(&self) -> f64 {
        if self.before == 0 {
            return 0.0;
        }
        (self.before - self.after.min(self.before)) as f64 / self.before as f64 * 100.0
    }
}

/// Compress every `chunk-NNN.wav` in `dir` to FLAC, delete the WAV only after
/// the FLAC round-trips to the same frame count, and repoint `meta.json` at the
/// new filenames.
///
/// Best-effort per file: one chunk that fails to encode keeps its WAV and does
/// not stop the others. The audio is the thing being protected here, so every
/// failure path leaves the original in place.
pub fn run(dir: &Path, warn: &mut dyn FnMut(String)) -> Result<Savings> {
    let mut savings = Savings::default();
    let mut renamed: Vec<(String, String)> = Vec::new();

    for wav in wav_chunks(dir)? {
        // `chunk-NNN.json` is written only on a successful transcription, so its
        // absence means this chunk still needs its float WAV. Load-bearing for
        // `--compress`, which sweeps sessions this process never recorded.
        if !wav.with_extension("json").exists() {
            warn(format!("{} not transcribed yet, left alone", name(&wav)));
            continue;
        }
        let flac = wav.with_extension("flac");
        let before = std::fs::metadata(&wav).map(|m| m.len()).unwrap_or(0);
        match compress_one(&wav, &flac) {
            Ok(after) => {
                if let Err(e) = std::fs::remove_file(&wav) {
                    // The FLAC is verified good, so the only cost of a failed
                    // delete is disk. Keep the FLAC and leave meta pointing at
                    // the WAV that is still there.
                    warn(format!("kept {} (could not remove): {e}", name(&wav)));
                    let _ = std::fs::remove_file(&flac);
                    continue;
                }
                savings.files += 1;
                savings.before += before;
                savings.after += after;
                renamed.push((name(&wav), name(&flac)));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&flac); // don't leave a partial file
                warn(format!("{} not compressed: {e:#}", name(&wav)));
            }
        }
    }

    if !renamed.is_empty() {
        // meta.json is the source of truth for what audio exists, so it has to
        // stop naming files that were just deleted.
        if let Err(e) = repoint_meta(dir, &renamed) {
            warn(format!("meta.json still names the .wav files: {e:#}"));
        }
    }

    Ok(savings)
}

/// Every session directory under `~/.meetrs/recordings`, oldest first. Returns
/// an empty list rather than an error when there are no recordings yet.
pub fn sessions() -> Result<Vec<PathBuf>> {
    let base = crate::types::recordings_dir()?;
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    // Names are timestamps, so lexical order is chronological.
    dirs.sort();
    Ok(dirs)
}

fn name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Every `chunk-NNN.wav` in the session dir, in index order.
fn wav_chunks(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let is_chunk_wav = path.extension().is_some_and(|e| e == "wav")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("chunk-"));
        if is_chunk_wav {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Encode one WAV to FLAC and verify it before the caller deletes the source.
/// Returns the FLAC's size on success.
fn compress_one(wav: &Path, flac: &Path) -> Result<u64> {
    let expected = wav_frames(wav).context("reading source frame count")?;

    // `-f flac` selects the FLAC file type; `-d flac` its only data format.
    // Passing a PCM data format here fails with 'fmt?'.
    let out = Command::new("/usr/bin/afconvert")
        .arg("-f")
        .arg("flac")
        .arg("-d")
        .arg("flac")
        .arg(wav)
        .arg(flac)
        .output()
        .context("running afconvert")?;
    if !out.status.success() {
        anyhow::bail!(
            "afconvert failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    // Verify by decoding back and counting frames rather than trusting the exit
    // status — this is the check that gates deleting the only copy of the audio.
    // Costs one temp file the size of the original, for a few milliseconds.
    // Deliberately outside the session dir: anything named `chunk-*.wav` in
    // there is a compression candidate, so a probe file left by a crash would
    // come back as input on the next run.
    let probe = std::env::temp_dir().join(format!(
        "meetrs-verify-{}-{}.wav",
        std::process::id(),
        flac.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let decoded = decode_frames(flac, &probe);
    let _ = std::fs::remove_file(&probe);
    let decoded = decoded.context("decoding the FLAC back to verify it")?;
    anyhow::ensure!(
        decoded == expected,
        "FLAC has {decoded} frames, source had {expected} — refusing to delete the WAV"
    );

    Ok(std::fs::metadata(flac)?.len())
}

fn decode_frames(flac: &Path, probe: &Path) -> Result<u64> {
    let out = Command::new("/usr/bin/afconvert")
        .arg("-f")
        .arg("WAVE")
        .arg("-d")
        .arg("LEF32")
        .arg(flac)
        .arg(probe)
        .output()
        .context("running afconvert to decode")?;
    if !out.status.success() {
        anyhow::bail!(
            "afconvert decode failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    wav_frames(probe)
}

fn wav_frames(path: &Path) -> Result<u64> {
    let reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(reader.duration() as u64)
}

/// Rewrite `meta.json`'s `chunks[].file` values for the files we renamed.
///
/// Parsed as an untyped `Value` on purpose: this only needs to touch one field
/// per chunk, and going through a typed mirror of the writer's `Meta` struct
/// would silently drop any field that struct doesn't declare.
fn repoint_meta(dir: &Path, renamed: &[(String, String)]) -> Result<()> {
    let path = dir.join("meta.json");
    let raw = std::fs::read_to_string(&path).context("reading meta.json")?;
    let mut meta: serde_json::Value = serde_json::from_str(&raw).context("parsing meta.json")?;

    let Some(chunks) = meta.get_mut("chunks").and_then(|c| c.as_array_mut()) else {
        anyhow::bail!("meta.json has no chunks array");
    };
    for chunk in chunks {
        let Some(file) = chunk.get("file").and_then(|f| f.as_str()) else {
            continue;
        };
        if let Some((_, to)) = renamed.iter().find(|(from, _)| from == file) {
            chunk["file"] = serde_json::Value::String(to.clone());
        }
    }

    let bytes = serde_json::to_vec_pretty(&meta).context("serializing meta.json")?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes).context("writing meta.json.tmp")?;
    std::fs::rename(&tmp, &path).context("replacing meta.json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meetrs-compress-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A 3-channel float32 WAV, same shape as a real chunk.
    fn write_wav(path: &Path, frames: u32) {
        let spec = hound::WavSpec {
            channels: 3,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            let v = (i as f32 * 0.01).sin() * 0.5;
            w.write_sample(v).unwrap();
            w.write_sample(-v).unwrap();
            w.write_sample(0.0).unwrap();
        }
        w.finalize().unwrap();
    }

    /// A transcribed chunk: the WAV plus the `chunk-NNN.json` that proves ASR
    /// ran, which is what `run` requires before it will touch the audio.
    fn write_transcribed_chunk(dir: &Path, index: u32, frames: u32) {
        write_wav(&dir.join(format!("chunk-{index:03}.wav")), frames);
        std::fs::write(
            dir.join(format!("chunk-{index:03}.json")),
            format!(r#"{{"index":{index},"segments":[]}}"#),
        )
        .unwrap();
    }

    #[test]
    fn compresses_chunks_repoints_meta_and_removes_the_wavs() {
        let dir = scratch("happy");
        write_transcribed_chunk(&dir, 0, 24_000);
        write_transcribed_chunk(&dir, 1, 24_000);
        // A non-chunk wav must be left alone.
        write_wav(&dir.join("scratch.wav"), 480);
        std::fs::write(
            dir.join("meta.json"),
            r#"{"started":"x","channels":3,"chunks":[
                 {"index":0,"file":"chunk-000.wav","duration_secs":0.5},
                 {"index":1,"file":"chunk-001.wav","duration_secs":0.5}]}"#,
        )
        .unwrap();

        let mut warnings = Vec::new();
        let savings = run(&dir, &mut |w| warnings.push(w)).unwrap();

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(savings.files, 2);
        assert!(savings.after < savings.before, "{savings:?}");

        assert!(dir.join("chunk-000.flac").exists());
        assert!(dir.join("chunk-001.flac").exists());
        assert!(!dir.join("chunk-000.wav").exists());
        assert!(!dir.join("chunk-001.wav").exists());
        // Untouched: not a chunk.
        assert!(dir.join("scratch.wav").exists());
        // No verify temp files left behind.
        assert!(!dir.join("chunk-000.verify.wav").exists());

        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        let files: Vec<&str> = meta["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["file"].as_str().unwrap())
            .collect();
        assert_eq!(files, vec!["chunk-000.flac", "chunk-001.flac"]);
        // Fields we don't touch survive the rewrite.
        assert_eq!(meta["started"], "x");
        assert_eq!(meta["chunks"][0]["duration_secs"], 0.5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_untranscribed_chunk_keeps_its_wav() {
        let dir = scratch("untranscribed");
        // WAV with no chunk-000.json: transcription never succeeded for it, so
        // the float audio has to survive a --compress sweep.
        write_wav(&dir.join("chunk-000.wav"), 4_800);
        write_transcribed_chunk(&dir, 1, 4_800);
        std::fs::write(
            dir.join("meta.json"),
            r#"{"chunks":[{"index":0,"file":"chunk-000.wav"},
                          {"index":1,"file":"chunk-001.wav"}]}"#,
        )
        .unwrap();

        let mut warnings = Vec::new();
        let savings = run(&dir, &mut |w| warnings.push(w)).unwrap();

        assert_eq!(savings.files, 1, "only the transcribed chunk");
        assert!(dir.join("chunk-000.wav").exists());
        assert!(!dir.join("chunk-000.flac").exists());
        assert!(!dir.join("chunk-001.wav").exists());
        assert!(dir.join("chunk-001.flac").exists());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("not transcribed"), "{warnings:?}");

        // Only the compressed chunk is repointed; the skipped one still names
        // the WAV that is still there.
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["chunks"][0]["file"], "chunk-000.wav");
        assert_eq!(meta["chunks"][1]["file"], "chunk-001.flac");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unencodable_file_keeps_its_wav_and_warns() {
        let dir = scratch("bad");
        // Not audio at all: afconvert must fail and the file must survive.
        std::fs::write(dir.join("chunk-000.wav"), b"definitely not a wav").unwrap();
        std::fs::write(dir.join("chunk-000.json"), b"{}").unwrap();

        let mut warnings = Vec::new();
        let savings = run(&dir, &mut |w| warnings.push(w)).unwrap();

        assert_eq!(savings, Savings::default());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(dir.join("chunk-000.wav").exists());
        assert!(!dir.join("chunk-000.flac").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_to_do_is_not_an_error() {
        let dir = scratch("empty");
        let mut warnings = Vec::new();
        let savings = run(&dir, &mut |w| warnings.push(w)).unwrap();
        assert_eq!(savings, Savings::default());
        assert_eq!(savings.percent_saved(), 0.0);
        assert!(warnings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
