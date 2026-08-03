// ponytail: unused until main.rs wires this in — silence dead_code until then.
#![allow(dead_code)]

//! Single-instance guard via `flock(2)`.
//!
//! Chosen over a PID file because the kernel releases an flock automatically
//! when the holding process dies — including SIGKILL and panics — so there is
//! no stale-lock case to detect or clean up. A PID file has no such guarantee;
//! it would need pid-liveness checks that flock makes unnecessary.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// Holds the single-instance lock for as long as it lives.
///
/// Dropping this releases the lock immediately — keep it bound in `main` for
/// the process lifetime (e.g. `let _lock = lock::acquire()?;`), don't let it
/// go out of scope before shutdown.
#[must_use = "dropping this releases the single-instance lock"]
pub struct InstanceLock(File);

/// Acquire the single-instance lock at `~/.meetrs/meetrs.lock`.
pub fn acquire() -> Result<InstanceLock> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    acquire_at(&PathBuf::from(home).join(".meetrs").join("meetrs.lock"))
}

fn acquire_at(path: &Path) -> Result<InstanceLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening lock file {}", path.display()))?;

    // SAFETY: flock is per-open-file-description; `file` is opened exactly
    // once above and its fd stays valid (and open) for the guard's lifetime.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            anyhow::bail!(
                "meetrs is already running (another process holds {}) — stop it first",
                path.display()
            );
        }
        return Err(err).with_context(|| format!("locking {}", path.display()));
    }

    // Informational only — never used to decide anything, the flock is the
    // actual mechanism. Just lets a human `cat` the file and see who holds it.
    file.set_len(0)
        .with_context(|| format!("truncating {}", path.display()))?;
    write!(file, "{}", std::process::id())
        .with_context(|| format!("writing pid to {}", path.display()))?;

    Ok(InstanceLock(file))
}

#[test]
fn second_acquire_fails_while_held_then_succeeds_after_drop() {
    let path = std::env::temp_dir().join(format!("meetrs-lock-test-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let first = acquire_at(&path).expect("first acquire should succeed");
    assert!(
        acquire_at(&path).is_err(),
        "second acquire should fail while the first guard is held"
    );

    drop(first);
    assert!(
        acquire_at(&path).is_ok(),
        "acquire should succeed again once the first guard is dropped"
    );

    let _ = std::fs::remove_file(&path);
}
