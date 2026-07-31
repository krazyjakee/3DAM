//! SFTP file source over `russh` + `russh-sftp` (tech-spec 07 §3.2).
//!
//! Pure-Rust async SSH; the connection is established once at construction and reused. Because the
//! engine's scan runs inside `spawn_blocking` (off the async workers — tech-spec 14), the sync
//! `FileSource` methods drive the async client on a private **current-thread** runtime. There is no
//! push-notification channel over SFTP, so watch is poll-based upstream (§3.2) — this backend only
//! provides enumerate + fetch.
//!
//! v1 secret handling: the password/key comes straight from the source record's connection blob
//! (tech-spec 10 owns a real keyring later), and the server host key is trust-on-first-use
//! (accepted) — a documented v1 limitation, revisited with the auth work.

use crate::{guard_rel_path, Fetched, FileEntry, FileSource, SftpConfig};
use dam_api::LibError;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

/// Minimal client handler. Accepts the server key (trust-on-first-use, v1).
struct Client;

impl russh::client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct SftpSource {
    cfg: SftpConfig,
    rt: Runtime,
    session: Mutex<russh_sftp::client::SftpSession>,
    /// Where downloads are materialised (issue #87) — under the data dir, not the OS temp dir.
    scratch: std::path::PathBuf,
    /// Keeps the SSH connection alive for as long as the source exists.
    _handle: Mutex<russh::client::Handle<Client>>,
}

impl SftpSource {
    pub fn connect(cfg: SftpConfig, scratch: std::path::PathBuf) -> Result<SftpSource, LibError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LibError::Internal(format!("sftp runtime: {e}")))?;
        let (handle, session) = rt.block_on(connect_inner(&cfg))?;
        Ok(SftpSource {
            cfg,
            rt,
            session: Mutex::new(session),
            scratch,
            _handle: Mutex::new(handle),
        })
    }

    /// Resolve a source-relative path to the absolute remote path.
    fn remote_path(&self, rel: &str) -> String {
        join_remote(&self.cfg.base_path, rel)
    }
}

async fn connect_inner(
    cfg: &SftpConfig,
) -> Result<
    (
        russh::client::Handle<Client>,
        russh_sftp::client::SftpSession,
    ),
    LibError,
> {
    let config = Arc::new(russh::client::Config::default());
    let mut handle = russh::client::connect(config, (cfg.host.as_str(), cfg.port), Client)
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("sftp connect {}: {e}", cfg.host)))?;

    let authed = if let Some(key_path) = &cfg.private_key {
        let key = russh::keys::load_secret_key(key_path, cfg.passphrase.as_deref())
            .map_err(|e| LibError::BadRequest(format!("load key {key_path}: {e}")))?;
        let key = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None);
        handle
            .authenticate_publickey(cfg.username.clone(), key)
            .await
    } else if let Some(pw) = &cfg.password {
        handle
            .authenticate_password(cfg.username.clone(), pw.clone())
            .await
    } else {
        return Err(LibError::BadRequest(
            "sftp source needs a password or private key".into(),
        ));
    }
    .map_err(|e| LibError::SourceUnavailable(format!("sftp auth: {e}")))?;

    if !authed.success() {
        return Err(LibError::Forbidden(format!(
            "sftp authentication failed for {}@{}",
            cfg.username, cfg.host
        )));
    }

    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("sftp channel: {e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("sftp subsystem: {e}")))?;
    let session = russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("sftp session: {e}")))?;
    Ok((handle, session))
}

impl FileSource for SftpSource {
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        // Iterative DFS over remote directories (out-of-core; no full tree in RAM at once).
        let mut stack: Vec<String> = vec![String::new()];
        while let Some(rel_dir) = stack.pop() {
            let abs_dir = self.remote_path(&rel_dir);
            let listing = self.rt.block_on(async {
                let session = self.session.lock().await;
                session
                    .read_dir(abs_dir.clone())
                    .await
                    .map_err(|e| LibError::SourceUnavailable(format!("read_dir {abs_dir}: {e}")))
            });
            let listing = match listing {
                Ok(l) => l,
                Err(e) => {
                    if !sink(Err(e)) {
                        return Ok(());
                    }
                    continue;
                }
            };
            for entry in listing {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let meta = entry.metadata();
                let child_rel = if rel_dir.is_empty() {
                    name.clone()
                } else {
                    format!("{rel_dir}/{name}")
                };
                if meta.is_dir() {
                    stack.push(child_rel);
                    continue;
                }
                let fe = FileEntry {
                    rel_path: child_rel,
                    size: meta.size.unwrap_or(0),
                    modified_ms: meta.mtime.map(|s| s as i64 * 1000),
                };
                if !sink(Ok(fe)) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Download to a scratch file, **streaming** (issue #87).
    ///
    /// `SftpSession::read` returns the whole file as a `Vec<u8>`, which meant a remote asset was
    /// held entirely in memory *and* then written to a tmpfs temp file — two full copies in RAM for
    /// a file that may be gigabytes. `open` hands back an `AsyncRead` instead, so bytes go
    /// chunk-by-chunk from the socket to disk and peak memory is one buffer.
    fn fetch(&self, rel_path: &str) -> Result<Fetched, LibError> {
        guard_rel_path(rel_path)?;
        let abs = self.remote_path(rel_path);
        let mut sink = crate::temp_sink(rel_path, &self.scratch)?;
        self.rt.block_on(async {
            use tokio::io::AsyncReadExt;
            let session = self.session.lock().await;
            let mut remote = session
                .open(abs.clone())
                .await
                .map_err(|e| LibError::SourceUnavailable(format!("open {abs}: {e}")))?;
            let mut buf = vec![0u8; crate::FETCH_CHUNK];
            loop {
                let n = remote
                    .read(&mut buf)
                    .await
                    .map_err(|e| LibError::SourceUnavailable(format!("read {abs}: {e}")))?;
                if n == 0 {
                    break;
                }
                // A blocking write inside `block_on` is fine here: this runtime is current-thread
                // and private to the source, and the caller already handed off via `spawn_blocking`.
                std::io::Write::write_all(&mut sink, &buf[..n])
                    .map_err(|e| LibError::Internal(format!("scratch write: {e}")))?;
            }
            Ok::<(), LibError>(())
        })?;
        std::io::Write::flush(&mut sink).ok();
        Ok(Fetched::Temp(sink))
    }
}

/// Join a base directory and a source-relative path into a `/`-separated remote path.
fn join_remote(base: &str, rel: &str) -> String {
    let base = base.trim_end_matches('/');
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        if base.is_empty() {
            ".".to_string()
        } else {
            base.to_string()
        }
    } else if base.is_empty() || base == "." {
        rel.to_string()
    } else {
        format!("{base}/{rel}")
    }
}
