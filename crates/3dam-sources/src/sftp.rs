//! SFTP file source over `russh` + `russh-sftp` (tech-spec 07 §3.2).
//!
//! Pure-Rust async SSH; the connection is established once at construction and reused. Because the
//! engine's scan runs inside `spawn_blocking` (off the async workers — tech-spec 14), the sync
//! `FileSource` methods drive the async client on a private **current-thread** runtime. There is no
//! push-notification channel over SFTP, so watch is poll-based upstream (§3.2) — this backend only
//! provides enumerate + fetch.
//!
//! Password/key material is hydrated from the host secret store immediately before construction;
//! the portable source connection contains only non-secret host/path/user fields (issue #103).
//! The server host key is trust-on-first-use (accepted) — a documented v1 limitation.

use crate::{guard_rel_path, ContentStat, Fetched, FileEntry, FileSource, SftpConfig};
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
    /// Raw listing protocol uses one server page at a time; high-level read_dir collects all
    /// pages. Reuse a second subsystem on the same authenticated SSH connection.
    listing_session: Mutex<russh_sftp::client::RawSftpSession>,
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
        let (handle, session, listing_session) = rt.block_on(connect_inner(&cfg))?;
        Ok(SftpSource {
            cfg,
            rt,
            session: Mutex::new(session),
            listing_session: Mutex::new(listing_session),
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
        russh_sftp::client::RawSftpSession,
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
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("sftp listing channel: {e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("sftp listing subsystem: {e}")))?;
    let listing_session = russh_sftp::client::RawSftpSession::new(channel.into_stream());
    listing_session
        .init()
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("sftp listing session: {e}")))?;
    Ok((handle, session, listing_session))
}

impl FileSource for SftpSource {
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        self.walk_filtered(&mut |_| Ok(true), &mut || Ok(()), sink)
    }

    fn walk_filtered(
        &self,
        eligible: &mut dyn FnMut(&str) -> Result<bool, LibError>,
        pace: &mut dyn FnMut() -> Result<(), LibError>,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        use russh_sftp::client::error::Error;
        use russh_sftp::protocol::StatusCode;

        // Iterative DFS over remote directories (out-of-core; no full tree in RAM at once).
        let mut stack: Vec<String> = vec![String::new()];
        while let Some(rel_dir) = stack.pop() {
            let abs_dir = self.remote_path(&rel_dir);
            let keep_going = self.rt.block_on(async {
                let session = self.listing_session.lock().await;
                pace()?;
                let handle = match session.opendir(abs_dir.clone()).await {
                    Ok(handle) => handle.handle,
                    Err(error) => {
                        return Ok(sink(Err(LibError::SourceUnavailable(format!(
                            "sftp open directory {abs_dir}: {error}"
                        )))));
                    }
                };
                // Keep cleanup outside the fallible loop: pace, predicate, server errors and
                // sink cancellation all close the directory before returning to the caller.
                let outcome: Result<bool, LibError> = async {
                    loop {
                        pace()?;
                        let page = match session.readdir(handle.as_str()).await {
                            Ok(page) => page,
                            Err(Error::Status(status)) if status.status_code == StatusCode::Eof => {
                                return Ok(true)
                            }
                            Err(error) => {
                                return Ok(sink(Err(LibError::SourceUnavailable(format!(
                                    "sftp read directory {abs_dir}: {error}"
                                )))));
                            }
                        };
                        for entry in page.files {
                            pace()?;
                            let name = entry.filename;
                            if name == "." || name == ".." {
                                continue;
                            }
                            let child_rel = if rel_dir.is_empty() {
                                name
                            } else {
                                format!("{rel_dir}/{name}")
                            };
                            if entry.attrs.is_dir() {
                                stack.push(child_rel);
                            } else if eligible(&child_rel)?
                                && !sink(Ok(FileEntry {
                                    rel_path: child_rel,
                                    size: entry.attrs.size.unwrap_or(0),
                                    modified_ms: entry.attrs.mtime.map(|s| s as i64 * 1000),
                                }))
                            {
                                return Ok(false);
                            }
                        }
                    }
                }
                .await;
                let closed = session.close(handle).await;
                let keep_going = outcome?;
                if let Err(error) = closed {
                    return Err(LibError::SourceUnavailable(format!(
                        "sftp close directory {abs_dir}: {error}"
                    )));
                }
                Ok::<bool, LibError>(keep_going)
            })?;
            if !keep_going {
                return Ok(());
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
        self.fetch_paced(rel_path, &mut |_| Ok(()))
    }

    fn fetch_paced(
        &self,
        rel_path: &str,
        pace: &mut dyn FnMut(u64) -> Result<(), LibError>,
    ) -> Result<Fetched, LibError> {
        let rel_path = guard_rel_path(rel_path)?;
        let abs = self.remote_path(&rel_path);
        let mut sink = crate::temp_sink(&rel_path, &self.scratch)?;
        let (content_hash, source_stat) = self.rt.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let session = self.session.lock().await;
            let mut remote = session
                .open(abs.clone())
                .await
                .map_err(|e| LibError::SourceUnavailable(format!("open {abs}: {e}")))?;
            let outcome = async {
                let before = remote.metadata().await.map_err(|e| {
                    LibError::SourceUnavailable(format!("sftp fetch stat {abs}: {e}"))
                })?;
                let mut hasher = blake3::Hasher::new();
                let mut copied = 0u64;
                let mut buf = vec![0u8; crate::FETCH_CHUNK];
                loop {
                    pace(crate::FETCH_CHUNK as u64)?;
                    let n = remote
                        .read(&mut buf)
                        .await
                        .map_err(|e| LibError::SourceUnavailable(format!("read {abs}: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    // This private current-thread runtime already runs on spawn_blocking.
                    std::io::Write::write_all(&mut sink, &buf[..n])
                        .map_err(|e| LibError::Internal(format!("scratch write: {e}")))?;
                    hasher.update(&buf[..n]);
                    copied = copied.saturating_add(n as u64);
                }
                let after = remote.metadata().await.map_err(|e| {
                    LibError::SourceUnavailable(format!("sftp fetch stat {abs}: {e}"))
                })?;
                if copied != after.len()
                    || before.len() != after.len()
                    || before.modified().ok() != after.modified().ok()
                {
                    return Err(crate::source_changed());
                }
                Ok::<_, LibError>((
                    hasher.finalize().to_hex().to_string(),
                    ContentStat {
                        len: after.len(),
                        modified_ms: after.modified().ok().and_then(crate::system_time_ms),
                    },
                ))
            }
            .await;
            let closed = remote.shutdown().await;
            let fetched = outcome?;
            closed.map_err(|e| LibError::SourceUnavailable(format!("sftp close {abs}: {e}")))?;
            Ok::<_, LibError>(fetched)
        })?;
        Ok(Fetched::HashedTemp {
            file: sink,
            content_hash,
            source_stat: Some(source_stat),
        })
    }

    fn content_stat(&self, rel_path: &str) -> Result<ContentStat, LibError> {
        let rel_path = guard_rel_path(rel_path)?;
        let abs = self.remote_path(&rel_path);
        self.rt.block_on(async {
            let session = self.session.lock().await;
            let metadata = session
                .metadata(abs.clone())
                .await
                .map_err(|e| LibError::SourceUnavailable(format!("sftp stat {abs}: {e}")))?;
            Ok(ContentStat {
                len: metadata.len(),
                modified_ms: metadata.modified().ok().and_then(crate::system_time_ms),
            })
        })
    }

    fn read_range(
        &self,
        rel_path: &str,
        offset: u64,
        mut length: u64,
        sink: &mut dyn FnMut(Vec<u8>) -> bool,
    ) -> Result<(), LibError> {
        let rel_path = guard_rel_path(rel_path)?;
        let abs = self.remote_path(&rel_path);
        self.rt.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};

            let session = self.session.lock().await;
            let mut remote = session
                .open(abs.clone())
                .await
                .map_err(|e| LibError::SourceUnavailable(format!("sftp open {abs}: {e}")))?;
            remote
                .seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| LibError::SourceUnavailable(format!("sftp seek {abs}: {e}")))?;
            while length > 0 {
                let wanted = length.min(crate::FETCH_CHUNK as u64) as usize;
                let mut chunk = vec![0u8; wanted];
                remote.read_exact(&mut chunk).await.map_err(|e| {
                    LibError::SourceUnavailable(format!("sftp range read {abs}: {e}"))
                })?;
                length -= wanted as u64;
                if !sink(chunk) {
                    return Ok(());
                }
            }
            Ok(())
        })
    }

    // ── write side (issue #80 slice 7) ───────────────────────────────────────

    /// The *kind* can be written to; whether this credential can is settled by [`Self::put`].
    ///
    /// Unlike the local backend, which answers by asking the kernel, there is no way to test remote
    /// write access without a round trip — and this is consulted for every source in a listing. See
    /// [`crate::writable_without_handshake`] for why that budget matters. Answering `true` here
    /// means the destination picker offers the source and the *first* file reports a permission
    /// problem, rather than all of them failing after the user has dropped two hundred.
    fn writable(&self) -> bool {
        true
    }

    fn mkdir(&self, rel_path: &str) -> Result<(), LibError> {
        let rel = crate::safe_name::check_rel_path(rel_path)?;
        // Idempotent, like the local impl and like `mkdir -p`: every ancestor is created, and one
        // that already exists is success. SFTP has no `create_dir_all`, so walk the components —
        // and swallow *each* failure, because "already exists" is not distinguishable from a real
        // error in SSH_FX_FAILURE. A genuine problem surfaces on the `put` that follows.
        let mut acc = String::new();
        for seg in rel.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            let abs = self.remote_path(&acc);
            let _ = self.rt.block_on(async {
                let session = self.session.lock().await;
                session.create_dir(abs).await
            });
        }
        // Confirm the leaf really is there, so a caller is not told a directory exists when the
        // whole walk silently failed.
        let abs = self.remote_path(&rel);
        self.rt.block_on(async {
            let session = self.session.lock().await;
            session
                .metadata(abs.clone())
                .await
                .map_err(|e| LibError::SourceUnavailable(format!("mkdir {abs}: {e}")))
                .and_then(|m| {
                    if m.is_dir() {
                        Ok(())
                    } else {
                        Err(LibError::Conflict(format!(
                            "{rel} exists and is not a folder"
                        )))
                    }
                })
        })
    }

    /// Create a **new** remote file, streaming from `bytes`.
    ///
    /// Two protocol facts carry the non-destructive invariant here, and neither is a convention we
    /// have to remember:
    ///
    /// 1. **`CREATE | EXCLUDE` is SFTP's `O_EXCL`** — the server itself refuses if the name is
    ///    taken, atomically. That is what reserves the `.part` name against a second uploader.
    /// 2. **`SSH_FXP_RENAME` is specified to fail when the destination exists**
    ///    (draft-ietf-secsh-filexfer-02 §6.5), unlike POSIX `rename(2)` which replaces silently.
    ///
    /// The `.part`-then-rename shape is what keeps a crash from leaving a half-written file at the
    /// real name for the next scan to catalogue as a truncated asset.
    ///
    /// **The one honest caveat**: fact 2 is the server's to honour, and we cannot verify it from
    /// here. OpenSSH implements it as stat-then-rename, which is spec-correct but not atomic, so a
    /// file appearing in that window would be replaced. The pre-check below closes the ordinary
    /// case; a server that implements rename with POSIX overwrite semantics would defeat both. This
    /// is strictly weaker than the local backend's `persist_noclobber`, and it is weaker because of
    /// the protocol, not the implementation.
    fn put(&self, rel_path: &str, bytes: &mut dyn std::io::Read) -> Result<(), LibError> {
        use russh_sftp::protocol::OpenFlags;
        use tokio::io::AsyncWriteExt;

        let rel = crate::safe_name::check_rel_path(rel_path)?;
        let abs = self.remote_path(&rel);
        // A *unique* staging name, not `{abs}.3dam-part`. A fixed one would be a shared resource:
        // an orphan left by a `kill -9` would make that filename permanently un-uploadable, since
        // every retry would hit the `EXCLUDE` reservation and fail with an error the user cannot
        // act on — and two concurrent uploads of the same name would collide on the staging file
        // rather than on the destination, which is where the collision belongs.
        let part = format!("{abs}.{}.3dam-part", stage_nonce());

        self.rt.block_on(async {
            let session = self.session.lock().await;

            // Cheap, clear rejection before transferring anything. Not the guarantee — that is the
            // rename — but it turns the common case into a `Conflict` the user can act on rather
            // than a transfer that is thrown away at the end.
            if session.try_exists(abs.clone()).await.unwrap_or(false) {
                return Err(LibError::Conflict(format!("{rel} already exists")));
            }

            // Create the parent chain, as the local backend does — and only now, *after* the name
            // has been found free, so a *collision* creates nothing at all. Upload relies on that:
            // it deliberately does not call `mkdir` first. Levels created here are rolled back
            // below if the transfer itself then fails.
            let mut made: Vec<String> = Vec::new();
            if let (Some(dir), _) = crate::safe_name::split_parent(&rel) {
                let mut acc = String::new();
                for seg in dir.split('/') {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(seg);
                    // Ignored per level: SFTP reports "already exists" as an undifferentiated
                    // failure, so the only honest test is whether the `.part` open below works.
                    if session.create_dir(self.remote_path(&acc)).await.is_ok() {
                        made.push(acc.clone());
                    }
                }
            }

            // `CREATE | EXCLUDE` on a name we just minted: this can only fail for a real reason,
            // never because a previous attempt left something behind.
            let opened = session
                .open_with_flags(
                    part.clone(),
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                )
                .await;
            let mut remote = match opened {
                Ok(f) => f,
                Err(e) => {
                    rollback_dirs(&session, self, &made).await;
                    return Err(LibError::SourceUnavailable(format!(
                        "create staging {rel}: {e}"
                    )));
                }
            };

            // Stream: peak memory is one buffer whatever the file's size, matching `fetch`.
            let mut buf = vec![0u8; crate::FETCH_CHUNK];
            let outcome = async {
                loop {
                    let n = bytes
                        .read(&mut buf)
                        .map_err(|e| LibError::Internal(format!("read staged bytes: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    remote
                        .write_all(&buf[..n])
                        .await
                        .map_err(|e| LibError::SourceUnavailable(format!("write {rel}: {e}")))?;
                }
                remote
                    .flush()
                    .await
                    .map_err(|e| LibError::SourceUnavailable(format!("flush {rel}: {e}")))?;
                remote
                    .shutdown()
                    .await
                    .map_err(|e| LibError::SourceUnavailable(format!("close {rel}: {e}")))?;
                Ok::<(), LibError>(())
            }
            .await;

            // Close the handle *before* unlinking. A POSIX server is happy to remove an open file,
            // but Windows servers refuse with a sharing violation — which would leave the staging
            // file behind on exactly the platform where that is hardest to notice.
            let _ = remote.shutdown().await;
            drop(remote);

            if let Err(e) = outcome {
                let _ = session.remove_file(part.clone()).await;
                rollback_dirs(&session, self, &made).await;
                return Err(e);
            }

            match session.rename(part.clone(), abs.clone()).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = session.remove_file(part.clone()).await;
                    rollback_dirs(&session, self, &made).await;
                    // A spec-compliant server refuses the rename precisely because the name was
                    // taken in the meantime — which is a collision, not a transport failure, and
                    // the caller's `Suffix`/`Skip` handling keys off that distinction.
                    if session.try_exists(abs.clone()).await.unwrap_or(false) {
                        Err(LibError::Conflict(format!("{rel} already exists")))
                    } else {
                        Err(LibError::SourceUnavailable(format!("commit {rel}: {e}")))
                    }
                }
            }
        })
    }
}

/// A per-attempt token for the staging filename — process id plus a monotonic counter, which is
/// enough to be unique among every attempt this process makes and every other process on the host.
/// It does not need to be unguessable: the name is `EXCLUDE`-created, so a guess cannot hijack it.
fn stage_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Remove directory levels this `put` created, deepest first, after it failed.
///
/// Best-effort and deliberately narrow: `remove_dir` fails on a non-empty directory, which is
/// exactly the guard wanted — anything another upload has since put in there stops the rollback
/// dead rather than taking someone else's file with it.
async fn rollback_dirs(
    session: &russh_sftp::client::SftpSession,
    src: &SftpSource,
    made: &[String],
) {
    for rel in made.iter().rev() {
        if session.remove_dir(src.remote_path(rel)).await.is_err() {
            break; // non-empty (or refused): every shallower level is non-empty too
        }
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
