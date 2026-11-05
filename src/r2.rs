//! Uploads finished session directories to a Cloudflare R2 bucket.
//!
//! R2 speaks the S3 API, so each file is a `PUT` signed with AWS Signature
//! Version 4. The signer is written out here rather than pulled in from an AWS
//! SDK: it is one request shape, and the dependencies it would bring are far
//! larger than the code. HMAC is built from `sha2`, which is already here.
//!
//! Credentials come from the environment and never from `config.yaml`, so a
//! secret is not sitting in plaintext in the file a user might paste into a bug
//! report.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// Signing region. R2 ignores the value but SigV4 requires one, and `auto` is
/// what Cloudflare's documentation uses.
const REGION: &str = "auto";

/// A single PUT of a whole chunk. Generous, because a slow uplink on a long
/// session's final file should not be cut off mid-transfer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub const ACCESS_KEY_ENV: &str = "MEETRS_R2_ACCESS_KEY_ID";
pub const SECRET_KEY_ENV: &str = "MEETRS_R2_SECRET_ACCESS_KEY";
pub const SESSION_TOKEN_ENV: &str = "MEETRS_R2_SESSION_TOKEN";

#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Only for temporary credentials. Sent as `x-amz-security-token`.
    pub session_token: Option<String>,
}

// Hand-written so a `{:?}` of the struct, or of anything holding it, cannot
// leak the secret into a log or a panic message.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("session_token", &self.session_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Credentials {
    pub fn from_env() -> Result<Self> {
        let access_key_id = non_empty_env(ACCESS_KEY_ENV)
            .with_context(|| format!("{ACCESS_KEY_ENV} is not set"))?;
        let secret_access_key = non_empty_env(SECRET_KEY_ENV)
            .with_context(|| format!("{SECRET_KEY_ENV} is not set"))?;
        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token: non_empty_env(SESSION_TOKEN_ENV),
        })
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Where uploads go: `<bucket>/<prefix><session>/<file>` on the account's R2
/// endpoint.
#[derive(Debug, Clone)]
pub struct Target {
    pub account_id: String,
    pub bucket: String,
    /// Either empty or ending in `/`. Normalized by `Target::new`.
    pub prefix: String,
}

impl Target {
    pub fn new(account_id: &str, bucket: &str, prefix: &str) -> Self {
        let prefix = prefix.trim_matches('/');
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        Self {
            account_id: account_id.trim().to_string(),
            bucket: bucket.trim().to_string(),
            prefix,
        }
    }

    pub fn host(&self) -> String {
        format!("{}.r2.cloudflarestorage.com", self.account_id)
    }

    /// The canonical URI for one object, path-style: `/<bucket>/<key>`.
    fn object_path(&self, key: &str) -> String {
        format!("/{}/{}", self.bucket, key)
    }
}

/// Everything one upload run needs, built once at startup so a missing
/// credential is reported before the recording starts, not after it ends.
pub struct Uploader {
    pub target: Target,
    creds: Credentials,
    agent: ureq::Agent,
}

/// What a file looked like when it was last sent. A file whose stamp has not
/// changed since is not sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    len: u64,
    modified: SystemTime,
}

/// The files already sent, keyed by path. Passed between passes so a periodic
/// pass and the final pass do not resend what is unchanged.
pub type Sent = HashMap<PathBuf, Stamp>;

impl Uploader {
    pub fn new(target: Target, creds: Credentials) -> Self {
        // Same reason as model.rs: ureq 3 defaults to Rustls even when only
        // native-tls is compiled in, and panics at request time.
        let agent = ureq::Agent::config_builder()
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    .build(),
            )
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .into();
        Self {
            target,
            creds,
            agent,
        }
    }

    /// Send every file in `sessions` that is new or has changed since `sent`
    /// last saw it. `active` is the session still being recorded: its newest
    /// WAV is skipped because that chunk is still being written. Returns the
    /// errors. A failed file is left out of `sent`, so the next pass retries it.
    pub fn upload_pass(
        &self,
        sessions: &[PathBuf],
        active: Option<&Path>,
        sent: &mut Sent,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for dir in sessions {
            let skip = if active == Some(dir.as_path()) {
                newest_wav(dir)
            } else {
                None
            };
            let changed = match changed_files(dir, sent, skip.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    errors.push(format!("{}: {e:#}", dir.display()));
                    continue;
                }
            };
            for (file, stamp) in changed {
                match self.put_file(dir, &file) {
                    Ok(()) => {
                        sent.insert(file, stamp);
                    }
                    Err(e) => errors.push(format!("{}: {e:#}", file.display())),
                }
            }
        }
        errors
    }

    /// `<prefix><session>/<file>`.
    fn key_for(&self, session_dir: &Path, file: &Path) -> Result<String> {
        let session = session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("session dir has no name: {}", session_dir.display()))?;
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .context("non-UTF-8 file name")?;
        Ok(format!("{}{session}/{name}", self.target.prefix))
    }

    fn put_file(&self, session_dir: &Path, path: &Path) -> Result<()> {
        let key = self.key_for(session_dir, path)?;
        let encoded = encode_path(&key);
        let payload_sha256 = sha256_file_hex(path)?;
        let now = chrono::Utc::now();
        let headers = sign_put(
            &self.creds,
            &self.target.host(),
            &self.target.object_path(&encoded),
            &payload_sha256,
            &now.format("%Y%m%dT%H%M%SZ").to_string(),
            &now.format("%Y%m%d").to_string(),
        );

        let url = format!(
            "https://{}{}",
            self.target.host(),
            self.target.object_path(&encoded)
        );
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut request = self.agent.put(&url);
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request
            .send(file)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("PUT {key} failed: {e}"))
    }
}

/// Regular files directly inside `dir`, sorted by name so uploads are in chunk
/// order. Dotfiles are skipped: they are temporaries such as `.meta.json.tmp`.
fn session_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    files.sort();
    Ok(files)
}

/// Files in `dir` that are new or differ from `sent`. `skip` is left out
/// entirely. A file that vanishes between listing and stat is ignored, since
/// compression can remove WAVs while a pass is running.
pub fn changed_files(
    dir: &Path,
    sent: &Sent,
    skip: Option<&Path>,
) -> Result<Vec<(PathBuf, Stamp)>> {
    let mut changed = Vec::new();
    for file in session_files(dir)? {
        if skip == Some(file.as_path()) {
            continue;
        }
        let Ok(now) = stamp(&file) else { continue };
        if sent.get(&file) != Some(&now) {
            changed.push((file, now));
        }
    }
    Ok(changed)
}

fn stamp(path: &Path) -> Result<Stamp> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    Ok(Stamp {
        len: meta.len(),
        modified: meta.modified().context("no modification time")?,
    })
}

/// The highest-indexed WAV in `dir`: the chunk most likely still being written.
pub fn newest_wav(dir: &Path) -> Option<PathBuf> {
    session_files(dir)
        .ok()?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .last()
}

/// Session directories under `root` whose names sort at or after `since`,
/// oldest first. Names are `%Y-%m-%dT%H-%M-%S`, so string order is time order.
pub fn sessions_since(root: &Path, since: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.as_bytes().first().is_some_and(u8::is_ascii_digit) && n >= since
                })
        })
        .collect();
    dirs.sort();
    dirs
}

/// What a periodic thread hands back when it stops: the errors it hit, and the
/// files it has already sent, so the final pass can skip them.
pub struct PeriodicResult {
    pub errors: Vec<String>,
    pub sent: Sent,
}

/// A running periodic-upload thread.
pub struct Periodic {
    stop: mpsc::Sender<()>,
    handle: JoinHandle<PeriodicResult>,
}

impl Periodic {
    /// Stop the thread after its current pass, if any, and collect its result.
    /// Any pass in flight is finished first, so this can block for as long as
    /// one upload takes.
    pub fn finish(self) -> PeriodicResult {
        let Periodic { stop, handle } = self;
        drop(stop);
        handle.join().unwrap_or_else(|_| PeriodicResult {
            errors: vec!["periodic upload thread panicked".into()],
            sent: Sent::new(),
        })
    }
}

/// Start a thread that uploads whatever changed every `interval`, for as long as
/// recording runs. It never prints: the TUI owns the terminal, so its errors
/// come back through `Periodic::finish`.
///
/// `since` is the name of the first session in this run. Sessions in `root`
/// with names at or after it belong to this run.
pub fn spawn_periodic(
    uploader: Arc<Uploader>,
    root: PathBuf,
    since: String,
    interval: Duration,
) -> Result<Periodic> {
    let (stop, stop_rx) = mpsc::channel::<()>();
    let handle = std::thread::Builder::new()
        .name("meetrs-upload".into())
        .spawn(move || {
            let mut sent = Sent::new();
            let mut errors = Vec::new();
            loop {
                match stop_rx.recv_timeout(interval) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    // The sender was dropped by `finish`, or a message arrived.
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                let sessions = sessions_since(&root, &since);
                // The newest session is the one being recorded. Earlier ones
                // have closed, so every file in them is final.
                let active = sessions.last().cloned();
                errors.extend(uploader.upload_pass(&sessions, active.as_deref(), &mut sent));
            }
            PeriodicResult { errors, sent }
        })
        .context("spawning upload thread")?;
    Ok(Periodic { stop, handle })
}

/// Percent-encode an object key for the URI, keeping `/` as the separator.
/// SigV4 requires exactly this encoding on both the request and the signature,
/// so the two must come from the same string.
fn encode_path(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The headers to send with a signed `PUT`, `Authorization` included.
///
/// Pure: the clock and the payload hash come in as arguments, so the same inputs
/// always produce the same signature and the tests can pin it.
pub fn sign_put(
    creds: &Credentials,
    host: &str,
    uri_path: &str,
    payload_sha256_hex: &str,
    amz_date: &str,
    date_stamp: &str,
) -> Vec<(String, String)> {
    let mut signed: Vec<(&str, String)> = vec![
        ("host", host.to_string()),
        ("x-amz-content-sha256", payload_sha256_hex.to_string()),
        ("x-amz-date", amz_date.to_string()),
    ];
    if let Some(token) = &creds.session_token {
        signed.push(("x-amz-security-token", token.clone()));
    }
    // SigV4 wants the canonical header names sorted.
    signed.sort_by(|a, b| a.0.cmp(b.0));

    let canonical_headers: String = signed
        .iter()
        .map(|(n, v)| format!("{n}:{v}\n"))
        .collect();
    let signed_names: Vec<&str> = signed.iter().map(|(n, _)| *n).collect();
    let signed_list = signed_names.join(";");

    let canonical_request = format!(
        "PUT\n{uri_path}\n\n{canonical_headers}\n{signed_list}\n{payload_sha256_hex}"
    );
    let scope = format!("{date_stamp}/{REGION}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let key = signing_key(&creds.secret_access_key, date_stamp, REGION, "s3");
    let signature = hex(&hmac_sha256(&key, string_to_sign.as_bytes()));

    let mut headers: Vec<(String, String)> = signed
        .iter()
        .map(|(n, v)| (n.to_string(), v.clone()))
        .collect();
    headers.push((
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_list}, Signature={signature}",
            creds.access_key_id
        ),
    ));
    headers
}

/// The SigV4 signing key: four chained HMACs over the date, region and service.
pub fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// HMAC-SHA256 (RFC 2104) on top of `sha2`.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();

    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Hash a file in fixed-size reads, so a long chunk is never held in memory.
fn sha256_file_hex(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fail early if a value is missing or the bucket name is unusable, so the
/// error appears at startup rather than after a long recording.
pub fn check_bucket_name(bucket: &str) -> Result<()> {
    if bucket.is_empty()
        || !bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("bucket name {bucket:?} is not valid (lowercase letters, digits and '-')");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_test_case_2() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signing_key_matches_aws_documented_example() {
        // From AWS's SigV4 documentation: secret, date 20150830, us-east-1, iam.
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn put_signature_matches_independent_reference() {
        // Expected value computed outside this code, with Python's hmac and
        // hashlib, from the same canonical request. Changing the signer changes
        // this value, which is the point.
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let headers = sign_put(
            &creds,
            "acct.r2.cloudflarestorage.com",
            "/bucket/meetings/2026-11-05T09-00-00/chunk-000.flac",
            &sha256_hex(b"hello"),
            "20261105T090000Z",
            "20261105",
        );
        let auth = &headers.iter().find(|(n, _)| n == "authorization").unwrap().1;
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261105/auto/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, \
             Signature=d2f121f319967a6f5dd006a1579b258cdcc10b7a91bf306854487e6f183bc322"
        );
    }

    #[test]
    fn session_token_is_signed_when_present() {
        let creds = Credentials {
            access_key_id: "AKID".into(),
            secret_access_key: "secret".into(),
            session_token: Some("tok".into()),
        };
        let headers = sign_put(&creds, "h", "/b/k", "00", "20260101T000000Z", "20260101");
        let auth = &headers.iter().find(|(n, _)| n == "authorization").unwrap().1;
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"));
        assert!(headers.iter().any(|(n, v)| n == "x-amz-security-token" && v == "tok"));
    }

    #[test]
    fn keys_are_percent_encoded_but_keep_slashes() {
        assert_eq!(encode_path("a b/c+d.flac"), "a%20b/c%2Bd.flac");
        assert_eq!(encode_path("2026-11-05T09-00-00/chunk-000.json"), "2026-11-05T09-00-00/chunk-000.json");
    }

    #[test]
    fn prefix_is_normalized() {
        assert_eq!(Target::new("a", "b", "/meetings/").prefix, "meetings/");
        assert_eq!(Target::new("a", "b", "").prefix, "");
        assert_eq!(Target::new("a", "b", "/").prefix, "");
    }

    #[test]
    fn bucket_names_are_checked() {
        assert!(check_bucket_name("hatcher-meetrs").is_ok());
        assert!(check_bucket_name("Hatcher").is_err());
        assert!(check_bucket_name("").is_err());
        assert!(check_bucket_name("a_b").is_err());
    }

    #[test]
    fn credentials_debug_never_shows_the_secret() {
        let creds = Credentials {
            access_key_id: "AKID".into(),
            secret_access_key: "very-secret".into(),
            session_token: Some("tok-secret".into()),
        };
        let shown = format!("{creds:?}");
        assert!(!shown.contains("very-secret"));
        assert!(!shown.contains("tok-secret"));
    }

    #[test]
    fn session_files_are_sorted_and_skip_dotfiles() {
        let dir = std::env::temp_dir().join(format!("meetrs-r2-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["chunk-001.flac", "chunk-000.flac", "meta.json", ".meta.json.tmp"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let names: Vec<String> = session_files(&dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["chunk-000.flac", "chunk-001.flac", "meta.json"]);
    }
}

#[cfg(test)]
mod periodic_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meetrs-r2-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_is_sent_once_then_only_when_it_changes() {
        let dir = scratch("changed");
        std::fs::write(dir.join("meta.json"), b"{}").unwrap();
        let mut sent = Sent::new();

        let first = changed_files(&dir, &sent, None).unwrap();
        assert_eq!(first.len(), 1);
        sent.insert(first[0].0.clone(), first[0].1);

        assert!(changed_files(&dir, &sent, None).unwrap().is_empty());

        // Different length, so the stamp differs even on a coarse clock.
        std::fs::write(dir.join("meta.json"), b"{\"a\":1}").unwrap();
        assert_eq!(changed_files(&dir, &sent, None).unwrap().len(), 1);
    }

    #[test]
    fn the_skipped_file_is_left_out_even_when_new() {
        let dir = scratch("skip");
        std::fs::write(dir.join("chunk-000.wav"), b"a").unwrap();
        std::fs::write(dir.join("chunk-001.wav"), b"b").unwrap();
        let active = newest_wav(&dir).unwrap();
        assert!(active.ends_with("chunk-001.wav"));

        let names: Vec<String> = changed_files(&dir, &Sent::new(), Some(&active))
            .unwrap()
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["chunk-000.wav"]);
    }

    #[test]
    fn only_sessions_at_or_after_the_run_start_are_picked_up() {
        let root = scratch("since");
        for name in [
            "2026-11-04T09-00-00",
            "2026-11-05T08-00-00",
            "2026-11-05T09-30-00",
            "scratch",
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let names: Vec<String> = sessions_since(&root, "2026-11-05T08-00-00")
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["2026-11-05T08-00-00", "2026-11-05T09-30-00"]);
    }

    #[test]
    fn a_missing_root_has_no_sessions() {
        let root = std::env::temp_dir().join("meetrs-r2-no-such-root-xyz");
        assert!(sessions_since(&root, "2026").is_empty());
    }
}
