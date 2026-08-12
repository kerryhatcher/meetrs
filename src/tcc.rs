//! Making meetrs its own TCC subject.
//!
//! macOS attributes a privacy (TCC) request to the *responsible process*, not to
//! the process that made the call. For a program started from a shell the
//! responsible process is the terminal application, so a Core Audio process tap
//! opened by meetrs gets judged against the terminal's bundle. Terminals ship
//! `NSMicrophoneUsageDescription` but not `NSAudioCaptureUsageDescription`, and
//! tccd refuses a request outright — without ever prompting — when the subject
//! carries no usage-description string for the service:
//!
//! ```text
//! AUTHREQ_ATTRIBUTION: responsible={com.github.wez.wezterm}, accessing={meetrs}
//! AUTHREQ_SUBJECT:     subject=com.github.wez.wezterm
//! Refusing authorization request for service kTCCServiceAudioCapture
//!     and subject ...wezterm-gui without NSAudioCaptureUsageDescription key
//! AUTHREQ_RESULT:      authValue=0, authReason=8
//! ```
//!
//! coreaudiod then logs "Client is not granted access to the tap" and starts the
//! aggregate device anyway: `AudioDeviceStart` returns `noErr`, the IOProc is
//! never invoked, and from inside the process that is indistinguishable from a
//! callback that never registered. The microphone leg still works, because
//! terminals *do* carry the mic key — which is why this presents as "system
//! audio is bit-exact zero" and reads exactly like the process-tap zero-samples
//! bug. A user grant on `com.kerryhatcher.meetrs` cannot fix it, because meetrs
//! is never the subject being checked.
//!
//! So re-exec ourselves as our own responsible process. Then the subject is
//! meetrs.app, whose Info.plist carries both usage descriptions, and TCC
//! consults the grant for `com.kerryhatcher.meetrs` as intended.

use std::ffi::{CString, OsStr, OsString, c_int, c_short};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

/// Set in the re-executed image. Its presence means "already our own responsible
/// process", which is what keeps the re-exec from looping forever.
const SENTINEL: &str = "MEETRS_OWN_RESPONSIBILITY";

/// Replace the current process image instead of forking a child: same pid, same
/// tty, same file descriptors, so the TUI, the single-instance lock, and shell
/// job control are all unaffected. Defined here rather than taken from `libc`,
/// which does not expose it for every Apple target.
const POSIX_SPAWN_SETEXEC: c_short = 0x0040;

// Both private (`spawn_private.h` / `responsibility.h`), both in libSystem since
// 10.14. Declared rather than looked up with `dlsym` on purpose: if Apple ever
// drops one, a link error at build time is a better outcome than silently
// falling back to capturing nothing on a user's machine.
unsafe extern "C" {
    /// Marks the spawned image as *not* the caller's responsibility, so TCC
    /// attributes its requests to that image rather than to this process's
    /// responsible ancestor.
    fn responsibility_spawnattrs_setdisclaim(
        attrs: *mut libc::posix_spawnattr_t,
        disclaim: c_int,
    ) -> c_int;
    /// The pid TCC would attribute `pid`'s requests to; `-1` on failure.
    fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
}

/// Whether TCC will judge meetrs' own bundle when we ask for audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// This image is its own responsible process, so the subject is meetrs.app.
    Own,
    /// Not running from inside a `.app`, so there is no bundle identity to adopt.
    /// Disclaiming here would only make us responsible for a bare binary with no
    /// Info.plist, which tccd refuses for the very same missing-usage-description
    /// reason — so it is skipped, and the caller should say so.
    Unbundled,
}

/// Adopt meetrs' own TCC identity, re-executing this binary if needed.
///
/// On the re-exec path the process image is replaced, so this call does not
/// return; every path that returns means no re-exec happened. Must run before
/// anything opens the audio device.
pub fn adopt_own_identity() -> Result<Identity> {
    if std::env::var_os(SENTINEL).is_some() {
        return Ok(Identity::Own);
    }
    // Canonicalized because the installed entry point is a symlink into the
    // bundle (`~/.local/bin/meetrs`), and the bundle layout is what we test for.
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| anyhow!("locating our own executable: {e}"))?;
    if bundle_root(&exe).is_none() {
        return Ok(Identity::Unbundled);
    }
    Err(reexec_disclaimed(&exe))
}

/// Re-exec `exe` as its own responsible process. Returns only if the exec
/// failed, so an error is the only possible return value.
fn reexec_disclaimed(exe: &Path) -> anyhow::Error {
    let Ok(path) = CString::new(exe.as_os_str().as_bytes()) else {
        return anyhow!(
            "executable path contains an interior NUL: {}",
            exe.display()
        );
    };

    // argv[0] becomes the resolved bundle path; the rest is passed through
    // untouched so the re-executed image sees the same command line.
    let mut argv_owned = vec![path.clone()];
    for arg in std::env::args_os().skip(1) {
        match CString::new(arg.as_bytes()) {
            Ok(c) => argv_owned.push(c),
            Err(_) => return anyhow!("argument contains an interior NUL: {arg:?}"),
        }
    }

    let mut env_owned = Vec::new();
    for (key, value) in std::env::vars_os() {
        if key == OsStr::new(SENTINEL) {
            continue;
        }
        let mut entry = key.into_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        // A NUL inside an environment value cannot be passed on; dropping the
        // variable beats refusing to start over something we did not set.
        if let Ok(c) = CString::new(entry) {
            env_owned.push(c);
        }
    }
    env_owned.push(CString::new(format!("{SENTINEL}=1")).expect("no NUL in a literal"));

    let terminated = |owned: &[CString]| -> Vec<*mut libc::c_char> {
        owned
            .iter()
            .map(|c| c.as_ptr().cast_mut())
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect()
    };
    let argv = terminated(&argv_owned);
    let envp = terminated(&env_owned);

    // SAFETY: `attrs` is initialized before use and destroyed on every path.
    // `path`, `argv`, and `envp` all outlive the call, and both vectors are
    // NULL-terminated as posix_spawn requires. With POSIX_SPAWN_SETEXEC a
    // successful spawn never returns here at all.
    unsafe {
        let mut attrs: libc::posix_spawnattr_t = std::ptr::null_mut();
        let rc = libc::posix_spawnattr_init(&mut attrs);
        if rc != 0 {
            return spawn_err("posix_spawnattr_init", rc);
        }
        let rc = libc::posix_spawnattr_setflags(&mut attrs, POSIX_SPAWN_SETEXEC);
        if rc != 0 {
            libc::posix_spawnattr_destroy(&mut attrs);
            return spawn_err("posix_spawnattr_setflags", rc);
        }
        let rc = responsibility_spawnattrs_setdisclaim(&mut attrs, 1);
        if rc != 0 {
            libc::posix_spawnattr_destroy(&mut attrs);
            return spawn_err("responsibility_spawnattrs_setdisclaim", rc);
        }
        let rc = libc::posix_spawn(
            std::ptr::null_mut(),
            path.as_ptr(),
            std::ptr::null(),
            &attrs,
            argv.as_ptr(),
            envp.as_ptr(),
        );
        libc::posix_spawnattr_destroy(&mut attrs);
        spawn_err("posix_spawn", rc)
    }
}

/// The spawn family returns an errno value directly instead of setting `errno`.
fn spawn_err(what: &str, rc: c_int) -> anyhow::Error {
    anyhow!(
        "{what} failed while re-executing for TCC identity: {}",
        std::io::Error::from_raw_os_error(rc)
    )
}

/// How TCC will attribute this process, when that is not meetrs itself.
///
/// `None` means we are our own responsible process and there is nothing to
/// report. Purely diagnostic: it turns "the callback never ran" into the name of
/// the application actually being asked for permission.
pub fn foreign_responsible_process() -> Option<(libc::pid_t, PathBuf)> {
    let me = std::process::id() as libc::pid_t;
    // SAFETY: pid in, pid out; no pointers involved.
    let responsible = unsafe { responsibility_get_pid_responsible_for_pid(me) };
    if responsible <= 0 || responsible == me {
        return None;
    }
    let path = process_path(responsible).unwrap_or_else(|| PathBuf::from("<unknown>"));
    Some((responsible, path))
}

fn process_path(pid: libc::pid_t) -> Option<PathBuf> {
    /// `PROC_PIDPATHINFO_MAXSIZE`, which `libc` does not re-export.
    const MAX_PATH: usize = 4096;
    let mut buf = vec![0u8; MAX_PATH];
    // SAFETY: proc_pidpath writes at most `buf.len()` bytes into `buf` and
    // returns how many it wrote.
    let written = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if written <= 0 {
        return None;
    }
    buf.truncate(written as usize);
    Some(PathBuf::from(OsString::from_vec(buf)))
}

/// `…/meetrs.app/Contents/MacOS/meetrs` → `…/meetrs.app`, and `None` for
/// anything not laid out like a bundle executable.
fn bundle_root(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then_some(app)
}

#[cfg(test)]
mod tests {
    use super::bundle_root;
    use std::path::Path;

    #[test]
    fn recognizes_a_bundle_executable() {
        let exe = Path::new("/Users/x/Applications/meetrs.app/Contents/MacOS/meetrs");
        assert_eq!(
            bundle_root(exe),
            Some(Path::new("/Users/x/Applications/meetrs.app"))
        );
    }

    #[test]
    fn rejects_a_bare_binary() {
        // The exact path `cargo build` produces, which is how this failure
        // reaches people: no Info.plist, so no usage descriptions to consult.
        assert_eq!(bundle_root(Path::new("/repo/target/release/meetrs")), None);
    }

    #[test]
    fn rejects_a_partial_bundle_layout() {
        // Right depth, wrong directory names: must not be mistaken for a bundle.
        assert_eq!(
            bundle_root(Path::new("/repo/target/release/meetrs/bin/meetrs")),
            None
        );
        assert_eq!(
            bundle_root(Path::new("/opt/meetrs.app/Resources/MacOS/meetrs")),
            None
        );
    }
}
