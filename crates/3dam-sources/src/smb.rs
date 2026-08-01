//! SMB2/3 file source over the pure-Rust `smb` crate (tech-spec 07 §3.3).
//!
//! Like SFTP, this is driven from a private current-thread runtime because the engine's scan runs
//! inside `spawn_blocking`. SMB change-notify is uneven across servers, so v1 treats SMB as
//! poll-only upstream (§3.3) — this backend provides enumerate + fetch. The share connection is
//! established once at construction and reused.
//!
//! v1 secret handling matches SFTP: credentials come from the source record's connection blob.
//! Only the default SMB port is supported in v1 (share_connect resolves the server from the UNC);
//! a non-default port is rejected up front rather than silently ignored.

use crate::{guard_rel_path, Fetched, FileEntry, FileSource, SmbConfig};
use dam_api::LibError;
use futures::StreamExt;
use smb::create::CreateDisposition;
use smb::resource::{Directory, Resource};
use smb::{
    Client, ClientConfig, CreateOptions, FileAccessMask, FileAttributes, FileCreateArgs,
    FileDirectoryInformation, UncPath,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::runtime::Runtime;

pub struct SmbSource {
    cfg: SmbConfig,
    rt: Runtime,
    client: Client,
    unc: UncPath,
    /// Where downloads are materialised (issue #87) — under the data dir, not the OS temp dir.
    scratch: std::path::PathBuf,
}

impl SmbSource {
    pub fn connect(cfg: SmbConfig, scratch: std::path::PathBuf) -> Result<SmbSource, LibError> {
        if cfg.port != 445 {
            return Err(LibError::Unsupported(format!(
                "SMB on a non-default port ({}) is not supported in this build",
                cfg.port
            )));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LibError::Internal(format!("smb runtime: {e}")))?;
        let client = Client::new(ClientConfig::default());
        let unc = UncPath::from_str(&format!(r"\\{}\{}", cfg.host, cfg.share)).map_err(|e| {
            LibError::BadRequest(format!("bad smb share //{}/{}: {e}", cfg.host, cfg.share))
        })?;

        // sspi accepts `DOMAIN\user`; fold the domain into the username when supplied.
        let user = match &cfg.domain {
            Some(d) if !d.is_empty() => format!("{d}\\{}", cfg.username),
            _ => cfg.username.clone(),
        };
        let password = cfg.password.clone().unwrap_or_default();
        rt.block_on(client.share_connect(&unc, &user, password))
            .map_err(|e| {
                LibError::SourceUnavailable(format!(
                    "smb connect //{}/{}: {e}",
                    cfg.host, cfg.share
                ))
            })?;

        Ok(SmbSource {
            cfg,
            rt,
            client,
            unc,
            scratch,
        })
    }

    /// UNC path for a source-relative path (SMB uses `\`-separated paths under the share).
    fn unc_for(&self, rel: &str) -> UncPath {
        let joined = join_smb(&self.cfg.base_path, rel);
        if joined.is_empty() {
            self.unc.clone()
        } else {
            self.unc.clone().with_path(&joined)
        }
    }
}

impl FileSource for SmbSource {
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        let mut stack: Vec<String> = vec![String::new()];
        while let Some(rel_dir) = stack.pop() {
            let unc = self.unc_for(&rel_dir);
            let listing = self.rt.block_on(list_dir(&self.client, &unc));
            let entries = match listing {
                Ok(e) => e,
                Err(e) => {
                    if !sink(Err(e)) {
                        return Ok(());
                    }
                    continue;
                }
            };
            for (name, is_dir, size, mtime_ms) in entries {
                if name == "." || name == ".." {
                    continue;
                }
                let child_rel = if rel_dir.is_empty() {
                    name
                } else {
                    format!("{rel_dir}/{name}")
                };
                if is_dir {
                    stack.push(child_rel);
                    continue;
                }
                let fe = FileEntry {
                    rel_path: child_rel,
                    size,
                    modified_ms: mtime_ms,
                };
                if !sink(Ok(fe)) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Download to a scratch file, **streaming** (issue #87). The read loop already worked in
    /// blocks; it used to accumulate them into one `Vec<u8>` that was then written out, so a large
    /// asset sat in memory twice over. Now each block goes straight to disk.
    fn fetch(&self, rel_path: &str) -> Result<Fetched, LibError> {
        let rel_path = guard_rel_path(rel_path)?;
        let unc = self.unc_for(&rel_path);
        let mut sink = crate::temp_sink(&rel_path, &self.scratch)?;
        self.rt
            .block_on(read_file_into(&self.client, &unc, &mut sink))?;
        std::io::Write::flush(&mut sink).ok();
        Ok(Fetched::Temp(sink))
    }

    // ── write side (issue #80 slice 7) ───────────────────────────────────────

    /// The *kind* can be written to; whether this credential can is settled by [`Self::put`]. Same
    /// reasoning as the SFTP backend — see [`crate::writable_without_handshake`].
    fn writable(&self) -> bool {
        true
    }

    fn mkdir(&self, rel_path: &str) -> Result<(), LibError> {
        let rel = crate::safe_name::check_rel_path(rel_path)?;
        // `OpenIf` is mkdir -p for one component: open it if it is there, create it if it is not.
        // Walking the components gives the whole chain, since SMB creates one level at a time.
        let mut acc = String::new();
        for seg in rel.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            let unc = self.unc_for(&acc);
            let args = FileCreateArgs {
                disposition: CreateDisposition::OpenIf,
                options: CreateOptions::new().with_directory_file(true),
                desired_access: FileAccessMask::new().with_generic_read(true),
                attributes: FileAttributes::new().with_directory(true),
            };
            // The handle must be closed *inside* the runtime. `Resource`'s async `Drop` schedules
            // the close with `tokio::task::spawn`, so letting it fall out of `block_on` and drop on
            // the calling thread panics with "there is no reactor running" — and closing it also
            // stops leaking one server-side handle per directory level.
            self.rt.block_on(async {
                let opened =
                    self.client.create_file(&unc, &args).await.map_err(|e| {
                        LibError::SourceUnavailable(format!("smb mkdir {acc}: {e}"))
                    })?;
                close_resource(opened).await;
                Ok::<(), LibError>(())
            })?;
        }
        Ok(())
    }

    /// Create a **new** remote file, streaming from `bytes`.
    ///
    /// `CreateDisposition::Create` is SMB2's `FILE_CREATE` (MS-SMB2 §2.2.13): the *server* fails the
    /// open if the name exists. That is a stronger guarantee than SFTP's, which leans on a rename
    /// the server is merely specified to refuse — here refusing is the operation's own semantics,
    /// so no flag combination and no race can turn this into an overwrite.
    ///
    /// **What it does not get is the temp-then-rename shape.** The `smb` crate exposes no rename at
    /// all, so the bytes must land at their final name as they arrive. The create-only invariant is
    /// untouched by that — nothing is ever replaced — but a transfer that dies leaves a *short* file
    /// at the real name rather than a collectable `.part`. The failure path below deletes it, and
    /// that covers every case except the process itself being killed. A later scan would then
    /// catalogue a truncated asset. This is the one place where a backend is materially weaker than
    /// the local one, it is the crate's limitation rather than a choice made here, and it is worth
    /// revisiting if `smb` ever grows `SET_INFO`/`FileRenameInformation`.
    ///
    /// **Untested.** There is no in-process SMB server to run this against, the way `tests/support`
    /// gives SFTP a real one — so unlike every other backend this path has never executed against a
    /// server. Treat it accordingly.
    fn put(&self, rel_path: &str, bytes: &mut dyn std::io::Read) -> Result<(), LibError> {
        let rel = crate::safe_name::check_rel_path(rel_path)?;
        let unc = self.unc_for(&rel);

        self.rt.block_on(async {
            let args = FileCreateArgs::make_create_new(FileAttributes::new(), CreateOptions::new());
            let resource = self.client.create_file(&unc, &args).await.map_err(|e| {
                // The server refusing because the name is taken is a collision, not a transport
                // failure — `Suffix`/`Skip` key off that distinction, so flattening it into
                // `SourceUnavailable` would silently disable both.
                if is_name_collision(&e) {
                    LibError::Conflict(format!("{rel} already exists"))
                } else {
                    LibError::SourceUnavailable(format!("smb create {rel}: {e}"))
                }
            })?;
            let file = match resource {
                Resource::File(f) => f,
                _ => {
                    return Err(LibError::SourceUnavailable(format!(
                        "smb {rel} is not a file"
                    )))
                }
            };

            // Stream: peak memory is one buffer whatever the file's size, matching `fetch`.
            let mut buf = vec![0u8; crate::FETCH_CHUNK];
            let mut pos: u64 = 0;
            let outcome = async {
                loop {
                    let n = bytes
                        .read(&mut buf)
                        .map_err(|e| LibError::Internal(format!("read staged bytes: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    let mut off = 0usize;
                    // A short write is legal; keep going until the block is placed.
                    while off < n {
                        let w = file
                            .write_block(&buf[off..n], pos + off as u64, None)
                            .await
                            .map_err(|e| {
                                LibError::SourceUnavailable(format!("smb write {rel}: {e}"))
                            })?;
                        if w == 0 {
                            return Err(LibError::SourceUnavailable(format!(
                                "smb write {rel}: server accepted no bytes"
                            )));
                        }
                        off += w;
                    }
                    pos += n as u64;
                }
                Ok::<(), LibError>(())
            }
            .await;
            let _ = file.close().await;

            if let Err(e) = outcome {
                // Best-effort removal of the short file, via the one delete SMB gives us: reopen it
                // asking for delete-on-close, then close. Leaving it would be worse than the failed
                // upload — a later scan would catalogue a truncated asset as a real one.
                let del = FileCreateArgs {
                    disposition: CreateDisposition::Open,
                    options: CreateOptions::new().with_delete_on_close(true),
                    desired_access: FileAccessMask::new().with_delete(true),
                    attributes: FileAttributes::new(),
                };
                if let Ok(Resource::File(f)) = self.client.create_file(&unc, &del).await {
                    let _ = f.close().await;
                }
                return Err(e);
            }
            Ok(())
        })
    }
}

/// Did the server refuse a create because the name is already taken?
///
/// Matched on the **NT status code**, not the message. The crate's `Display` renders
/// `0xC0000035` as the prose `"Object Name Collision (0xc0000035)"`, so a substring test for the
/// symbolic `OBJECT_NAME_COLLISION` never fires — and a collision that reads as a transport failure
/// takes `Suffix` and `Skip` down with it, silently, since both key off `Conflict`.
fn is_name_collision(e: &smb::Error) -> bool {
    use smb::Status;
    matches!(
        e,
        smb::Error::ReceivedErrorMessage(Status::U32_OBJECT_NAME_COLLISION, _)
            | smb::Error::UnexpectedMessageStatus(Status::U32_OBJECT_NAME_COLLISION)
    )
}

/// Close an opened handle inside the runtime.
///
/// `Resource`'s async `Drop` schedules its close with `tokio::task::spawn`, which panics if the
/// value is dropped after `block_on` has returned and the runtime context has left TLS. Every
/// handle this module opens is therefore closed explicitly, on the runtime, before that can happen.
async fn close_resource(resource: Resource) {
    match resource {
        Resource::File(f) => {
            let _ = f.close().await;
        }
        Resource::Directory(d) => {
            let _ = d.close().await;
        }
        other => drop(other),
    }
}

/// List a directory's immediate children: `(name, is_dir, size, modified_ms)`.
async fn list_dir(
    client: &Client,
    unc: &UncPath,
) -> Result<Vec<(String, bool, u64, Option<i64>)>, LibError> {
    let args = FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
    let resource = client
        .create_file(unc, &args)
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("smb open dir: {e}")))?;
    let dir = match resource {
        Resource::Directory(d) => Arc::new(d),
        _ => {
            return Err(LibError::SourceUnavailable(
                "smb path is not a directory".into(),
            ))
        }
    };
    let mut stream = Directory::query::<FileDirectoryInformation>(&dir, "*")
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("smb query dir: {e}")))?;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| LibError::SourceUnavailable(format!("smb dir entry: {e}")))?;
        let name = info.file_name.to_string();
        let is_dir = info.file_attributes.directory();
        let mtime = if info.last_write_time.is_zero() {
            None
        } else {
            crate::system_time_ms(SystemTime::from(info.last_write_time))
        };
        out.push((name, is_dir, info.end_of_file, mtime));
    }
    Ok(out)
}

/// Copy a remote file block-by-block into `out`, never holding more than one block in memory.
async fn read_file_into<W: std::io::Write>(
    client: &Client,
    unc: &UncPath,
    out: &mut W,
) -> Result<(), LibError> {
    let args = FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
    let resource = client
        .create_file(unc, &args)
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("smb open file: {e}")))?;
    let file = match resource {
        Resource::File(f) => f,
        _ => return Err(LibError::SourceUnavailable("smb path is not a file".into())),
    };
    let mut buf = vec![0u8; crate::FETCH_CHUNK];
    let mut pos: u64 = 0;
    loop {
        let n = file
            .read_block(&mut buf, pos, None, false)
            .await
            .map_err(|e| LibError::SourceUnavailable(format!("smb read: {e}")))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| LibError::Internal(format!("scratch write: {e}")))?;
        pos += n as u64;
    }
    let _ = file.close().await;
    Ok(())
}

/// Join a base dir and a source-relative path into a `\`-separated share path.
fn join_smb(base: &str, rel: &str) -> String {
    let base = base.trim_matches('/').replace('/', "\\");
    let rel = rel.trim_matches('/').replace('/', "\\");
    match (base.is_empty(), rel.is_empty()) {
        (true, true) => String::new(),
        (true, false) => rel,
        (false, true) => base,
        (false, false) => format!("{base}\\{rel}"),
    }
}
