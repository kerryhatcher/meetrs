//! First-run download and local cache of the Whisper GGML model.
//!
//! Steady state is fully offline: once the file is on disk at
//! `~/.meetrs/models/` and its checksum matches, [`ensure`] returns without
//! touching the network. `MEETRS_MODEL` is the escape hatch for anyone who
//! wants no network call at all, or a bigger model than the default.

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::types::models_dir;

/// Set this to an absolute path to a model file you already have, and `ensure`
/// will use it verbatim and never hit the network.
pub const MODEL_ENV: &str = "MEETRS_MODEL";

const FILENAME: &str = "ggml-base.en-q5_1.bin";
const URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en-q5_1.bin";
/// Verified 2026-08-03: matches both the git-lfs `oid` HuggingFace's API
/// reports for this file and the sha256sum of a full downloaded copy.
const SHA256: &str = "4baf70dd0d7c4247ba2b81fafd9c01005ac77c2f9ef064e00dcf195d0e2fdd2f";

/// Path to a ready-to-use Whisper model, downloading it on first run.
/// `progress` is called with (bytes_so_far, total_bytes_or_0) during download.
pub fn ensure(mut progress: impl FnMut(u64, u64)) -> Result<PathBuf> {
    if let Some(over) = env::var_os(MODEL_ENV) {
        let path = PathBuf::from(over);
        if !path.is_file() {
            anyhow::bail!(
                "{MODEL_ENV} is set to {} but that file does not exist",
                path.display()
            );
        }
        return Ok(path);
    }

    let dir = models_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating model cache dir {}", dir.display()))?;
    let dest = dir.join(FILENAME);

    if dest.is_file() {
        if verify(&dest, SHA256)? {
            return Ok(dest);
        }
        eprintln!(
            "meetrs: cached model {} failed checksum, re-downloading",
            dest.display()
        );
        fs::remove_file(&dest).ok();
    }

    let tmp = dir.join(format!("{FILENAME}.part"));
    if let Err(e) = download(URL, &tmp, &mut progress) {
        fs::remove_file(&tmp).ok();
        return Err(anyhow!(
            "{e:#}\n\nCouldn't download the Whisper model. Set {MODEL_ENV} to the path of a \
             model file you've downloaded yourself, or fetch one manually from {URL}"
        ));
    }

    if !verify(&tmp, SHA256)? {
        fs::remove_file(&tmp).ok();
        anyhow::bail!(
            "downloaded model failed checksum verification; try again, or set {MODEL_ENV} \
             to a manually-downloaded copy from {URL}"
        );
    }

    fs::rename(&tmp, &dest).with_context(|| format!("installing model at {}", dest.display()))?;
    Ok(dest)
}

/// Streams `url` into `dest_tmp`, calling `progress` at a coarse cadence
/// (roughly every 250ms, and only once the percentage has actually moved).
fn download(url: &str, dest_tmp: &Path, progress: &mut impl FnMut(u64, u64)) -> Result<()> {
    // ureq 3 defaults its TLS *provider* to Rustls even when only the
    // `native-tls` feature is enabled, and then panics at request time. Select
    // the provider explicitly so TLS goes through Apple's Security.framework
    // rather than a vendored crypto stack.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let total = resp.body().content_length().unwrap_or(0);

    let mut file =
        File::create(dest_tmp).with_context(|| format!("creating {}", dest_tmp.display()))?;
    let mut reader = resp.body_mut().as_reader();

    let mut buf = [0u8; 64 * 1024];
    let mut downloaded = 0u64;
    let mut last_report = Instant::now();
    let mut last_pct = 0u64;
    loop {
        let n = reader.read(&mut buf).context("reading response body")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("writing model to disk")?;
        downloaded += n as u64;

        let pct = downloaded
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(0);
        if last_report.elapsed() >= Duration::from_millis(250) && (total == 0 || pct > last_pct) {
            progress(downloaded, total);
            last_report = Instant::now();
            last_pct = pct;
        }
    }
    progress(downloaded, total);
    Ok(())
}

/// True if `path`'s sha256 matches `expected_hex`.
fn verify(path: &Path, expected_hex: &str) -> Result<bool> {
    Ok(sha256_hex(path)? == expected_hex)
}

fn sha256_hex(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_matches_known_content() {
        let dir = std::env::temp_dir().join(format!("meetrs-model-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.bin");
        fs::write(&path, b"hello").unwrap();

        // sha256("hello")
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify(&path, expected).unwrap());
        assert!(!verify(&path, "not-a-real-hash").unwrap());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_env_override_is_used_verbatim() {
        let dir = std::env::temp_dir().join(format!("meetrs-model-env-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("custom.bin");
        fs::write(&path, b"not a real model").unwrap();

        // ponytail: std::env::set_var is unsafe (edition 2024) because it
        // races other threads reading the environment; fine here since tests
        // don't touch MEETRS_MODEL concurrently.
        unsafe { env::set_var(MODEL_ENV, &path) };
        let got = ensure(|_, _| {});
        unsafe { env::remove_var(MODEL_ENV) };

        assert_eq!(got.unwrap(), path);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_env_override_missing_file_errors() {
        let path = std::env::temp_dir().join("meetrs-model-does-not-exist.bin");
        unsafe { env::set_var(MODEL_ENV, &path) };
        let result = ensure(|_, _| {});
        unsafe { env::remove_var(MODEL_ENV) };
        assert!(result.is_err());
    }

    #[test]
    fn atomic_rename_moves_content_and_removes_source() {
        let dir = std::env::temp_dir().join(format!("meetrs-model-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("x.part");
        let dest = dir.join("x.bin");
        fs::write(&tmp, b"payload").unwrap();

        fs::rename(&tmp, &dest).unwrap();

        assert!(!tmp.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
        fs::remove_dir_all(&dir).ok();
    }
}
