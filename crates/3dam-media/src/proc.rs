//! Running a discovered external tool safely ([ADR 0014](../../../docs/adr/0014-video-decode-backend.md)).
//!
//! The video handler shells out to `ffprobe`/`ffmpeg` rather than linking libav*. That makes the
//! subprocess an untrusted-input boundary — the file we point it at may be hostile or merely
//! broken — so every invocation here is bounded three ways:
//!
//! - **argv vector, never a shell string.** No shell is involved, so a filename containing `;` or
//!   `$(…)` is just a filename. Paths are passed after `--`-style terminators where the tool
//!   supports it, and always as a single OS-string argument.
//! - **wall-clock timeout.** A malformed container can make a decoder spin; a scan worker must not
//!   be the thing that discovers this. On timeout the child is killed and the call fails soft.
//! - **capped stdout.** We stop reading past a byte limit so a pathological stream cannot balloon
//!   memory.
//!
//! A failure at any of these is a per-asset `None`, matching the fail-soft contract (golden rule 6).

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long any single probe/frame-grab may run before it is killed.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(20);

/// Ceiling on bytes read from a tool's stdout. A poster frame at 4K encodes well under this; JSON
/// metadata is a few KB.
pub const MAX_STDOUT: u64 = 64 * 1024 * 1024;

/// Locate an executable: an explicit `env_override` path wins, otherwise the first hit walking
/// `PATH`. Windows also tries the `.exe` suffix.
fn discover(bin: &str, env_override: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env_override) {
        let p = PathBuf::from(p);
        // An explicit override that doesn't exist is a configuration error worth saying out loud —
        // silently falling back to PATH would make the override look like it worked.
        if p.is_file() {
            return Some(p);
        }
        tracing::warn!(
            "{env_override} points at {} which is not a file; ignoring it",
            p.display()
        );
    }
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &[".exe", ""] } else { &[""] };
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter().find_map(|ext| {
            let cand = dir.join(format!("{bin}{ext}"));
            cand.is_file().then_some(cand)
        })
    })
}

/// A discovered tool, resolved once per process. `None` means "not installed" — a supported state,
/// not an error (ADR 0014 degradation tier 1).
pub struct Tool {
    bin: &'static str,
    env_override: &'static str,
    cell: OnceLock<Option<PathBuf>>,
}

impl Tool {
    pub const fn new(bin: &'static str, env_override: &'static str) -> Self {
        Self {
            bin,
            env_override,
            cell: OnceLock::new(),
        }
    }

    /// The resolved path, or `None` if the tool isn't installed. Discovery happens once; the result
    /// is cached for the life of the process (installing ffmpeg mid-scan is not a case we chase).
    pub fn path(&self) -> Option<&Path> {
        self.cell
            .get_or_init(|| {
                let found = discover(self.bin, self.env_override);
                match &found {
                    Some(p) => tracing::debug!("found {} at {}", self.bin, p.display()),
                    None => tracing::debug!(
                        "{} not found on PATH; video metadata/thumbnails degrade to the typed tile \
                         (set {} to point at it)",
                        self.bin,
                        self.env_override
                    ),
                }
                found
            })
            .as_deref()
    }

    pub fn available(&self) -> bool {
        self.path().is_some()
    }

    /// Run the tool with `args`, returning captured stdout on a clean exit. `None` if the tool is
    /// missing, failed to spawn, timed out, or exited non-zero.
    pub fn run<I, S>(&self, args: I) -> Option<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let exe = self.path()?;
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // stderr is where ffmpeg narrates; we don't parse it and don't want it on our console.
            .stderr(Stdio::null());
        run_bounded(cmd, TOOL_TIMEOUT, MAX_STDOUT, self.bin)
    }
}

/// Spawn `cmd`, read at most `max_stdout` bytes, and kill it if it outlives `timeout`.
fn run_bounded(
    mut cmd: Command,
    timeout: Duration,
    max_stdout: u64,
    label: &str,
) -> Option<Vec<u8>> {
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to spawn {label}: {e}");
            return None;
        }
    };
    // Take the pipe out before sharing the child, so reading never contends with the watchdog's
    // lock — otherwise the killer could block behind a read that only a kill would unblock.
    let stdout = child.stdout.take()?;
    let child = Arc::new(Mutex::new(child));
    let finished = Arc::new(AtomicBool::new(false));

    let watchdog = {
        let child = Arc::clone(&child);
        let finished = Arc::clone(&finished);
        let label = label.to_string();
        std::thread::spawn(move || {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if finished.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if !finished.load(Ordering::Relaxed) {
                tracing::warn!("{label} exceeded {timeout:?}; killing it");
                if let Ok(mut c) = child.lock() {
                    let _ = c.kill();
                }
            }
        })
    };

    let mut buf = Vec::new();
    let read = stdout.take(max_stdout).read_to_end(&mut buf);
    finished.store(true, Ordering::Relaxed);

    let status = child.lock().ok().and_then(|mut c| c.wait().ok());
    let _ = watchdog.join();

    if read.is_err() {
        return None;
    }
    match status {
        Some(s) if s.success() => Some(buf),
        _ => None,
    }
}
