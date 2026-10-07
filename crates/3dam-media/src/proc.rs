//! Running a discovered external tool safely ([ADR 0015](../../../docs/adr/0015-video-decode-backend.md)).
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
/// not an error (ADR 0015 degradation tier 1).
pub struct Tool {
    bin: &'static str,
    env_override: &'static str,
    cell: OnceLock<Option<PathBuf>>,
    #[cfg(all(test, unix))]
    fixture_script: Option<PathBuf>,
}

impl Tool {
    pub const fn new(bin: &'static str, env_override: &'static str) -> Self {
        Self {
            bin,
            env_override,
            cell: OnceLock::new(),
            #[cfg(all(test, unix))]
            fixture_script: None,
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

    #[cfg(all(test, unix))]
    pub(crate) fn at_script(path: PathBuf) -> Self {
        // Executing a newly written fixture directly can fail with ETXTBSY while a parallel
        // spawn briefly retains its writable descriptor. Let an existing shell read the fixture
        // as data; argv, deadlines, output limits and process groups still use the real runner.
        let cell = OnceLock::new();
        let _ = cell.set(Some(PathBuf::from("/bin/sh")));
        Self {
            bin: "fake ffprobe",
            env_override: "unused",
            cell,
            fixture_script: Some(path),
        }
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
        self.run_cancellable(args, TOOL_TIMEOUT, MAX_STDOUT, &|| false)
    }

    pub(crate) fn run_cancellable<I, S>(
        &self,
        args: I,
        timeout: Duration,
        max_stdout: u64,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        if cancelled() || timeout.is_zero() {
            return None;
        }
        let exe = self.path()?;
        let mut cmd = Command::new(exe);
        #[cfg(all(test, unix))]
        if let Some(script) = &self.fixture_script {
            cmd.arg(script);
        }
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // stderr is where ffmpeg narrates; we don't parse it and don't want it on our console.
            .stderr(Stdio::null());
        run_bounded(cmd, timeout, max_stdout, self.bin, cancelled)
    }
}

/// Spawn `cmd`, read at most `max_stdout` bytes, and kill it if it outlives `timeout`.
fn run_bounded(
    mut cmd: Command,
    timeout: Duration,
    max_stdout: u64,
    label: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<Vec<u8>> {
    if cancelled() || timeout.is_zero() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Wrappers may launch descendants which inherit the stdout pipe. Killing
        // only the wrapper would leave the reader blocked on that inherited fd.
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!("failed to spawn {label}: {error}");
            return None;
        }
    };
    let stdout = child.stdout.take()?;
    let child = Arc::new(Mutex::new(child));
    let finished = AtomicBool::new(false);
    let aborted = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let watchdog = scope.spawn(|| {
            let deadline = Instant::now() + timeout;
            loop {
                if finished.load(Ordering::Acquire) {
                    return;
                }
                if cancelled() || Instant::now() >= deadline {
                    if let Ok(mut child) = child.lock() {
                        if finished.load(Ordering::Acquire) {
                            return;
                        }
                        aborted.store(true, Ordering::Release);
                        terminate(&mut child);
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        // One extra byte distinguishes a complete response from truncated JSON.
        let mut bytes = Vec::new();
        let read = stdout
            .take(max_stdout.saturating_add(1))
            .read_to_end(&mut bytes);
        if bytes.len() as u64 > max_stdout || read.is_err() {
            aborted.store(true, Ordering::Release);
            if let Ok(mut child) = child.lock() {
                terminate(&mut child);
            }
        }
        // Keep the watchdog armed while the process finishes even if stdout closed.
        // Waiting under the mutex would prevent the watchdog from killing it.
        let status = loop {
            let status = child.lock().ok().and_then(|mut child| {
                let status = child.try_wait().ok();
                if matches!(status, Some(Some(_))) {
                    // Publish completion before releasing the process lock, so
                    // the watchdog cannot signal a group after its leader was
                    // reaped and the pid became eligible for reuse.
                    finished.store(true, Ordering::Release);
                }
                status
            });
            match status {
                Some(Some(status)) => break Some(status),
                Some(None) => std::thread::sleep(Duration::from_millis(10)),
                None => {
                    if let Ok(mut child) = child.lock() {
                        terminate(&mut child);
                        let _ = child.wait();
                    }
                    break None;
                }
            }
        };
        finished.store(true, Ordering::Release);
        let _ = watchdog.join();
        if aborted.load(Ordering::Acquire) || cancelled() {
            return None;
        }
        status.filter(|status| status.success()).map(|_| bytes)
    })
}

fn terminate(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: run_bounded created a dedicated process group whose leader is
        // this still-owned, unreaped child. A negative pid targets that group.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell_tool(dir: &Path, body: &str) -> Tool {
        let path = dir.join("probe.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        Tool::at_script(path)
    }

    #[test]
    fn timeout_kills_descendants_holding_inherited_stdout() {
        let dir = tempfile::tempdir().unwrap();
        // The wrapper exits immediately; its child alone keeps stdout open.
        let tool = shell_tool(dir.path(), "sleep 3 &\nprintf '{}'\nexit 0");
        let start = Instant::now();
        let output = tool.run_cancellable(
            std::iter::empty::<&str>(),
            Duration::from_millis(100),
            4096,
            &|| false,
        );
        assert!(output.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "inherited stdout outlived the watchdog"
        );
    }

    #[test]
    fn cancellation_kills_descendants_holding_inherited_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shell_tool(dir.path(), "sleep 3 &\nprintf '{}'\nwait");
        let start = Instant::now();
        let output = tool.run_cancellable(
            std::iter::empty::<&str>(),
            Duration::from_secs(10),
            4096,
            &|| start.elapsed() >= Duration::from_millis(100),
        );
        assert!(output.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cancellation left inherited stdout open"
        );
    }

    #[test]
    fn script_fixture_runs_with_a_retained_writable_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shell_tool(dir.path(), "printf '{\"streams\":[]}'");
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("probe.sh"))
            .unwrap();
        assert_eq!(
            tool.run_cancellable(
                std::iter::empty::<&str>(),
                Duration::from_secs(2),
                4096,
                &|| false
            ),
            Some(br#"{"streams":[]}"#.to_vec())
        );
        drop(writer);
    }

    #[test]
    fn normal_short_tool_still_returns_complete_output() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shell_tool(dir.path(), "printf '{\"streams\":[]}'");
        assert_eq!(
            tool.run_cancellable(
                std::iter::empty::<&str>(),
                Duration::from_secs(2),
                4096,
                &|| false,
            ),
            Some(br#"{"streams":[]}"#.to_vec())
        );
    }
}
