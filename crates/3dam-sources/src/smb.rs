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

use crate::{guard_rel_path, temp_from_bytes, Fetched, FileEntry, FileSource, SmbConfig};
use dam_api::LibError;
use futures::StreamExt;
use smb::resource::{Directory, Resource};
use smb::{
    Client, ClientConfig, FileAccessMask, FileCreateArgs, FileDirectoryInformation, UncPath,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::runtime::Runtime;

const READ_CHUNK: usize = 256 * 1024;

pub struct SmbSource {
    cfg: SmbConfig,
    rt: Runtime,
    client: Client,
    unc: UncPath,
}

impl SmbSource {
    pub fn connect(cfg: SmbConfig) -> Result<SmbSource, LibError> {
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

    fn fetch(&self, rel_path: &str) -> Result<Fetched, LibError> {
        guard_rel_path(rel_path)?;
        let unc = self.unc_for(rel_path);
        let bytes = self.rt.block_on(read_file(&self.client, &unc))?;
        temp_from_bytes(rel_path, &bytes)
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

/// Read a whole remote file into memory (needed for the content hash anyway — §2.2).
async fn read_file(client: &Client, unc: &UncPath) -> Result<Vec<u8>, LibError> {
    let args = FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
    let resource = client
        .create_file(unc, &args)
        .await
        .map_err(|e| LibError::SourceUnavailable(format!("smb open file: {e}")))?;
    let file = match resource {
        Resource::File(f) => f,
        _ => return Err(LibError::SourceUnavailable("smb path is not a file".into())),
    };
    let mut out = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut pos: u64 = 0;
    loop {
        let n = file
            .read_block(&mut buf, pos, None, false)
            .await
            .map_err(|e| LibError::SourceUnavailable(format!("smb read: {e}")))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        pos += n as u64;
    }
    let _ = file.close().await;
    Ok(out)
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
