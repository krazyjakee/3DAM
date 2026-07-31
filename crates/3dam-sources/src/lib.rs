//! `dam-sources` — the `Source` trait and its file-source implementations (tech-spec 07).
//!
//! A **file source** yields raw bytes the engine hashes, detects, and catalogues locally (§1, §2).
//! Three kinds ship here behind one trait: the **local filesystem**, **SFTP** (`russh`), and
//! **SMB2/3** (`smb`). The engine above them is identical — the only divergence is connection setup
//! and how an entry's bytes reach a locally-readable path ([`Fetched`]). The federated peer source
//! (catalog rows, not bytes) is a separate surface that lands with phase 6.
//!
//! **Byte access is uniform.** The media handlers ([`dam_media`]) are path-based, so every source
//! resolves an entry to a real local path via [`FileSource::fetch`]: local sources hand back the
//! file in place (no copy); remote sources download it to a temp file whose suffix preserves the
//! logical extension, so detection and the cheap-tier probes behave exactly as they do locally.

use dam_api::LibError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod safe_name;
#[cfg(feature = "sftp")]
mod sftp;
#[cfg(feature = "smb")]
mod smb;

/// One file discovered by a file source. Root-relative; no absolute path is carried, because a
/// remote entry has none on this host — [`FileSource::fetch`] resolves bytes to a local path.
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Path relative to the source root (stored as `asset.path`), normalised to `/`.
    pub rel_path: String,
    pub size: u64,
    /// Source mtime in epoch-millis, when the backend reports one. The cheap delta-scan change
    /// token (mtime+size) — never a content hash (§2).
    pub modified_ms: Option<i64>,
}

/// A locally-readable handle to an entry's bytes. For a local source it borrows the real file in
/// place; for a remote source it is a downloaded temp file, removed on drop. Either way `path()`
/// hands the (path-based) media handlers something they can open.
pub enum Fetched {
    /// The source file itself (local FS): zero-copy.
    InPlace(PathBuf),
    /// A downloaded copy (SFTP/SMB), deleted when this drops. Suffixed with the logical extension
    /// so extension-keyed detection (tech-spec 04 §7) still works.
    Temp(tempfile::NamedTempFile),
}

impl Fetched {
    pub fn path(&self) -> &Path {
        match self {
            Fetched::InPlace(p) => p.as_path(),
            Fetched::Temp(f) => f.path(),
        }
    }
}

/// A file source yields raw file entries the engine processes locally (tech-spec 07 §2).
pub trait FileSource: Send + Sync {
    /// Walk the source tree, invoking `sink` per discovered file. Fail-soft: a per-entry error is
    /// reported to `sink` as `Err` and iteration continues. `sink` returns `false` to stop early
    /// (cooperative cancellation).
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError>;

    /// Resolve an entry's bytes to a local path for the media handlers. Local → in place; remote →
    /// a downloaded temp file. Guards against `..` traversal out of the source root.
    fn fetch(&self, rel_path: &str) -> Result<Fetched, LibError>;

    // ── write side (issue #80) ───────────────────────────────────────────────
    //
    // Upload is the *one* sanctioned path that writes into a source tree, and it is create-only
    // (tech-spec 08 §5.1). The three methods below are deliberately the whole of it: there is no
    // `delete`, no `rename`, and no overwrite parameter, so "3DAM modified my file" is not
    // expressible through this trait at all.

    /// Can this source be written to *right now*?
    ///
    /// Defaults to `false`, so a new backend is read-only until someone opts in deliberately — the
    /// safe direction for a trait whose other methods only ever read. Implementations should probe
    /// real writability (a read-only mount, a share the credential cannot write) rather than
    /// trusting the source kind, because the UI uses this to decide what to offer as a destination
    /// *before* the user picks files rather than failing after they drop two hundred of them.
    fn writable(&self) -> bool {
        false
    }

    /// Create a directory and any missing parents under the source root.
    ///
    /// Idempotent: an existing directory is success, not a conflict.
    fn mkdir(&self, _rel_path: &str) -> Result<(), LibError> {
        Err(LibError::Unsupported(
            "this source kind is read-only".into(),
        ))
    }

    /// Create a **new** file under the source root, streaming from `bytes`.
    ///
    /// **Create-only, and that is load-bearing**: an existing path is an error, never an overwrite.
    /// The caller resolves collisions (fail / suffix / skip) *before* calling — there is no
    /// overwrite flag to get wrong, which is what keeps the non-destructive invariant structural
    /// rather than policy-dependent.
    ///
    /// Implementations write to a temp path on the destination filesystem, `fsync`, then atomically
    /// rename into place, so a crash leaves a collectable partial file rather than a half-written
    /// asset that a watcher might ingest mid-write (tech-spec 08 §5.2).
    fn put(&self, _rel_path: &str, _bytes: &mut dyn std::io::Read) -> Result<(), LibError> {
        Err(LibError::Unsupported(
            "this source kind is read-only".into(),
        ))
    }
}

// ── connection model (persisted per source, tech-spec 02 §3 `source.connection`) ────────────────

/// Everything needed to (re)build a file source, including its secret. Serialised into the source
/// record's `connection` column; **never** returned to a client (the sanitised [`display_uri`] is).
///
/// [`display_uri`]: SourceConnection::display_uri
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConnection {
    LocalFs { root: String },
    Sftp(SftpConfig),
    Smb(SmbConfig),
    Federated(FederatedConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SftpConfig {
    pub host: String,
    #[serde(default = "default_sftp_port")]
    pub port: u16,
    pub username: String,
    /// Source-root-relative base directory on the remote host (absolute or `~`-relative per server).
    #[serde(default = "default_remote_root")]
    pub base_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Path to a private key file on *this* host (v1 secret handling; tech-spec 10 owns a keyring later).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SmbConfig {
    pub host: String,
    #[serde(default = "default_smb_port")]
    pub port: u16,
    pub share: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_path: String,
    #[serde(default)]
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

/// A peer 3DAM server (phase 6, issue #39). Yields **catalog rows, not bytes** — it satisfies the
/// source *record* model (persisted connection + secret) but is deliberately not a [`FileSource`]:
/// the query fan-out in `dam-core` talks to it over the peer's HTTP read API, and the only byte
/// transfer it ever does is fetching remote-owned previews (tech-spec 07 §4).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FederatedConfig {
    /// Peer base endpoint, `http(s)://host:port` (no trailing slash, no path).
    pub endpoint: String,
    /// Bearer token for the peer, when its auth mode requires one. Held server-side in the
    /// connection blob like every other source secret; never returned to clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

fn default_sftp_port() -> u16 {
    22
}
fn default_smb_port() -> u16 {
    445
}
fn default_remote_root() -> String {
    ".".to_string()
}

/// Optional connection inputs collected from the CLI/API alongside the URI (tech-spec 07 §3.2).
/// Values here override anything parsed from the URI's userinfo.
#[derive(Clone, Debug, Default)]
pub struct ConnOptions {
    pub username: Option<String>,
    pub password: Option<String>,
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
    pub domain: Option<String>,
    pub port: Option<u16>,
}

impl SourceConnection {
    /// The `source.kind` token this connection maps to.
    pub fn kind(&self) -> &'static str {
        match self {
            SourceConnection::LocalFs { .. } => "local_fs",
            SourceConnection::Sftp(_) => "sftp",
            SourceConnection::Smb(_) => "smb",
            SourceConnection::Federated(_) => "federated",
        }
    }

    /// A stable, **secret-free** URI for display/listing (never carries a password/key).
    pub fn display_uri(&self) -> String {
        match self {
            SourceConnection::LocalFs { root } => root.clone(),
            SourceConnection::Sftp(c) => {
                let base = c.base_path.trim_start_matches('/');
                format!("sftp://{}@{}:{}/{}", c.username, c.host, c.port, base)
            }
            SourceConnection::Smb(c) => {
                let base = if c.base_path.is_empty() {
                    String::new()
                } else {
                    format!("/{}", c.base_path.trim_start_matches('/'))
                };
                format!("smb://{}/{}{}", c.host, c.share.trim_matches('/'), base)
            }
            SourceConnection::Federated(c) => c.endpoint.clone(),
        }
    }

    /// Parse a user-supplied URI + options into a connection. `kind` is the requested source kind
    /// (`local_fs`|`sftp`|`smb`); the URI scheme, when present, must agree with it.
    pub fn parse(kind: &str, uri: &str, opts: &ConnOptions) -> Result<SourceConnection, LibError> {
        match kind {
            "local_fs" => Ok(SourceConnection::LocalFs {
                root: strip_scheme(uri, "file").to_string(),
            }),
            "sftp" => parse_sftp(uri, opts),
            "smb" => parse_smb(uri, opts),
            "federated" => parse_federated(uri, opts),
            other => Err(LibError::Unsupported(format!(
                "source kind {other:?} is not a file source"
            ))),
        }
    }
}

/// Build the concrete backend for a connection. Remote kinds require the matching crate feature.
/// `scratch` is where a remote backend materialises downloaded bytes (issue #87). It is a required
/// positional argument rather than an option with a default, for the same reason `Visibility` is:
/// the sensible-looking default (`std::env::temp_dir()`) is the wrong one on most Linux hosts, and a
/// caller that inherited it by omission would reintroduce the bug in silence. Local sources ignore
/// it — they never copy anything.
pub fn open_source(
    conn: &SourceConnection,
    scratch: &Path,
) -> Result<Box<dyn FileSource>, LibError> {
    let _ = scratch; // only the remote backends materialise bytes
    match conn {
        SourceConnection::LocalFs { root } => Ok(Box::new(LocalFsSource::new(root))),
        #[cfg(feature = "sftp")]
        SourceConnection::Sftp(cfg) => Ok(Box::new(sftp::SftpSource::connect(
            cfg.clone(),
            scratch.to_path_buf(),
        )?)),
        #[cfg(not(feature = "sftp"))]
        SourceConnection::Sftp(_) => Err(LibError::Unsupported(
            "SFTP support is not compiled into this build".into(),
        )),
        #[cfg(feature = "smb")]
        SourceConnection::Smb(cfg) => Ok(Box::new(smb::SmbSource::connect(
            cfg.clone(),
            scratch.to_path_buf(),
        )?)),
        #[cfg(not(feature = "smb"))]
        SourceConnection::Smb(_) => Err(LibError::Unsupported(
            "SMB support is not compiled into this build".into(),
        )),
        // Structurally cannot leak byte I/O: a peer yields catalog rows, never a file reader
        // (tech-spec 07 §1 "federate, don't reprocess"). The fan-out engine owns this kind.
        SourceConnection::Federated(_) => Err(LibError::Unsupported(
            "a federated peer yields catalog rows, not bytes".into(),
        )),
    }
}

/// Can this source accept an upload, answered **without a network handshake** (issue #80)?
///
/// This exists so "can it be written to" has exactly one definition. The listing path needs the
/// answer for every source on every render, and `open_source` on an SFTP/SMB connection is a full
/// login — a sidebar would become N logins against hosts that may be asleep. So remote kinds answer
/// statically here rather than being opened.
///
/// That static answer is *currently correct*, not merely cheap: neither remote backend implements
/// the write side yet, so both inherit `FileSource::writable`'s `false`. **When slice 7 lands SFTP
/// and SMB writes, this function is the one that must learn about it** — otherwise uploads would
/// start succeeding through `run_upload` (which asks the opened source) while the picker kept
/// hiding those destinations, and nothing would fail to compile to say so.
pub fn writable_without_handshake(conn: &SourceConnection) -> bool {
    match conn {
        // The only kind whose answer varies, and the only one cheap enough to ask for real.
        SourceConnection::LocalFs { root } => LocalFsSource::new(root).writable(),
        SourceConnection::Sftp(_) | SourceConnection::Smb(_) => false,
        // A peer's library is never a destination — it yields catalog rows, not a writable tree.
        SourceConnection::Federated(_) => false,
    }
}

// ── URI parsing helpers ─────────────────────────────────────────────────────────────────────────

fn strip_scheme<'a>(uri: &'a str, scheme: &str) -> &'a str {
    uri.strip_prefix(&format!("{scheme}://")).unwrap_or(uri)
}

/// `sftp://[user[:pass]@]host[:port]/base/path`. Options override userinfo.
fn parse_sftp(uri: &str, opts: &ConnOptions) -> Result<SourceConnection, LibError> {
    let rest = uri
        .strip_prefix("sftp://")
        .ok_or_else(|| LibError::BadRequest("sftp source uri must start with sftp://".into()))?;
    let (authority, path) = split_once_or(rest, '/', ("", ""));
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let (uri_user, uri_pass) = match userinfo {
        Some(ui) => match ui.split_once(':') {
            Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
            None => (Some(ui.to_string()), None),
        },
        None => (None, None),
    };
    let (host, uri_port) = split_host_port(hostport);
    if host.is_empty() {
        return Err(LibError::BadRequest(
            "sftp source uri is missing a host".into(),
        ));
    }
    let username = opts
        .username
        .clone()
        .or(uri_user)
        .ok_or_else(|| LibError::BadRequest("sftp source requires a username".into()))?;
    Ok(SourceConnection::Sftp(SftpConfig {
        host: host.to_string(),
        port: opts.port.or(uri_port).unwrap_or(22),
        username,
        base_path: if path.is_empty() {
            default_remote_root()
        } else {
            format!("/{}", path.trim_start_matches('/'))
        },
        password: opts.password.clone().or(uri_pass),
        private_key: opts.private_key.clone(),
        passphrase: opts.passphrase.clone(),
    }))
}

/// `smb://host[:port]/share[/base/path]`. Credentials come from options (or the `domain` field).
fn parse_smb(uri: &str, opts: &ConnOptions) -> Result<SourceConnection, LibError> {
    let rest = uri
        .strip_prefix("smb://")
        .ok_or_else(|| LibError::BadRequest("smb source uri must start with smb://".into()))?;
    let (authority, path) = split_once_or(rest, '/', ("", ""));
    let (host, uri_port) = split_host_port(authority);
    if host.is_empty() {
        return Err(LibError::BadRequest(
            "smb source uri is missing a host".into(),
        ));
    }
    let (share, base) = split_once_or(path, '/', (path, ""));
    if share.is_empty() {
        return Err(LibError::BadRequest(
            "smb source uri must include a share: smb://host/share/…".into(),
        ));
    }
    Ok(SourceConnection::Smb(SmbConfig {
        host: host.to_string(),
        port: opts.port.or(uri_port).unwrap_or(445),
        share: share.to_string(),
        base_path: base.trim_matches('/').to_string(),
        username: opts.username.clone().unwrap_or_default(),
        password: opts.password.clone(),
        domain: opts.domain.clone(),
    }))
}

/// `3dam://host[:port][/]` (plain HTTP), `3dams://…` (HTTPS), or a literal `http(s)://…` endpoint.
/// The bearer token rides in `opts.password` — the same slot every other network source's secret
/// uses, so the CLI/API surface stays one shape.
fn parse_federated(uri: &str, opts: &ConnOptions) -> Result<SourceConnection, LibError> {
    let endpoint = if let Some(rest) = uri.strip_prefix("3dam://") {
        format!("http://{rest}")
    } else if let Some(rest) = uri.strip_prefix("3dams://") {
        format!("https://{rest}")
    } else if uri.starts_with("http://") || uri.starts_with("https://") {
        uri.to_string()
    } else {
        return Err(LibError::BadRequest(
            "federated source uri must be 3dam://host:port or http(s)://host:port".into(),
        ));
    };
    let endpoint = endpoint.trim_end_matches('/').to_string();
    let host = endpoint.split("://").nth(1).unwrap_or("");
    if host.is_empty() {
        return Err(LibError::BadRequest(
            "federated source uri is missing a host".into(),
        ));
    }
    Ok(SourceConnection::Federated(FederatedConfig {
        endpoint,
        token: opts.password.clone(),
    }))
}

fn split_once_or<'a>(s: &'a str, sep: char, default: (&'a str, &'a str)) -> (&'a str, &'a str) {
    match s.split_once(sep) {
        Some((a, b)) => (a, b),
        None => {
            if default.0.is_empty() && default.1.is_empty() {
                (s, "")
            } else {
                default
            }
        }
    }
}

fn split_host_port(s: &str) -> (&str, Option<u16>) {
    match s.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()),
        None => (s, None),
    }
}

/// Reject a relative path that escapes its source root. Shared by every backend's `fetch`.
pub(crate) fn guard_rel_path(rel_path: &str) -> Result<(), LibError> {
    if Path::new(rel_path)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(LibError::BadRequest(
            "asset path escapes its source root".into(),
        ));
    }
    Ok(())
}

/// Transfer buffer for a streaming remote fetch. This is now the *whole* memory cost of downloading
/// an asset, however large it is (issue #87) — 256 KiB is big enough to keep a network round-trip
/// amortised and small enough that a pool of concurrent fetches is still nothing.
pub(crate) const FETCH_CHUNK: usize = 256 * 1024;

/// Filename prefix every remote download carries. Also what [`clean_scratch`] matches on, so the
/// two must agree — a rename here silently orphans whatever a previous build left behind.
pub(crate) const SCRATCH_PREFIX: &str = "3dam-remote-";

/// Filename prefix for an **inbound upload** staged in scratch (issue #80).
///
/// Public because the staging happens in the server's transport layer, not here — but it lives
/// beside [`SCRATCH_PREFIX`] and is swept by the same [`clean_scratch`] for the same reason. A
/// prefix the sweep does not know about is worse than no sweep: an upload killed at 3.5 GB of a
/// 4 GB video leaves that file in the data dir forever, and a leading dot would hide it from `ls`
/// as well.
pub const UPLOAD_SCRATCH_PREFIX: &str = "3dam-upload-";

/// Open a temp file for a remote download, **inside the engine's scratch directory**.
///
/// The directory matters (issue #87): `tempfile`'s default is `std::env::temp_dir()`, which on most
/// Linux distributions is a tmpfs — RAM backed by swap. That was harmless when this seam only
/// carried small files, and stopped being harmless once video became a media type and analyse and
/// convert started fetching remote bytes too. `scratch` is a directory under the user's data dir,
/// which they chose and which is real disk.
///
/// Falls back to the OS temp dir if `scratch` is unusable — a read-only or missing data dir should
/// degrade to the old behaviour, not fail the fetch.
///
/// The suffix preserves the entry's logical extension so extension-keyed detection (tech-spec 04 §7)
/// still works on a file whose stem is random.
#[cfg(any(feature = "sftp", feature = "smb"))]
pub(crate) fn temp_sink(
    rel_path: &str,
    scratch: &Path,
) -> Result<tempfile::NamedTempFile, LibError> {
    let suffix = Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let build = || {
        tempfile::Builder::new()
            .prefix(SCRATCH_PREFIX)
            .suffix(&suffix)
            .tempfile_in(scratch)
    };
    match build() {
        Ok(f) => Ok(f),
        Err(e) => {
            tracing::warn!(
                scratch = %scratch.display(),
                error = %e,
                "scratch dir unusable; falling back to the OS temp dir"
            );
            tempfile::Builder::new()
                .prefix(SCRATCH_PREFIX)
                .suffix(&suffix)
                .tempfile()
                .map_err(|e| LibError::Internal(format!("temp file: {e}")))
        }
    }
}

/// Delete stale remote downloads left in `scratch` by a previous run.
///
/// [`Fetched::Temp`] removes its file on drop, which covers every normal path — but not a kill -9,
/// a panic-abort, or a power cut mid-fetch. Without this, a crash during a large remote pass leaves
/// multi-gigabyte files sitting in the data dir with nothing that will ever collect them.
///
/// Deliberately unfiltered by age: this runs at engine open, and any matching file present at that
/// moment belongs to a process that is no longer running. (Two engines sharing one data dir would
/// race here — but they already race on `library.db`'s write lock, so that is not a new constraint.)
/// Best-effort: a scratch directory we cannot read is not worth failing to start over.
///
/// Sweeps **both** things that stage bytes in scratch: remote downloads and inbound uploads. An
/// upload can be larger than any download, since it is bounded only by `[upload] max_file_mb`.
pub fn clean_scratch(scratch: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(scratch) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let orphan = name.starts_with(SCRATCH_PREFIX) || name.starts_with(UPLOAD_SCRATCH_PREFIX);
        if orphan && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(removed, "cleared orphaned scratch files");
    }
    removed
}

// ── local filesystem source ───────────────────────────────────────────────────────────────────

/// A local directory tree walked with `walkdir`.
pub struct LocalFsSource {
    root: PathBuf,
}

impl LocalFsSource {
    pub fn new(root: impl Into<PathBuf>) -> LocalFsSource {
        LocalFsSource { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl FileSource for LocalFsSource {
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        if !self.root.exists() {
            return Err(LibError::SourceUnavailable(format!(
                "path does not exist: {}",
                self.root.display()
            )));
        }
        for entry in walkdir::WalkDir::new(&self.root).follow_links(false) {
            let cont = match entry {
                Ok(de) if de.file_type().is_file() => {
                    let abs = de.path().to_path_buf();
                    let rel = abs
                        .strip_prefix(&self.root)
                        .unwrap_or(&abs)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let (size, modified_ms) = match de.metadata() {
                        Ok(m) => (m.len(), m.modified().ok().and_then(system_time_ms)),
                        Err(_) => (0, None),
                    };
                    sink(Ok(FileEntry {
                        rel_path: rel,
                        size,
                        modified_ms,
                    }))
                }
                Ok(_) => true, // directories/symlinks: skip, keep going
                Err(e) => sink(Err(LibError::Internal(e.to_string()))),
            };
            if !cont {
                break; // sink asked to stop (cancellation)
            }
        }
        Ok(())
    }

    fn fetch(&self, rel_path: &str) -> Result<Fetched, LibError> {
        guard_rel_path(rel_path)?;
        let abs = self.root.join(rel_path);
        if !abs.is_file() {
            return Err(LibError::NotFound(format!("file gone: {rel_path}")));
        }
        Ok(Fetched::InPlace(abs))
    }

    /// Probed, not assumed (issue #80). A local source can sit on a read-only mount, a full disk,
    /// or a directory the server process does not own — none of which the *kind* tells you. The
    /// answer feeds a destination picker, so being wrong here means offering the user a folder they
    /// cannot write to.
    ///
    /// **This creates and removes a file rather than reading the mode bits.** `Permissions::
    /// readonly()` answers "is every write bit clear", which is not the same question and misses
    /// all three cases above: an `ro` mount still reports mode 0755, a root-owned 0755 directory
    /// looks writable to an unprivileged process, and on Windows the read-only attribute is ignored
    /// for directories entirely — so it would answer "writable" for essentially every local root.
    /// std's own documentation warns it "cannot be relied upon to predict whether attempts to …
    /// write the file will actually succeed". The only honest probe is to try, and trying costs
    /// about what a `stat` does.
    fn writable(&self) -> bool {
        tempfile::Builder::new()
            .prefix(".3dam-writable-")
            .tempfile_in(&self.root)
            .is_ok() // NamedTempFile removes itself on drop.
    }

    fn mkdir(&self, rel_path: &str) -> Result<(), LibError> {
        let rel = safe_name::check_rel_path(rel_path)?;
        let abs = self.resolve_within(&rel)?;
        std::fs::create_dir_all(&abs).map_err(|e| LibError::Internal(format!("create {rel}: {e}")))
    }

    fn put(&self, rel_path: &str, bytes: &mut dyn std::io::Read) -> Result<(), LibError> {
        use std::io::Write;
        let rel = safe_name::check_rel_path(rel_path)?;
        let abs = self.resolve_within(&rel)?;

        // Create-only. Checked here *and* enforced by the rename below, because this check alone
        // is a TOCTOU window — see the atomic-rename comment.
        //
        // `Conflict`, not `BadRequest`: "the name is taken" is the one failure a caller *routinely*
        // recovers from (retry under a suffix, or report a skip), so it has to be distinguishable
        // from "that name is malformed" without matching on message text. It is also the honest
        // status — 409, not 400, since the request was well-formed and the world disagreed.
        if abs.exists() {
            return Err(LibError::Conflict(format!("{rel} already exists")));
        }
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LibError::Internal(format!("create parent of {rel}: {e}")))?;
        }

        // Temp file in the *destination* directory, so the rename below is same-filesystem and
        // therefore atomic. A temp in /tmp could land on another mount and degrade to a copy,
        // which is exactly the half-written-file window this is here to close (tech-spec 08 §5.2).
        let dir = abs.parent().unwrap_or(&self.root);
        let mut tmp = tempfile::Builder::new()
            .prefix(".3dam-upload-")
            .tempfile_in(dir)
            .map_err(|e| LibError::Internal(format!("temp file in {}: {e}", dir.display())))?;
        std::io::copy(bytes, &mut tmp)
            .map_err(|e| LibError::Internal(format!("write {rel}: {e}")))?;
        tmp.flush()
            .map_err(|e| LibError::Internal(format!("flush {rel}: {e}")))?;
        // fsync before the rename: a rename is atomic with respect to *ordering*, not durability,
        // so without this a power cut can leave the name pointing at unwritten blocks.
        tmp.as_file()
            .sync_all()
            .map_err(|e| LibError::Internal(format!("fsync {rel}: {e}")))?;

        // `persist_noclobber` fails rather than replacing, which closes the TOCTOU window the
        // `exists()` check above leaves open: between that check and this call another writer
        // could have created the path, and a plain rename would silently destroy their file.
        tmp.persist_noclobber(&abs).map_err(|e| {
            if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                LibError::Conflict(format!("{rel} already exists"))
            } else {
                LibError::Internal(format!("commit {rel}: {}", e.error))
            }
        })?;
        Ok(())
    }
}

impl LocalFsSource {
    /// Join a **validated** relative path onto the root and prove the result is still inside it
    /// after symlink resolution.
    ///
    /// The lexical guard in `safe_name` cannot see a symlink: if `Textures/` is a link to `/etc`,
    /// then `Textures/passwd` passes every string check and still escapes. So the deepest existing
    /// ancestor is canonicalised and compared against the canonical root — checking the ancestor
    /// rather than the target because the target is a file we are about to *create* and so does not
    /// exist yet.
    fn resolve_within(&self, rel: &str) -> Result<PathBuf, LibError> {
        let root = self
            .root
            .canonicalize()
            .map_err(|e| LibError::SourceUnavailable(format!("source root: {e}")))?;
        let abs = root.join(rel);

        let mut probe = abs.as_path();
        let existing = loop {
            if probe.exists() {
                break probe;
            }
            match probe.parent() {
                // Walked above the root without finding anything that exists: the root itself was
                // canonicalised above, so this cannot be inside it.
                Some(p) if p.starts_with(&root) => probe = p,
                _ => break root.as_path(),
            }
        };
        let real = existing
            .canonicalize()
            .map_err(|e| LibError::Internal(format!("resolve {rel}: {e}")))?;
        if !real.starts_with(&root) {
            return Err(LibError::BadRequest(format!(
                "{rel} resolves outside the source root"
            )));
        }
        Ok(abs)
    }
}

pub(crate) fn system_time_ms(t: std::time::SystemTime) -> Option<i64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sftp_uri_with_userinfo_and_port() {
        let c = SourceConnection::parse(
            "sftp",
            "sftp://bob@host.example:2222/assets",
            &ConnOptions::default(),
        )
        .unwrap();
        let SourceConnection::Sftp(cfg) = &c else {
            panic!()
        };
        assert_eq!(cfg.host, "host.example");
        assert_eq!(cfg.port, 2222);
        assert_eq!(cfg.username, "bob");
        assert_eq!(cfg.base_path, "/assets");
        // Display never leaks a secret.
        assert_eq!(c.display_uri(), "sftp://bob@host.example:2222/assets");
    }

    #[test]
    fn options_override_uri_userinfo() {
        let opts = ConnOptions {
            username: Some("alice".into()),
            password: Some("s3cret".into()),
            ..Default::default()
        };
        let c = SourceConnection::parse("sftp", "sftp://bob@host/dir", &opts).unwrap();
        let SourceConnection::Sftp(cfg) = &c else {
            panic!()
        };
        assert_eq!(cfg.username, "alice");
        assert_eq!(cfg.password.as_deref(), Some("s3cret"));
        assert!(
            !c.display_uri().contains("s3cret"),
            "secret must not appear in display uri"
        );
    }

    #[test]
    fn parses_smb_uri() {
        let c = SourceConnection::parse(
            "smb",
            "smb://nas.local/textures/pbr",
            &ConnOptions::default(),
        )
        .unwrap();
        let SourceConnection::Smb(cfg) = &c else {
            panic!()
        };
        assert_eq!(cfg.host, "nas.local");
        assert_eq!(cfg.share, "textures");
        assert_eq!(cfg.base_path, "pbr");
        assert_eq!(c.display_uri(), "smb://nas.local/textures/pbr");
    }

    #[test]
    fn rejects_traversal() {
        assert!(guard_rel_path("../etc/passwd").is_err());
        assert!(guard_rel_path("a/b/c.png").is_ok());
    }

    /// The point of issue #87: a download must land in the *given* directory, not
    /// `std::env::temp_dir()`. On most Linux hosts the latter is a tmpfs, so a multi-gigabyte
    /// remote video would be written into RAM.
    #[test]
    #[cfg(any(feature = "sftp", feature = "smb"))]
    fn a_download_lands_in_the_given_scratch_dir() {
        let dir = tempfile::tempdir().unwrap();
        let f = temp_sink("Textures/brick_wall.png", dir.path()).unwrap();
        assert_eq!(
            f.path().parent(),
            Some(dir.path()),
            "download escaped the scratch dir"
        );
        // The logical extension survives, so extension-keyed detection still works on a file whose
        // stem is random (tech-spec 04 §7).
        assert_eq!(
            f.path().extension().and_then(|e| e.to_str()),
            Some("png"),
            "logical extension lost: {}",
            f.path().display()
        );
        assert!(f
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(SCRATCH_PREFIX));
    }

    /// An unusable scratch dir degrades to the OS temp dir rather than failing the fetch — a
    /// read-only or missing data dir should cost you the tmpfs fix, not the download.
    #[test]
    #[cfg(any(feature = "sftp", feature = "smb"))]
    fn an_unusable_scratch_dir_falls_back_instead_of_failing() {
        let missing = Path::new("/nonexistent-3dam-scratch-cf81/nope");
        let f = temp_sink("a.bin", missing).expect("fetch must not fail on a bad scratch dir");
        assert_ne!(f.path().parent(), Some(missing));
    }

    /// `Fetched::Temp` cleans up on drop, which covers every normal path but not a kill -9 mid-fetch.
    /// Without the startup sweep, a crash during a large remote pass strands gigabytes in the data
    /// dir with nothing that will ever collect them.
    #[test]
    fn clean_scratch_removes_orphans_and_leaves_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir.path().join(format!("{SCRATCH_PREFIX}abc123.mp4"));
        // An interrupted *upload* strands bytes exactly the same way, and can be larger — it is
        // bounded only by `[upload] max_file_mb`. A prefix the sweep does not know is worse than no
        // sweep at all, because nothing else will ever collect it (issue #80).
        let upload = dir
            .path()
            .join(format!("{UPLOAD_SCRATCH_PREFIX}def456.fbx"));
        let innocent = dir.path().join("library.db");
        std::fs::write(&orphan, b"stranded").unwrap();
        std::fs::write(&upload, b"half an upload").unwrap();
        std::fs::write(&innocent, b"precious").unwrap();

        assert_eq!(clean_scratch(dir.path()), 2);
        assert!(!orphan.exists(), "orphaned download survived the sweep");
        assert!(!upload.exists(), "orphaned upload survived the sweep");
        assert!(
            innocent.exists(),
            "the sweep must only ever match its own prefixes"
        );
    }

    /// `writable()` has to answer "can this process create a file here", not "are the mode bits
    /// clear". The two differ on the cases that actually occur: a directory owned by another user
    /// with mode 0755 reads as writable by every mode-bit check and is not.
    ///
    /// (Assumes a non-root test runner, as the rest of the suite does: root bypasses the permission
    /// check entirely and would genuinely be able to write here.)
    #[test]
    #[cfg(unix)]
    fn writable_reflects_real_access_not_mode_bits() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, src) = local_root();
        assert!(src.writable(), "a fresh temp dir is writable");

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let answer = src.writable();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !answer,
            "a directory this process cannot create files in is not a destination"
        );

        // And the probe leaves nothing behind: it is called on every source listing.
        assert!(src.writable());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert!(leftovers.is_empty(), "the probe littered: {leftovers:?}");
    }

    /// A scratch directory that does not exist is not an error worth failing startup over.
    #[test]
    fn clean_scratch_tolerates_a_missing_dir() {
        assert_eq!(
            clean_scratch(Path::new("/nonexistent-3dam-scratch-cf81")),
            0
        );
    }

    // ── upload write path (issue #80) ──────────────────────────────────────

    fn local_root() -> (tempfile::TempDir, LocalFsSource) {
        let dir = tempfile::tempdir().unwrap();
        let src = LocalFsSource::new(dir.path());
        (dir, src)
    }

    #[test]
    fn put_creates_a_file_and_refuses_to_overwrite_it() {
        let (dir, src) = local_root();
        src.put("Textures/brick.png", &mut &b"first"[..]).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("Textures/brick.png")).unwrap(),
            b"first",
            "parents are created and the bytes land"
        );

        // Create-only is the invariant the whole feature rests on (tech-spec 08 §5.1): a second
        // write to the same path must fail rather than replace, with no flag that changes it.
        //
        // Specifically `Conflict`, and upload's collision handling depends on that: `Suffix` and
        // `Skip` recover from a taken name by matching this variant, so if it ever collapsed back
        // into `BadRequest` they would become indistinguishable from a malformed-name rejection
        // and either retry a name that can never succeed or skip a file the user meant to store.
        let err = src
            .put("Textures/brick.png", &mut &b"second"[..])
            .unwrap_err();
        assert!(matches!(err, LibError::Conflict(_)), "got {err:?}");
        assert_eq!(
            std::fs::read(dir.path().join("Textures/brick.png")).unwrap(),
            b"first",
            "the original bytes are untouched"
        );

        // The other half of that contract: a name we refuse to create is *not* a conflict, so a
        // caller retrying under a suffix cannot loop on it.
        let err = src.put("Textures/CON", &mut &b"x"[..]).unwrap_err();
        assert!(matches!(err, LibError::BadRequest(_)), "got {err:?}");
    }

    /// The escape a lexical path check cannot catch: every string rule passes, and the write still
    /// lands outside the root unless the destination is canonicalised.
    #[test]
    #[cfg(unix)]
    fn put_refuses_a_symlinked_escape() {
        let (dir, src) = local_root();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();

        let err = src.put("escape/stolen.png", &mut &b"x"[..]).unwrap_err();
        assert!(matches!(err, LibError::BadRequest(_)), "got {err:?}");
        assert!(
            !outside.path().join("stolen.png").exists(),
            "nothing may be written outside the source root"
        );
    }

    #[test]
    fn put_rejects_hostile_names_before_touching_the_filesystem() {
        let (dir, src) = local_root();
        for bad in [
            "../escape.png",
            "/etc/passwd",
            "CON.png",
            "a\u{202E}gnp.exe",
        ] {
            assert!(src.put(bad, &mut &b"x"[..]).is_err(), "must reject {bad:?}");
        }
        // Not one stray file, not even a temp: validation happens before any write.
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert!(
            left.is_empty(),
            "rejected uploads left {} entries",
            left.len()
        );
    }

    #[test]
    fn mkdir_is_idempotent_and_guarded() {
        let (dir, src) = local_root();
        src.mkdir("Environment/Rock").unwrap();
        src.mkdir("Environment/Rock").unwrap(); // existing is success, not a conflict
        assert!(dir.path().join("Environment/Rock").is_dir());
        assert!(src.mkdir("../escape").is_err());
    }

    /// A source kind that has not opted in stays read-only — the default that makes adding a
    /// backend safe rather than accidentally writable.
    #[test]
    fn remote_backends_are_not_writable_yet() {
        struct ReadOnly;
        impl FileSource for ReadOnly {
            fn walk(
                &self,
                _sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
            ) -> Result<(), LibError> {
                Ok(())
            }
            fn fetch(&self, _rel: &str) -> Result<Fetched, LibError> {
                Err(LibError::NotFound("x".into()))
            }
        }
        let s = ReadOnly;
        assert!(!s.writable());
        assert!(matches!(s.mkdir("a"), Err(LibError::Unsupported(_))));
        assert!(matches!(
            s.put("a.png", &mut &b""[..]),
            Err(LibError::Unsupported(_))
        ));
    }
}
