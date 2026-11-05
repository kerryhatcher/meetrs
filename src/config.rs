//! User configuration at `~/.meetrs/config.yaml`.
//!
//! Created with commented-out defaults on first run and never overwritten after
//! that, so a user's edits survive upgrades. Every key is optional: an unset key
//! falls back to the compiled-in default in `types`. Environment variables still
//! win over the file, so `MEETRS_SILENCE_RMS=0.03 meetrs` works without editing
//! anything.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// RMS (linear, not dB) below which a block counts as silence, used only by
    /// the RMS fallback that runs when the VAD fails to initialize. Normal
    /// chunking is driven by the VAD and ignores this. See
    /// `types::DEFAULT_SILENCE_RMS`.
    pub silence_rms: Option<f32>,
    /// Seconds of continuous speech before a chunk is closed. Default 150.
    /// See `types::max_chunk`.
    pub max_chunk_secs: Option<f64>,
    /// The largest a chunk's WAV may grow, in bytes, and the largest file the
    /// uploader sends. Default 99,000,000. See `types::max_chunk_bytes`.
    pub max_chunk_bytes: Option<u64>,
    #[serde(default)]
    pub upload: Upload,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    #[serde(default)]
    pub r2: R2Upload,
}

/// Upload finished sessions to a Cloudflare R2 bucket. Credentials are never
/// read from this file; see `r2::Credentials::from_env`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct R2Upload {
    /// Off unless set. Nothing else in this section is checked when it is off.
    #[serde(default)]
    pub enabled: bool,
    pub account_id: Option<String>,
    pub bucket: Option<String>,
    /// Key prefix inside the bucket, e.g. `meetings/`. Empty means the bucket root.
    #[serde(default)]
    pub prefix: String,
    /// Seconds between uploads of whatever changed while recording. A crash
    /// loses at most this much. Must be at least 30.
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
}

fn default_interval_secs() -> u64 {
    300
}

/// Smallest `max_chunk_secs` allowed. Shorter chunks would cut speech mid-word.
const MIN_MAX_CHUNK_SECS: f64 = 10.0;
/// Largest `max_chunk_secs` allowed: a day. Larger values overflow the Duration
/// conversion, and no recording needs a single chunk that long.
const MAX_MAX_CHUNK_SECS: f64 = 86_400.0;
/// Smallest `max_chunk_bytes` allowed. It has to hold more than the one second
/// of frames the chunker keeps back, so the limit is never reached by silence alone.
const MIN_MAX_CHUNK_BYTES: u64 = 10_000_000;

/// The chunk-length key, as written to a new file or added to an existing one.
const MAX_CHUNK_SECS_BLOCK: &str = "\
# How long a recording chunk may run, in seconds of continuous speech, before it
# is closed. A chunk also closes after 2 seconds of silence, so this only limits
# unbroken speech. Minimum 10.
max_chunk_secs: 150
";

/// The chunk-size key, as written to a new file or added to an existing one.
const MAX_CHUNK_BYTES_BLOCK: &str = "\
# The largest a chunk's WAV may grow, in bytes. A chunk is closed before it would
# pass this. Uploads also refuse any file over it. 99 MB keeps files under the
# planned backend's limit. Minimum 10000000.
max_chunk_bytes: 99000000
";

/// What gets written on first run. Everything is commented out, so the file
/// parses to `Config::default()` and the compiled-in values stay in effect until
/// the user opts in to a key.
const TEMPLATE: &str = "\
# meetrs configuration.
#
# Created on first run. Edit freely; meetrs reads it at startup and never
# rewrites it. Environment variables such as MEETRS_SILENCE_RMS override these
# values. A commented-out key falls back to its built-in default. The chunk limits
# below are written in full, so their defaults are visible.

# How long a recording chunk may run, in seconds of continuous speech, before it
# is closed. A chunk also closes after 2 seconds of silence, so this only limits
# unbroken speech. Minimum 10.
max_chunk_secs: 150

# The largest a chunk's WAV may grow, in bytes. A chunk is closed before it would
# pass this. Uploads also refuse any file over it. 99 MB keeps files under the
# planned backend's limit. Minimum 10000000.
max_chunk_bytes: 99000000

# Below this RMS (linear, not dB) a block counts as silence. This only applies
# when the voice-activity detector fails to load and meetrs falls back to a
# plain volume threshold. Normal operation ignores it.
# silence_rms: 0.02

# Upload recordings to a Cloudflare R2 bucket. Every interval_secs, whatever
# changed is sent, and a final pass runs when recording stops. Keys look like
# <prefix><session>/<file>. The access key goes in the environment, never in
# this file:
#   MEETRS_R2_ACCESS_KEY_ID, MEETRS_R2_SECRET_ACCESS_KEY
# upload:
#   r2:
#     enabled: false
#     account_id: <your Cloudflare account id>
#     bucket: <bucket name>
#     prefix: ''
#     interval_secs: 300
";

static CONFIG: OnceLock<Config> = OnceLock::new();

/// `~/.meetrs/config.yaml`.
pub fn path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set; cannot locate ~/.meetrs")?;
    Ok(PathBuf::from(home).join(".meetrs").join("config.yaml"))
}

/// Load the config for this process, creating the file first if it is missing.
/// Call once at startup, before anything reads a setting. A malformed file is an
/// error rather than a silent fallback: the user wrote it and should hear about a
/// typo before a recording runs on the wrong threshold.
pub fn init() -> Result<()> {
    let cfg = load_or_create(&path()?)?;
    // A second call is a no-op; the first load wins for the process lifetime.
    let _ = CONFIG.set(cfg);
    Ok(())
}

/// The loaded config, or defaults if `init` was never called (for example in
/// tests, or for a subcommand that runs before startup).
pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

/// Read `path`, writing the template there first if it does not exist.
fn load_or_create(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let cfg = parse(&text, path)?;
            add_missing_keys(path, &text, cfg)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            create_template(path)?;
            Ok(Config::default())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Add the chunk-limit keys the file does not define, at their defaults, so the
/// user can see the options and what they are set to. Only these keys are ever
/// appended, and only at the end of the file. The new text is parsed before
/// anything is written, so a file that would not load is left untouched.
fn add_missing_keys(path: &Path, text: &str, cfg: Config) -> Result<Config> {
    // Looked up in the text, not the parsed value: a key that is present but empty
    // (`max_chunk_secs:`) parses to None, and appending it again would make the
    // file fail to load.
    let mut add = String::new();
    if !defines_key(text, "max_chunk_secs") {
        add.push_str(MAX_CHUNK_SECS_BLOCK);
    }
    if !defines_key(text, "max_chunk_bytes") {
        if !add.is_empty() {
            add.push('\n');
        }
        add.push_str(MAX_CHUNK_BYTES_BLOCK);
    }
    if add.is_empty() {
        return Ok(cfg);
    }

    let gap = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let combined = format!("{text}{gap}\n{add}");
    // A file that is not a plain mapping (a flow mapping, say) cannot take appended
    // lines. Leave it alone and run on the defaults, rather than refuse to start.
    let Ok(updated) = parse(&combined, path) else {
        eprintln!(
            "meetrs: warning: {} is not a plain YAML mapping, so chunk settings were not added",
            path.display()
        );
        return Ok(cfg);
    };

    let written = OpenOptions::new()
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(format!("{gap}\n{add}").as_bytes()));
    match written {
        Ok(()) => Ok(updated),
        // Read-only or in use: meetrs still runs, with the same values in effect.
        Err(e) => {
            eprintln!(
                "meetrs: warning: could not add chunk settings to {}: {e}",
                path.display()
            );
            Ok(updated)
        }
    }
}

/// Write the template with `create_new`, so an existing file (including one that
/// appeared between the read and here) is never clobbered.
fn create_template(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file
            .write_all(TEMPLATE.as_bytes())
            .with_context(|| format!("writing {}", path.display())),
        // Someone else created it since we looked. Their file wins.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

/// Whether the document has a top-level key `key`, whatever its value.
fn defines_key(text: &str, key: &str) -> bool {
    serde_yaml_ng::from_str::<Option<serde_yaml_ng::Mapping>>(text)
        .ok()
        .flatten()
        .is_some_and(|m| m.keys().any(|k| k.as_str() == Some(key)))
}

fn parse(text: &str, path: &Path) -> Result<Config> {
    // `Option` so a file with only comments (an empty YAML document) is valid and
    // means "all defaults", rather than an error about a missing mapping.
    let cfg = serde_yaml_ng::from_str::<Option<Config>>(text)
        .with_context(|| format!("parsing {}", path.display()))?
        .unwrap_or_default();
    if let Some(secs) = cfg.max_chunk_secs {
        anyhow::ensure!(
            secs.is_finite() && (MIN_MAX_CHUNK_SECS..=MAX_MAX_CHUNK_SECS).contains(&secs),
            "{}: max_chunk_secs must be between {MIN_MAX_CHUNK_SECS} and {MAX_MAX_CHUNK_SECS}, got {secs}",
            path.display()
        );
    }
    if let Some(bytes) = cfg.max_chunk_bytes {
        anyhow::ensure!(
            bytes >= MIN_MAX_CHUNK_BYTES,
            "{}: max_chunk_bytes must be at least {MIN_MAX_CHUNK_BYTES}, got {bytes}",
            path.display()
        );
    }
    if let Some(rms) = cfg.silence_rms {
        anyhow::ensure!(
            rms > 0.0 && rms.is_finite(),
            "{}: silence_rms must be a positive number, got {rms}",
            path.display()
        );
    }
    let r2 = &cfg.upload.r2;
    if r2.enabled {
        // Only checked when enabled, so a half-filled section costs nothing
        // until the user turns it on.
        let account = r2.account_id.as_deref().unwrap_or("").trim();
        anyhow::ensure!(
            !account.is_empty(),
            "{}: upload.r2.account_id is required when upload.r2 is enabled",
            path.display()
        );
        anyhow::ensure!(
            account.len() == 32 && account.bytes().all(|b| b.is_ascii_hexdigit()),
            "{}: upload.r2.account_id must be the 32-character hex id from the Cloudflare dashboard",
            path.display()
        );
        let bucket = r2.bucket.as_deref().unwrap_or("").trim();
        anyhow::ensure!(
            !bucket.is_empty(),
            "{}: upload.r2.bucket is required when upload.r2 is enabled",
            path.display()
        );
        crate::r2::check_bucket_name(bucket)
            .with_context(|| format!("{}: upload.r2.bucket", path.display()))?;
        anyhow::ensure!(
            r2.interval_secs >= 30,
            "{}: upload.r2.interval_secs must be at least 30, got {}",
            path.display(),
            r2.interval_secs
        );
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meetrs-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.yaml")
    }

    #[test]
    fn missing_file_is_created_with_template_and_yields_defaults() {
        let path = scratch("create");
        let cfg = load_or_create(&path).expect("load");
        assert!(cfg.silence_rms.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEMPLATE);
    }

    #[test]
    fn existing_file_keeps_its_own_text_and_gains_only_the_chunk_keys() {
        let path = scratch("keep");
        std::fs::write(&path, "silence_rms: 0.05\n").unwrap();
        let cfg = load_or_create(&path).expect("load");
        assert_eq!(cfg.silence_rms, Some(0.05));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("silence_rms: 0.05\n"), "{text}");
        assert!(text.contains(MAX_CHUNK_SECS_BLOCK));
        assert!(text.contains(MAX_CHUNK_BYTES_BLOCK));
    }

    #[test]
    fn missing_chunk_keys_are_added_with_their_defaults() {
        let path = scratch("add");
        std::fs::write(&path, "silence_rms: 0.03").unwrap(); // no trailing newline
        let cfg = load_or_create(&path).expect("load");
        assert_eq!(cfg.max_chunk_secs, Some(150.0));
        assert_eq!(cfg.max_chunk_bytes, Some(99_000_000));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("max_chunk_secs: 150"), "{text}");
        assert!(text.contains("max_chunk_bytes: 99000000"), "{text}");
        // Second load: nothing left to add, so the file does not change.
        let before = text.clone();
        load_or_create(&path).expect("reload");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_defined_chunk_key_is_left_alone() {
        let path = scratch("defined");
        let text = "max_chunk_secs: 60\nmax_chunk_bytes: 50000000\n";
        std::fs::write(&path, text).unwrap();
        let cfg = load_or_create(&path).expect("load");
        assert_eq!(cfg.max_chunk_secs, Some(60.0));
        assert_eq!(cfg.max_chunk_bytes, Some(50_000_000));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn template_carries_both_chunk_blocks_verbatim() {
        assert!(TEMPLATE.contains(MAX_CHUNK_SECS_BLOCK));
        assert!(TEMPLATE.contains(MAX_CHUNK_BYTES_BLOCK));
    }

    #[test]
    fn chunk_limits_below_the_minimum_are_rejected() {
        assert!(parse("max_chunk_secs: 5\n", Path::new("x")).is_err());
        assert!(parse("max_chunk_secs: .inf\n", Path::new("x")).is_err());
        assert!(parse("max_chunk_bytes: 1000\n", Path::new("x")).is_err());
        assert!(
            parse(
                "max_chunk_secs: 10\nmax_chunk_bytes: 10000000\n",
                Path::new("x")
            )
            .is_ok()
        );
    }

    #[test]
    fn template_itself_parses_to_defaults() {
        let cfg = parse(TEMPLATE, Path::new("template")).expect("template must parse");
        assert!(cfg.silence_rms.is_none());
    }

    #[test]
    fn empty_file_means_defaults() {
        let cfg = parse("", Path::new("empty")).expect("empty must parse");
        assert!(cfg.silence_rms.is_none());
    }

    #[test]
    fn silence_rms_is_read() {
        let cfg = parse("silence_rms: 0.035\n", Path::new("x")).unwrap();
        assert_eq!(cfg.silence_rms, Some(0.035));
    }

    #[test]
    fn unknown_key_is_an_error_so_typos_surface() {
        let err = parse("silence_rmss: 0.03\n", Path::new("x")).unwrap_err();
        assert!(format!("{err:#}").contains("silence_rmss"), "{err:#}");
    }

    #[test]
    fn non_positive_or_non_finite_threshold_is_rejected() {
        for bad in ["0", "-0.1", ".nan", ".inf"] {
            let text = format!("silence_rms: {bad}\n");
            assert!(
                parse(&text, Path::new("x")).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn upload_is_off_and_unchecked_by_default() {
        let cfg = parse("upload:\n  r2:\n    bucket: NOT VALID\n", Path::new("x")).unwrap();
        assert!(!cfg.upload.r2.enabled);
    }

    #[test]
    fn enabled_upload_needs_account_and_bucket() {
        let no_account = "upload:\n  r2:\n    enabled: true\n    bucket: hatcher-meetrs\n";
        assert!(parse(no_account, Path::new("x")).is_err());
        let no_bucket =
            "upload:\n  r2:\n    enabled: true\n    account_id: 0123456789abcdef0123456789abcdef\n";
        assert!(parse(no_bucket, Path::new("x")).is_err());
    }

    #[test]
    fn enabled_upload_with_full_settings_parses() {
        let text = "upload:\n  r2:\n    enabled: true\n    account_id: 0123456789abcdef0123456789abcdef\n    bucket: hatcher-meetrs\n    prefix: meetings/\n";
        let cfg = parse(text, Path::new("x")).unwrap();
        assert!(cfg.upload.r2.enabled);
        assert_eq!(cfg.upload.r2.bucket.as_deref(), Some("hatcher-meetrs"));
        assert_eq!(cfg.upload.r2.prefix, "meetings/");
    }

    #[test]
    fn enabled_upload_rejects_a_bad_bucket_name() {
        let text = "upload:\n  r2:\n    enabled: true\n    account_id: 0123456789abcdef0123456789abcdef\n    bucket: Bad_Name\n";
        assert!(parse(text, Path::new("x")).is_err());
    }

    #[test]
    fn upload_interval_defaults_to_five_minutes_and_has_a_floor() {
        let on = "upload:\n  r2:\n    enabled: true\n    account_id: 0123456789abcdef0123456789abcdef\n    bucket: hatcher-meetrs\n";
        assert_eq!(
            parse(on, Path::new("x")).unwrap().upload.r2.interval_secs,
            300
        );
        let too_fast = format!("{on}    interval_secs: 5\n");
        assert!(parse(&too_fast, Path::new("x")).is_err());
    }

    #[test]
    fn an_empty_chunk_key_is_not_appended_a_second_time() {
        // `max_chunk_secs:` with no value is present, so it must not be added again.
        let path = scratch("empty-key");
        std::fs::write(&path, "max_chunk_secs:\nmax_chunk_bytes: 50000000\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        load_or_create(&path).expect("an empty key must not break the file");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_flow_mapping_file_still_starts_and_is_left_alone() {
        let path = scratch("flow");
        let text = "{silence_rms: 0.03}\n";
        std::fs::write(&path, text).unwrap();
        let cfg = load_or_create(&path).expect("must still start");
        assert_eq!(cfg.silence_rms, Some(0.03));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn chunk_secs_above_a_day_is_rejected() {
        assert!(parse("max_chunk_secs: 1e300\n", Path::new("x")).is_err());
        assert!(parse("max_chunk_secs: 86400\n", Path::new("x")).is_ok());
    }

    #[test]
    fn account_id_must_be_32_hex_digits() {
        let bad = "upload:\n  r2:\n    enabled: true\n    account_id: abc/def\n    bucket: hatcher-meetrs\n";
        assert!(parse(bad, Path::new("x")).is_err());
    }

    #[test]
    fn malformed_yaml_is_an_error() {
        assert!(parse("silence_rms: [0.1\n", Path::new("x")).is_err());
    }
}
