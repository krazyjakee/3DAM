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
pub fn open_source(conn: &SourceConnection) -> Result<Box<dyn FileSource>, LibError> {
    match conn {
        SourceConnection::LocalFs { root } => Ok(Box::new(LocalFsSource::new(root))),
        #[cfg(feature = "sftp")]
        SourceConnection::Sftp(cfg) => Ok(Box::new(sftp::SftpSource::connect(cfg.clone())?)),
        #[cfg(not(feature = "sftp"))]
        SourceConnection::Sftp(_) => Err(LibError::Unsupported(
            "SFTP support is not compiled into this build".into(),
        )),
        #[cfg(feature = "smb")]
        SourceConnection::Smb(cfg) => Ok(Box::new(smb::SmbSource::connect(cfg.clone())?)),
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

/// Materialise remote bytes into a temp file suffixed with the entry's logical extension.
#[cfg(any(feature = "sftp", feature = "smb"))]
pub(crate) fn temp_from_bytes(rel_path: &str, bytes: &[u8]) -> Result<Fetched, LibError> {
    use std::io::Write;
    let suffix = Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut file = tempfile::Builder::new()
        .prefix("3dam-remote-")
        .suffix(&suffix)
        .tempfile()
        .map_err(|e| LibError::Internal(format!("temp file: {e}")))?;
    file.write_all(bytes)
        .map_err(|e| LibError::Internal(format!("temp write: {e}")))?;
    file.flush().ok();
    Ok(Fetched::Temp(file))
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
}
