//! `dam-sources` — the `Source` trait and its file-source implementations (tech-spec 07).
//!
//! A **file source** yields raw bytes the engine hashes, detects, and catalogues locally (§1, §2).
//! Three kinds ship here behind one trait: the **local filesystem**, **SFTP** (`russh`), and
//! **SMB2/3** (`smb`). The engine above them is identical — the only divergence is connection setup
//! and how an entry's bytes reach a locally-readable path ([`Fetched`]). The federated peer source
//! (catalog rows, not bytes) is a separate surface that lands with phase 6.
//!
//! **Byte access is uniform.** The media handlers ([`dam_media`]) are path-based, so every source
//! resolves an entry to a private local path via [`FileSource::fetch`]. Remote sources download to
//! it; local sources copy from a capability-relative, already-open handle. That copy is intentional:
//! path-based handlers would otherwise reopen an attacker-swappable source pathname after fetch
//! returned. The temp suffix preserves the logical extension for extension-keyed detection.

use dam_api::LibError;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use url::{Host, Url};

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

/// A locally-readable handle to an entry's bytes. Every backend materialises a private temp file,
/// removed on drop, so path-based media handlers never reopen an attacker-swappable source path.
pub enum Fetched {
    /// A pinned/materialised copy, deleted when this drops. Suffixed with the logical extension so
    /// extension-keyed detection (tech-spec 04 §7) still works.
    Temp(tempfile::NamedTempFile),
}

impl Fetched {
    pub fn path(&self) -> &Path {
        match self {
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

    /// Resolve an entry's bytes to a private local path for the media handlers. Guards all rooted,
    /// prefixed, URL, and traversal shapes and pins local reads to the registered root capability.
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

/// Everything needed to (re)build a file source. Only the non-secret fields are serialised into the
/// portable source record; credential material is populated at runtime from `credential_ref`, an
/// opaque host-secret-store key held in the source table's separate `auth_ref` column.
///
/// [`display_uri`]: SourceConnection::display_uri
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConnection {
    LocalFs { root: String },
    Sftp(SftpConfig),
    Smb(SmbConfig),
    Federated(FederatedConfig),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SftpConfig {
    pub host: String,
    #[serde(default = "default_sftp_port")]
    pub port: u16,
    pub username: String,
    /// Source-root-relative base directory on the remote host (absolute or `~`-relative per server).
    #[serde(default = "default_remote_root")]
    pub base_path: String,
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    /// Path to a private key file on *this* host. It is host-private configuration: copying the
    /// portable catalog must not disclose either the path or the key's location.
    #[serde(default, skip_serializing)]
    pub private_key: Option<String>,
    #[serde(default, skip_serializing)]
    pub passphrase: Option<String>,
    /// Opaque secret-store key. The store injects this from `source.auth_ref`; serde deliberately
    /// ignores it so connection JSON cannot become a second credential-reference authority.
    #[serde(skip)]
    pub credential_ref: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SmbConfig {
    pub host: String,
    #[serde(default = "default_smb_port")]
    pub port: u16,
    pub share: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_path: String,
    #[serde(default)]
    pub username: String,
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip)]
    pub credential_ref: Option<String>,
}

/// A peer 3DAM server (phase 6, issue #39). Yields **catalog rows, not bytes** — it satisfies the
/// source *record* model (persisted connection + secret) but is deliberately not a [`FileSource`]:
/// the query fan-out in `dam-core` talks to it over the peer's HTTP read API, and the only byte
/// transfer it ever does is fetching remote-owned previews (tech-spec 07 §4).
#[derive(Clone, Serialize, Deserialize)]
pub struct FederatedConfig {
    /// Peer base endpoint, `http(s)://host:port` (no trailing slash, no path).
    pub endpoint: String,
    /// Bearer token for the peer, when its auth mode requires one. Runtime-only: the serialised
    /// connection always omits it, including while migrating a legacy row that still contains it.
    #[serde(default, skip_serializing)]
    pub token: Option<String>,
    #[serde(skip)]
    pub credential_ref: Option<String>,
}

/// Credential payload stored behind one opaque reference. This type is intentionally separate
/// from the portable connection model, and its `Debug` implementation never reveals values.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceCredentials {
    Sftp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        private_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<String>,
    },
    Smb { password: String },
    Federated { token: String },
}

impl std::fmt::Debug for SourceCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            SourceCredentials::Sftp { .. } => "sftp",
            SourceCredentials::Smb { .. } => "smb",
            SourceCredentials::Federated { .. } => "federated",
        };
        f.debug_struct("SourceCredentials")
            .field("kind", &kind)
            .field("material", &"[REDACTED]")
            .finish()
    }
}

impl std::fmt::Debug for SourceConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceConnection")
            .field("kind", &self.kind())
            .field("uri", &self.display_uri())
            .field("credential_ref", &self.credential_ref().map(|_| "[OPAQUE]"))
            .field(
                "credential_material",
                &self.has_inline_credentials().then_some("[REDACTED]"),
            )
            .finish()
    }
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

    /// Opaque key for resolving this connection's host-local credential, when one exists.
    pub fn credential_ref(&self) -> Option<&str> {
        match self {
            SourceConnection::LocalFs { .. } => None,
            SourceConnection::Sftp(c) => c.credential_ref.as_deref(),
            SourceConnection::Smb(c) => c.credential_ref.as_deref(),
            SourceConnection::Federated(c) => c.credential_ref.as_deref(),
        }
    }

    /// Inject the reference stored in `source.auth_ref`. It is never serialised into connection
    /// JSON, so there is one canonical reference field in the database.
    pub fn set_credential_ref(&mut self, credential_ref: Option<String>) {
        match self {
            SourceConnection::LocalFs { .. } => {}
            SourceConnection::Sftp(c) => c.credential_ref = credential_ref,
            SourceConnection::Smb(c) => c.credential_ref = credential_ref,
            SourceConnection::Federated(c) => c.credential_ref = credential_ref,
        }
    }

    /// Remove runtime/legacy inline credential fields and return their secure-store payload.
    /// Calling this before every catalog write is defence in depth on top of serde's unconditional
    /// `skip_serializing` annotations.
    pub fn take_credentials(&mut self) -> Option<SourceCredentials> {
        match self {
            SourceConnection::LocalFs { .. } => None,
            SourceConnection::Sftp(c) => {
                let password = c.password.take();
                let private_key = c.private_key.take();
                let passphrase = c.passphrase.take();
                (password.is_some() || private_key.is_some() || passphrase.is_some()).then_some(
                    SourceCredentials::Sftp {
                        password,
                        private_key,
                        passphrase,
                    },
                )
            }
            SourceConnection::Smb(c) => c
                .password
                .take()
                .map(|password| SourceCredentials::Smb { password }),
            SourceConnection::Federated(c) => c
                .token
                .take()
                .map(|token| SourceCredentials::Federated { token }),
        }
    }

    /// Whether runtime or legacy inline credential material is present. Persistence layers use
    /// this to reject an unsafe call site instead of silently dropping credentials during serde.
    pub fn has_inline_credentials(&self) -> bool {
        match self {
            SourceConnection::LocalFs { .. } => false,
            SourceConnection::Sftp(c) => {
                c.password.is_some() || c.private_key.is_some() || c.passphrase.is_some()
            }
            SourceConnection::Smb(c) => c.password.is_some(),
            SourceConnection::Federated(c) => c.token.is_some(),
        }
    }

    /// Hydrate a persisted connection from a resolved credential payload. A kind mismatch means
    /// the host secret entry is corrupt or was replaced; fail closed without exposing either value.
    pub fn apply_credentials(&mut self, credentials: SourceCredentials) -> Result<(), LibError> {
        match (self, credentials) {
            (
                SourceConnection::Sftp(c),
                SourceCredentials::Sftp {
                    password,
                    private_key,
                    passphrase,
                },
            ) => {
                c.password = password;
                c.private_key = private_key;
                c.passphrase = passphrase;
                Ok(())
            }
            (SourceConnection::Smb(c), SourceCredentials::Smb { password }) => {
                c.password = Some(password);
                Ok(())
            }
            (SourceConnection::Federated(c), SourceCredentials::Federated { token }) => {
                c.token = Some(token);
                Ok(())
            }
            _ => Err(LibError::SourceUnavailable(
                "source credential entry is invalid; re-enter the source credentials".into(),
            )),
        }
    }

    /// A stable, **secret-free** URI for display/listing (never carries a password/key).
    pub fn display_uri(&self) -> String {
        match self {
            SourceConnection::LocalFs { root } => root.clone(),
            SourceConnection::Sftp(c) => {
                let base = c.base_path.trim_start_matches('/');
                format!(
                    "sftp://{}@{}:{}/{}",
                    percent_encode_component(&c.username),
                    display_host(&c.host),
                    c.port,
                    percent_encode_path(base)
                )
            }
            SourceConnection::Smb(c) => {
                let authority = if c.port == default_smb_port() {
                    display_host(&c.host)
                } else {
                    format!("{}:{}", display_host(&c.host), c.port)
                };
                let base = if c.base_path.is_empty() {
                    String::new()
                } else {
                    format!(
                        "/{}",
                        percent_encode_path(c.base_path.trim_start_matches('/'))
                    )
                };
                format!(
                    "smb://{}/{}{}",
                    authority,
                    percent_encode_component(c.share.trim_matches('/')),
                    base
                )
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
/// `scratch` is where a backend materialises fetched bytes (issue #87). It is a required
/// positional argument rather than an option with a default, for the same reason `Visibility` is:
/// the sensible-looking default (`std::env::temp_dir()`) is the wrong one on most Linux hosts, and a
/// caller that inherited it by omission would reintroduce the bug in silence.
pub fn open_source(
    conn: &SourceConnection,
    scratch: &Path,
) -> Result<Box<dyn FileSource>, LibError> {
    match conn {
        SourceConnection::LocalFs { root } => Ok(Box::new(LocalFsSource::registered(
            root,
            Some(scratch.to_path_buf()),
        ))),
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
/// **The remote answer is a capability, not a probe** (slice 7). SFTP and SMB now implement the
/// write side, so they report `true` — meaning "this kind can be written to", not "this credential
/// can write here". Whether the account actually has permission is settled by the first `put`,
/// which fails with a clear error. That is a real step down from the local answer, and it is the
/// price of not opening a connection per source per render: the alternative is a sidebar that
/// stalls on a sleeping NAS to answer a question the upload itself will answer anyway.
///
/// The failure mode this leaves is one file failing where the picker implied it would work — which
/// is exactly what the per-file, fail-soft upload transport was built to absorb.
/// Feature-gated per kind, because a backend compiled out cannot write any more than one that was
/// never implemented — `open_source` answers `Unsupported` for both, and the picker should say so
/// the same way.
pub fn writable_without_handshake(conn: &SourceConnection) -> bool {
    match conn {
        // The only kind whose answer varies, and the only one cheap enough to ask for real.
        SourceConnection::LocalFs { root } => LocalFsSource::registered(root, None).writable(),
        SourceConnection::Sftp(_) => cfg!(feature = "sftp"),
        // The port check mirrors `SmbSource::connect`, which refuses a non-default port outright.
        // Offering such a source would not be the documented "the credential might lack permission"
        // trade-off — it is a build-level impossibility, knowable here without a handshake, and
        // every file in the drop would fail with `Unsupported` rather than anything actionable.
        SourceConnection::Smb(cfg) => cfg!(feature = "smb") && cfg.port == 445,
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
    let parsed = parse_connection_url(uri, "sftp")?;
    let host = parsed_host(&parsed, "sftp")?;
    let uri_user = if parsed.username().is_empty() {
        None
    } else {
        Some(decode_component(parsed.username(), "sftp")?)
    };
    let uri_pass = parsed
        .password()
        .map(|password| decode_component(password, "sftp"))
        .transpose()?;
    let username = opts
        .username
        .clone()
        .or(uri_user)
        .ok_or_else(|| LibError::BadRequest("sftp source requires a username".into()))?;
    if username.is_empty() {
        return Err(LibError::BadRequest(
            "sftp source requires a username".into(),
        ));
    }
    let path = decode_component(parsed.path(), "sftp")?;
    Ok(SourceConnection::Sftp(SftpConfig {
        host,
        port: opts.port.or(parsed.port()).unwrap_or(22),
        username,
        base_path: if path.is_empty() || path == "/" {
            default_remote_root()
        } else {
            format!("/{}", path.trim_start_matches('/'))
        },
        password: opts.password.clone().or(uri_pass),
        private_key: opts.private_key.clone(),
        passphrase: opts.passphrase.clone(),
        credential_ref: None,
    }))
}

/// `smb://[user[:pass]@]host[:port]/share[/base/path]`. Options override URI userinfo.
fn parse_smb(uri: &str, opts: &ConnOptions) -> Result<SourceConnection, LibError> {
    let parsed = parse_connection_url(uri, "smb")?;
    let host = parsed_host(&parsed, "smb")?;
    let mut segments = parsed
        .path_segments()
        .ok_or_else(|| LibError::BadRequest("invalid smb source uri".into()))?;
    let share = decode_component(segments.next().unwrap_or_default(), "smb")?;
    if share.is_empty() {
        return Err(LibError::BadRequest(
            "smb source uri must include a share: smb://host/share/…".into(),
        ));
    }
    let base = segments
        .map(|segment| decode_component(segment, "smb"))
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    let uri_user = if parsed.username().is_empty() {
        None
    } else {
        Some(decode_component(parsed.username(), "smb")?)
    };
    let uri_pass = parsed
        .password()
        .map(|password| decode_component(password, "smb"))
        .transpose()?;
    Ok(SourceConnection::Smb(SmbConfig {
        host,
        port: opts.port.or(parsed.port()).unwrap_or(445),
        share,
        base_path: base.trim_matches('/').to_string(),
        username: opts.username.clone().or(uri_user).unwrap_or_default(),
        password: opts.password.clone().or(uri_pass),
        domain: opts.domain.clone(),
        credential_ref: None,
    }))
}

fn parse_connection_url(uri: &str, scheme: &str) -> Result<Url, LibError> {
    validate_percent_escapes(uri, scheme)?;
    let parsed = Url::parse(uri)
        .map_err(|_| LibError::BadRequest(format!("invalid {scheme} source uri")))?;
    if parsed.scheme() != scheme || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(LibError::BadRequest(format!(
            "invalid {scheme} source uri"
        )));
    }
    Ok(parsed)
}

fn parsed_host(parsed: &Url, scheme: &str) -> Result<String, LibError> {
    match parsed.host() {
        Some(Host::Domain(host)) if !host.is_empty() => Ok(host.to_string()),
        Some(Host::Ipv4(host)) => Ok(host.to_string()),
        Some(Host::Ipv6(host)) => Ok(host.to_string()),
        _ => Err(LibError::BadRequest(format!(
            "{scheme} source uri is missing a host"
        ))),
    }
}

fn validate_percent_escapes(uri: &str, scheme: &str) -> Result<(), LibError> {
    let bytes = uri.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && (i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit())
        {
            return Err(LibError::BadRequest(format!(
                "invalid percent escape in {scheme} source uri"
            )));
        }
        i += if bytes[i] == b'%' { 3 } else { 1 };
    }
    Ok(())
}

fn decode_component(value: &str, scheme: &str) -> Result<String, LibError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = &value[i + 1..i + 3];
            decoded.push(u8::from_str_radix(hex, 16).expect("escapes validated before URL parsing"));
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| LibError::BadRequest(format!("invalid UTF-8 in {scheme} source uri")))
}

fn percent_encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn percent_encode_path(path: &str) -> String {
    path.split('/')
        .map(percent_encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn display_host(host: &str) -> String {
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    match unbracketed.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(address)) => format!("[{address}]"),
        _ => host.to_string(),
    }
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
        credential_ref: None,
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

/// Validate and normalise a source-relative path before any backend sees it.
///
/// This is deliberately a path-*geometry* check, not the stricter upload naming policy: an existing
/// POSIX file named `CON` or `report.` remains readable. In particular, a path which is merely an
/// ordinary filename on Unix (`C:\\secret`, for example) is still a drive path when the same
/// catalog is opened on Windows. Remote backends use it too, so changing source kind can never
/// change whether a path is interpreted as rooted, prefixed, URL-like, or traversing.
pub(crate) fn guard_rel_path(rel_path: &str) -> Result<String, LibError> {
    let reject = || LibError::BadRequest("asset path must stay within its source root".into());
    if rel_path.trim().is_empty()
        || rel_path.starts_with('/')
        || rel_path.starts_with('\\')
        // Backslash is a separator on Windows. The scanner normalises separators to `/`, so one in
        // a stored/requested path is ambiguous across platforms and must not reach a backend.
        || rel_path.contains('\\')
    {
        return Err(reject());
    }
    let bytes = rel_path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Err(reject()); // drive-absolute and drive-relative (`C:/x`, `C:x`)
    }
    // Reject every URI-scheme shape, not only `://`: `file:/x` and `file:x` are URLs too.
    if let Some((scheme, _)) = rel_path.split_once(':') {
        if !scheme.is_empty()
            && scheme
                .chars()
                .enumerate()
                .all(|(i, c)| c.is_ascii_alphabetic() || (i > 0 && (c.is_ascii_digit() || "+-.".contains(c))))
        {
            return Err(reject());
        }
    }
    let mut parts = Vec::new();
    for part in rel_path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(reject()),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(reject());
    }
    Ok(parts.join("/"))
}

/// Transfer buffer for a streaming remote fetch. This is now the *whole* memory cost of downloading
/// an asset, however large it is (issue #87) — 256 KiB is big enough to keep a network round-trip
/// amortised and small enough that a pool of concurrent fetches is still nothing.
pub(crate) const FETCH_CHUNK: usize = 256 * 1024;

/// Filename prefix every fetched-byte materialisation carries (remote download or secured local
/// copy). Also what [`clean_scratch`] matches, so changing it would orphan older leftovers.
pub(crate) const SCRATCH_PREFIX: &str = "3dam-remote-";

/// Filename prefix for an **inbound upload** staged in scratch (issue #80).
///
/// Public because the staging happens in the server's transport layer, not here — but it lives
/// beside [`SCRATCH_PREFIX`] and is swept by the same [`clean_scratch`] for the same reason. A
/// prefix the sweep does not know about is worse than no sweep: an upload killed at 3.5 GB of a
/// 4 GB video leaves that file in the data dir forever, and a leading dot would hide it from `ls`
/// as well.
pub const UPLOAD_SCRATCH_PREFIX: &str = "3dam-upload-";

/// Open a temp file for fetched bytes, **inside the engine's scratch directory**.
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

/// Delete stale fetched-byte materialisations left in `scratch` by a previous run.
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
/// Sweeps **both** things that stage bytes in scratch: fetched copies and inbound uploads. An
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

/// A local directory tree traversed relative to an open root capability.
pub struct LocalFsSource {
    root: PathBuf,
    /// The registered root as an open capability. All content reads and writes resolve relative to
    /// this handle, so renaming/replacing the path (or swapping a child for a symlink) cannot retarget
    /// an operation outside the tree between a check and an open.
    root_dir: Option<cap_std::fs::Dir>,
    /// Materialised local bytes use the same configured, disk-backed scratch area as remote
    /// downloads. `None` is used only by the cheap writability probe, which never fetches.
    scratch: Option<PathBuf>,
}

impl LocalFsSource {
    /// Construct only for metadata/writability operations which cannot fetch. Byte-bearing callers
    /// must come through [`open_source`], where configured scratch is mandatory.
    fn without_scratch(root: impl Into<PathBuf>) -> LocalFsSource {
        let supplied = root.into();
        let root = supplied.canonicalize().unwrap_or(supplied);
        let root_dir = open_registered_root(&root);
        LocalFsSource {
            root,
            root_dir,
            scratch: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reopen a persisted source root. Local roots are canonicalised before registration; opening
    /// every component with no-follow semantics prevents replacing that registered path (or any
    /// ancestor) with a link to a different tree between requests.
    fn registered(root: impl Into<PathBuf>, scratch: Option<PathBuf>) -> LocalFsSource {
        let root = root.into();
        let root_dir = open_registered_root(&root);
        LocalFsSource {
            root,
            root_dir,
            scratch,
        }
    }
}

/// Open a persisted canonical root one component at a time without following symlinks.
///
/// Starting at the platform filesystem root (`/`, `C:\\`, UNC prefix, …) matters: opening the
/// immediate parent ambiently would still let an attacker replace an earlier ancestor. Each step
/// is relative to the directory handle returned by the previous step, and `open_dir_nofollow`
/// combines the no-link decision with the open on Unix and Windows. On a target/filesystem which
/// cannot provide that primitive, the operation returns `None` and the source fails closed.
fn open_registered_root(root: &Path) -> Option<cap_std::fs::Dir> {
    use std::path::Component;

    let mut components = root.components().peekable();
    let mut anchor = PathBuf::new();
    while matches!(components.peek(), Some(Component::Prefix(_) | Component::RootDir)) {
        anchor.push(components.next()?.as_os_str());
    }
    if anchor.as_os_str().is_empty() {
        return None; // registered roots are canonical absolute paths
    }
    let anchor = cap_std::fs::Dir::open_ambient_dir(&anchor, cap_std::ambient_authority()).ok()?;
    let mut current = anchor.into_std_file();
    for component in components {
        let Component::Normal(name) = component else {
            return None;
        };
        current = cap_primitives::fs::open_dir_nofollow(&current, Path::new(name)).ok()?;
    }
    Some(cap_std::fs::Dir::from_std_file(current))
}

impl FileSource for LocalFsSource {
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        let root = self.cap_root()?;
        let mut stack = vec![PathBuf::new()];
        while let Some(rel_dir) = stack.pop() {
            let entries = root.read_dir(&rel_dir).map_err(|_| {
                LibError::SourceUnavailable("registered local source root is unavailable".into())
            })?;
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        if !sink(Err(LibError::SourceUnavailable(
                            "local source entry is unavailable".into(),
                        ))) {
                            return Ok(());
                        }
                        continue;
                    }
                };
                let child = rel_dir.join(entry.file_name());
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(_) => continue,
                };
                if kind.is_dir() {
                    stack.push(child);
                } else if kind.is_file() {
                    let rel_path = child.to_string_lossy().replace('\\', "/");
                    // Reopen through the entry capability before reading metadata. A path-based
                    // metadata call after `file_type` would be another swap window (and could
                    // disclose an external target's size/mtime even though fetch later refused it).
                    let (size, modified_ms) = match entry.open().and_then(|file| file.metadata()) {
                        Ok(meta) => (meta.len(), meta.modified().ok().and_then(system_time_ms)),
                        Err(_) => (0, None),
                    };
                    if !sink(Ok(FileEntry {
                        rel_path,
                        size,
                        modified_ms,
                    })) {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn fetch(&self, rel_path: &str) -> Result<Fetched, LibError> {
        let rel = guard_rel_path(rel_path)?;
        self.fetch_after_validation(&rel, || {})
    }

    /// Probed, not assumed (issue #80). A local source can sit on a read-only mount, a full disk,
    /// or a directory the server process does not own — none of which the *kind* tells you. The
    /// answer feeds a destination picker, so being wrong here means offering the user a folder they
    /// cannot write to.
    ///
    /// **Asks the kernel; does not write anything.** Both halves of that matter.
    ///
    /// *Not the mode bits*: `Permissions::readonly()` answers "is every write bit clear", which
    /// misses all three cases above — an `ro` mount still reports mode 0755, and a root-owned 0755
    /// directory looks writable to an unprivileged process. std's own docs warn it "cannot be
    /// relied upon to predict whether attempts to … write the file will actually succeed".
    /// `faccessat` answers the real question: it consults ownership, ACLs *and* mount flags
    /// (`EROFS`). `AT_EACCESS` asks about the effective uid, which is the one that will do the
    /// write. `X_OK` as well as `W_OK` because creating a file needs search permission on the
    /// directory too.
    ///
    /// *And not a create-then-delete probe*, which was the obvious alternative and is a trap: this
    /// runs on **every** source listing, and creating a file inside a watched source root makes the
    /// watcher fire (`Create(File)`/`Remove(File)` are content changes) and debounce-rescan the
    /// whole source. The web client invalidates its source list when a scan completes, which
    /// refetches, which probes again — an endless rescan of a folder nobody touched.
    fn writable(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let Some(root) = self.root_dir.as_ref() else {
                return false;
            };
            // Ask about `.` relative to the already-open root descriptor. Using the registered
            // pathname here would let a post-open root swap retarget even this metadata probe.
            let path = c".";
            // SAFETY: `path` is a static NUL-terminated C string and `root` remains open for the
            // duration of the call; `faccessat` only reads both.
            unsafe {
                libc::faccessat(
                    root.as_raw_fd(),
                    path.as_ptr(),
                    libc::W_OK | libc::X_OK,
                    libc::AT_EACCESS,
                ) == 0
            }
        }
        // Windows has no cheap equivalent (the read-only attribute is ignored for directories, and
        // a real answer means consulting the DACL), so this errs toward *offering* the destination
        // and letting `put` report the failure. The alternative — probing by creating a file — is
        // the rescan loop described above, which is a worse failure than an honest error at commit.
        #[cfg(not(unix))]
        {
            self.root_dir.is_some()
        }
    }

    fn mkdir(&self, rel_path: &str) -> Result<(), LibError> {
        let rel = safe_name::check_rel_path(rel_path)?;
        self.cap_root()?.create_dir_all(&rel).map_err(|_| {
            LibError::BadRequest("destination must stay within its source root".into())
        })
    }

    fn put(&self, rel_path: &str, bytes: &mut dyn std::io::Read) -> Result<(), LibError> {
        let rel = safe_name::check_rel_path(rel_path)?;
        self.put_after_validation(&rel, bytes, || {})
    }
}

impl LocalFsSource {
    fn put_after_validation(
        &self,
        rel: &str,
        bytes: &mut dyn std::io::Read,
        before_create: impl FnOnce(),
    ) -> Result<(), LibError> {
        use std::io::Write;
        let root = self.cap_root()?;
        before_create();
        if let Some(parent) = Path::new(rel).parent().filter(|p| !p.as_os_str().is_empty()) {
            root.create_dir_all(parent).map_err(|_| {
                LibError::BadRequest("destination must stay within its source root".into())
            })?;
        }

        // Stage under a random sibling name, but create it relative to the pinned capability too.
        // cap-std performs component resolution beneath the root handle on Unix and Windows and
        // refuses a link which would leave it.
        let temp_name = format!(".3dam-upload-{}", uuid::Uuid::now_v7());
        let temp_rel = match Path::new(rel).parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(parent) => parent.join(temp_name),
            None => PathBuf::from(temp_name),
        };
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = root.open_with(&temp_rel, &options).map_err(|_| {
            LibError::BadRequest("destination must stay within its source root".into())
        })?;
        let written = (|| {
            std::io::copy(bytes, &mut file)
                .map_err(|e| LibError::Internal(format!("write {rel}: {e}")))?;
            file.flush()
                .map_err(|e| LibError::Internal(format!("flush {rel}: {e}")))?;
            file.sync_all()
                .map_err(|e| LibError::Internal(format!("fsync {rel}: {e}")))?;

        // Widen the mode *before* the rename, so the file is never visible at its real name with
        // the wrong permissions.
        //
        // `tempfile` creates at 0600 — correct for a temp file, wrong for the asset it becomes. A
        // source is a shared project folder whose other files are 0644; an uploaded texture only
        // the server's uid can read is one nobody else on the machine, and no DCC tool running as
        // another user, can open. Applying the umask (0666 &! umask) would be more faithful still,
        // but reading the umask means temporarily setting it, which races every other thread in the
        // process — so this takes the conventional default rather than a racy approximation of it.
        //
        // Through the open descriptor, not the path: the destination is a user-chosen directory
        // that may be group- or world-writable, and a path-based `set_permissions` there could be
        // redirected by someone swapping the temp name for a symlink between creation and this
        // call. The fd already refers to the file we made, so there is nothing to redirect.
            #[cfg(unix)]
            {
                use cap_std::fs::PermissionsExt;
                file.set_permissions(cap_std::fs::Permissions::from_mode(0o644))
                    .map_err(|e| LibError::Internal(format!("chmod {rel}: {e}")))?;
            }
            Ok::<(), LibError>(())
        })();
        drop(file);
        if let Err(error) = written {
            let _ = root.remove_file(&temp_rel);
            return Err(error);
        }

        // Publishing is an atomic, no-clobber hard link: if the destination exists, the kernel
        // refuses the link; if it does not, readers see the fully written and synced inode in one
        // step. Both names are resolved relative to the same capability, closing symlink swaps.
        let published = root.hard_link(&temp_rel, root, &rel).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                LibError::Conflict(format!("{rel} already exists"))
            } else {
                LibError::BadRequest("destination must stay within its source root".into())
            }
        });
        let _ = root.remove_file(&temp_rel);
        published?;
        Ok(())
    }
    fn cap_root(&self) -> Result<&cap_std::fs::Dir, LibError> {
        self.root_dir.as_ref().ok_or_else(|| {
            LibError::SourceUnavailable("registered local source root is unavailable".into())
        })
    }

    /// Open from the pinned root capability, then copy from that already-open handle to a private
    /// temp file. Existing media handlers are path-based; returning the original path would make
    /// them reopen it and reintroduce a symlink-swap race after this method returned.
    fn fetch_after_validation(
        &self,
        rel: &str,
        before_open: impl FnOnce(),
    ) -> Result<Fetched, LibError> {
        before_open();
        let mut source = self.cap_root()?.open(rel).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => LibError::NotFound("source file is unavailable".into()),
            _ => LibError::BadRequest("asset path must stay within its source root".into()),
        })?;
        if !source
            .metadata()
            .map_err(|_| LibError::NotFound("source file is unavailable".into()))?
            .is_file()
        {
            return Err(LibError::NotFound("source file is unavailable".into()));
        }
        let scratch = self.scratch.as_deref().ok_or_else(|| {
            LibError::Internal("local source fetch has no configured scratch directory".into())
        })?;
        let mut sink = temp_sink(rel, scratch)?;
        std::io::copy(&mut source, &mut sink)
            .map_err(|e| LibError::Internal(format!("local fetch copy: {e}")))?;
        Ok(Fetched::Temp(sink))
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
    fn parses_and_displays_sftp_ipv6_and_encoded_userinfo() {
        let c = SourceConnection::parse(
            "sftp",
            "sftp://first%40last:p%3Ass@[::1]:2200/My%20Assets",
            &ConnOptions::default(),
        )
        .unwrap();
        let SourceConnection::Sftp(cfg) = &c else {
            panic!()
        };
        assert_eq!(cfg.host, "::1");
        assert_eq!(cfg.port, 2200);
        assert_eq!(cfg.username, "first@last");
        assert_eq!(cfg.password.as_deref(), Some("p:ss"));
        assert_eq!(cfg.base_path, "/My Assets");
        assert_eq!(
            c.display_uri(),
            "sftp://first%40last@[::1]:2200/My%20Assets"
        );

        let reparsed = SourceConnection::parse("sftp", &c.display_uri(), &ConnOptions::default())
            .expect("sanitized SFTP display URI must remain parseable");
        let SourceConnection::Sftp(reparsed) = reparsed else {
            panic!()
        };
        assert_eq!(reparsed.host, cfg.host);
        assert_eq!(reparsed.port, cfg.port);
        assert_eq!(reparsed.username, cfg.username);
        assert_eq!(reparsed.base_path, cfg.base_path);
        assert_eq!(reparsed.password, None);
    }

    #[test]
    fn parses_sftp_ipv6_without_an_explicit_port() {
        let c = SourceConnection::parse(
            "sftp",
            "sftp://bob@[2001:db8::1]/assets",
            &ConnOptions::default(),
        )
        .unwrap();
        let SourceConnection::Sftp(cfg) = c else {
            panic!()
        };
        assert_eq!(cfg.host, "2001:db8::1");
        assert_eq!(cfg.port, 22);
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
    fn parses_smb_ipv6_port_userinfo_and_encoded_paths() {
        let c = SourceConnection::parse(
            "smb",
            "smb://domain%5Cuser:p%40ss@[::1]:1445/team%20share/art%20work",
            &ConnOptions::default(),
        )
        .unwrap();
        let SourceConnection::Smb(cfg) = &c else {
            panic!()
        };
        assert_eq!(cfg.host, "::1");
        assert_eq!(cfg.port, 1445);
        assert_eq!(cfg.username, "domain\\user");
        assert_eq!(cfg.password.as_deref(), Some("p@ss"));
        assert_eq!(cfg.share, "team share");
        assert_eq!(cfg.base_path, "art work");
        assert_eq!(
            c.display_uri(),
            "smb://[::1]:1445/team%20share/art%20work"
        );

        let reparsed = SourceConnection::parse("smb", &c.display_uri(), &ConnOptions::default())
            .expect("sanitized SMB display URI must remain parseable");
        let SourceConnection::Smb(reparsed) = reparsed else {
            panic!()
        };
        assert_eq!(reparsed.host, cfg.host);
        assert_eq!(reparsed.port, cfg.port);
        assert_eq!(reparsed.share, cfg.share);
        assert_eq!(reparsed.base_path, cfg.base_path);
    }

    #[test]
    fn options_override_all_uri_connection_fields() {
        let opts = ConnOptions {
            username: Some("option-user".into()),
            password: Some("option-password".into()),
            port: Some(2022),
            ..Default::default()
        };
        let c = SourceConnection::parse(
            "sftp",
            "sftp://uri-user:uri-password@host.example:22/assets",
            &opts,
        )
        .unwrap();
        let SourceConnection::Sftp(cfg) = c else {
            panic!()
        };
        assert_eq!(cfg.username, "option-user");
        assert_eq!(cfg.password.as_deref(), Some("option-password"));
        assert_eq!(cfg.port, 2022);
    }

    #[test]
    fn malformed_remote_authorities_are_secret_free_bad_requests() {
        for (kind, uri, secret) in [
            ("sftp", "sftp://user:top-secret@[::1/assets", "top-secret"),
            ("sftp", "sftp://user:top-secret@host:nope/assets", "top-secret"),
            ("sftp", "sftp://user:top-secret@host:70000/assets", "top-secret"),
            ("sftp", "sftp://user:top%ZZsecret@host/assets", "top%ZZsecret"),
            ("smb", "smb://[::1/share", ""),
            ("smb", "smb://host:not-a-port/share", ""),
        ] {
            let error = SourceConnection::parse(kind, uri, &ConnOptions::default()).unwrap_err();
            assert!(matches!(error, LibError::BadRequest(_)), "{uri}: {error}");
            assert!(
                secret.is_empty() || !error.to_string().contains(secret),
                "parse error leaked URI credentials: {error}"
            );
        }
    }

    #[test]
    fn existing_connection_records_keep_their_serialized_meaning() {
        let json = r#"{
            "kind":"sftp",
            "host":"nas.example",
            "username":"legacy",
            "base_path":"/assets",
            "password":"secret"
        }"#;
        let SourceConnection::Sftp(cfg) = serde_json::from_str(json).unwrap() else {
            panic!()
        };
        assert_eq!(cfg.host, "nas.example");
        assert_eq!(cfg.port, 22);
        assert_eq!(cfg.username, "legacy");
        assert_eq!(cfg.base_path, "/assets");
        assert_eq!(cfg.password.as_deref(), Some("secret"));
    }

    #[test]
    fn portable_connection_serialization_and_debug_never_expose_credentials() {
        const SENTINEL: &str = "issue-103-sentinel-password";
        let connection = SourceConnection::parse(
            "sftp",
            "sftp://legacy@example.invalid/assets",
            &ConnOptions {
                password: Some(SENTINEL.into()),
                private_key: Some("/private/sentinel/id_ed25519".into()),
                passphrase: Some("sentinel-passphrase".into()),
                ..ConnOptions::default()
            },
        )
        .unwrap();
        let json = serde_json::to_string(&connection).unwrap();
        let debug = format!("{connection:?}");
        for forbidden in [SENTINEL, "/private/sentinel", "sentinel-passphrase"] {
            assert!(!json.contains(forbidden), "portable JSON leaked {forbidden}");
            assert!(!debug.contains(forbidden), "Debug leaked {forbidden}");
        }
        assert!(!json.contains("password"));
        assert!(!json.contains("private_key"));
        assert!(!json.contains("passphrase"));
    }

    #[test]
    fn fetch_paths_are_relative_under_unix_and_windows_semantics() {
        for bad in [
            "../etc/passwd",
            "a/../../etc/passwd",
            "/etc/passwd",
            "\\\\server\\share\\secret",
            "\\windows\\system32",
            "C:/Windows/System32",
            "C:notes.txt",
            "file:///etc/passwd",
            "file:/etc/passwd",
            "https://example.invalid/secret",
            "",
            ".",
        ] {
            let error = guard_rel_path(bad).unwrap_err();
            assert!(matches!(error, LibError::BadRequest(_)), "{bad:?}: {error}");
            assert!(!error.to_string().contains(bad), "escape was echoed: {error}");
        }
        assert_eq!(guard_rel_path("a/./b//c.png").unwrap(), "a/b/c.png");

        // Existing-file reads do not inherit upload's cross-platform *creation* policy.
        for valid_posix_name in ["CON", "report.", "photo\u{202e}gnp.exe"] {
            assert!(guard_rel_path(valid_posix_name).is_ok(), "{valid_posix_name:?}");
        }
    }

    fn fetched_local(root: &Path, scratch: &Path) -> LocalFsSource {
        LocalFsSource::registered(
            root.canonicalize().unwrap(),
            Some(scratch.to_path_buf()),
        )
    }

    #[test]
    fn local_fetch_materialises_from_the_open_capability_in_configured_scratch() {
        let root = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("asset.bin"), b"inside").unwrap();
        let source = fetched_local(root.path(), scratch.path());
        let fetched = source.fetch("asset.bin").unwrap();
        assert_eq!(std::fs::read(fetched.path()).unwrap(), b"inside");
        assert_eq!(fetched.path().parent(), Some(scratch.path()));
    }

    #[test]
    #[cfg(unix)]
    fn local_fetch_rejects_symlinked_files_and_directories_without_disclosure() {
        let root = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("do-not-disclose.bin");
        std::fs::write(&secret, b"outside secret bytes").unwrap();
        std::os::unix::fs::symlink(&secret, root.path().join("file-link.bin")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("dir-link")).unwrap();
        let source = fetched_local(root.path(), scratch.path());

        for rel in ["file-link.bin", "dir-link/do-not-disclose.bin"] {
            let error = source.fetch(rel).err().expect("escape must be rejected");
            assert!(matches!(error, LibError::BadRequest(_)), "{rel}: {error}");
            let rendered = error.to_string();
            assert!(!rendered.contains(&outside.path().to_string_lossy().to_string()));
            assert!(!rendered.contains("outside secret bytes"));
        }
    }

    /// Deterministically swap a checked directory for an external symlink after lexical validation
    /// but before the capability-relative open. This is the check/open race issue #124 reported.
    #[test]
    #[cfg(unix)]
    fn a_swap_between_validation_and_open_cannot_retarget_fetch() {
        let root = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("models")).unwrap();
        std::fs::write(root.path().join("models/scene.bin"), b"inside").unwrap();
        std::fs::write(outside.path().join("scene.bin"), b"outside secret").unwrap();
        let source = fetched_local(root.path(), scratch.path());
        let rel = guard_rel_path("models/scene.bin").unwrap();
        let parked = root.path().join("models-parked");

        let error = source
            .fetch_after_validation(&rel, || {
                std::fs::rename(root.path().join("models"), &parked).unwrap();
                std::os::unix::fs::symlink(outside.path(), root.path().join("models")).unwrap();
            })
            .err()
            .expect("swap must be rejected");
        assert!(matches!(error, LibError::BadRequest(_)), "{error}");
        assert!(!error.to_string().contains("outside secret"));

        std::fs::remove_file(root.path().join("models")).unwrap();
        std::fs::rename(parked, root.path().join("models")).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn replacing_the_registered_root_path_cannot_retarget_reads_or_writes() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = parent.path().join("source");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("asset.bin"), b"inside").unwrap();
        let registered_root = root.canonicalize().unwrap();
        let parked = parent.path().join("source-parked");
        std::fs::rename(&root, &parked).unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();

        // Rebuilding a backend from the persisted canonical connection refuses the replacement.
        let source = LocalFsSource::registered(
            registered_root,
            Some(scratch.path().to_path_buf()),
        );
        assert!(matches!(source.fetch("asset.bin"), Err(LibError::SourceUnavailable(_))));
        assert!(source.put("stolen.bin", &mut &b"x"[..]).is_err());
        assert!(!outside.path().join("stolen.bin").exists());

        std::fs::remove_file(&root).unwrap();
        std::fs::rename(parked, root).unwrap();
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
    /// It must also answer *without writing anything*, since it runs on every source listing and a
    /// create-then-delete inside a watched root makes the watcher rescan the source — see the
    /// second half of this test.
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

        // The probe must not *touch* the tree, not merely tidy up after itself. This runs on every
        // source listing, and `Create(File)` + `Remove(File)` both pass the watcher's
        // `is_content_change` filter — so a create-then-delete probe inside a watched root triggers
        // a debounced delta rescan, whose completion invalidates the client's source list, which
        // refetches, which probes again. A folder nobody touched rescans forever.
        assert!(src.writable());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert!(
            leftovers.is_empty(),
            "the probe touched the tree: {leftovers:?}"
        );
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
        let src = LocalFsSource::without_scratch(dir.path());
        (dir, src)
    }

    /// An uploaded asset must be readable by more than the server's own uid.
    ///
    /// `tempfile` creates at 0600 and `persist` keeps the mode, so without an explicit widening a
    /// file uploaded into a shared project folder is one no teammate — and no DCC tool running as
    /// another user — can open. Only running a real upload and looking at the result surfaces this.
    #[test]
    #[cfg(unix)]
    fn put_leaves_a_file_others_can_read() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, src) = local_root();
        src.put("brick.png", &mut &b"pixels"[..]).unwrap();
        let mode = std::fs::metadata(dir.path().join("brick.png"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "got {mode:o}");
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
        let mkdir_err = src.mkdir("escape/stolen-folder").unwrap_err();
        assert!(matches!(mkdir_err, LibError::BadRequest(_)), "got {mkdir_err:?}");
        assert!(
            !outside.path().join("stolen.png").exists()
                && !outside.path().join("stolen-folder").exists(),
            "neither file nor directory creation may escape the source root"
        );
    }

    /// The write counterpart to the fetch race: validation succeeds while `uploads` is an ordinary
    /// in-root directory, then an attacker replaces it before the first create. Both the staging
    /// create and final no-clobber publish remain relative to the pinned root capability.
    #[test]
    #[cfg(unix)]
    fn a_swap_between_validation_and_create_cannot_retarget_upload() {
        let (dir, src) = local_root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("uploads")).unwrap();
        let parked = dir.path().join("uploads-parked");
        let rel = safe_name::check_rel_path("uploads/stolen.bin").unwrap();

        let error = src
            .put_after_validation(&rel, &mut &b"secret"[..], || {
                std::fs::rename(dir.path().join("uploads"), &parked).unwrap();
                std::os::unix::fs::symlink(outside.path(), dir.path().join("uploads")).unwrap();
            })
            .unwrap_err();
        assert!(matches!(error, LibError::BadRequest(_)), "{error}");
        assert!(!outside.path().join("stolen.bin").exists());
        assert!(
            std::fs::read_dir(outside.path()).unwrap().next().is_none(),
            "no staging file may escape either"
        );

        std::fs::remove_file(dir.path().join("uploads")).unwrap();
        std::fs::rename(parked, dir.path().join("uploads")).unwrap();
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

    /// A backend that has not opted in stays read-only — the default that makes *adding* a source
    /// kind safe rather than accidentally writable. (SFTP and SMB have since opted in explicitly;
    /// this pins the default they had to override, not their current answer.)
    #[test]
    fn a_backend_that_does_not_opt_in_is_read_only() {
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
