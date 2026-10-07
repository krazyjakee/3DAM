//! Test-only support: an in-process SSH+SFTP server backed by a real directory.
//!
//! Lives under `tests/`, so it is compiled **only** for the test binaries — nothing here ships in
//! the library. It exists because the SFTP backend (`src/sftp.rs`) previously had zero coverage:
//! exercising it needs a real server on the other end of a real socket, and standing up `sshd` in
//! CI is not an option. `russh` and `russh-sftp` both carry server halves, so we run one on
//! `127.0.0.1:0` (ephemeral port) with an ephemeral host key and a fixed password login.
//!
//! The SFTP handler is a straightforward mapping onto `std::fs` rooted at a caller-supplied
//! directory, so a test asserts against **files on disk** rather than against a mock's bookkeeping.
//! `OpenFlags::CREATE|EXCLUDE` maps to `create_new(true)` (via russh-sftp's own
//! `OpenFlags -> OpenOptions` conversion), which is the create-if-absent semantics the upload path
//! relies on.

#![allow(dead_code)] // a shared support module is used piecemeal by each test binary

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

/// A running SSH+SFTP server, serving a real directory over loopback.
///
/// Dropping it shuts the server (and its runtime) down.
pub struct TestSftpServer {
    host: String,
    port: u16,
    root: PathBuf,
    username: String,
    password: String,
    pub listing: Arc<ListingStats>,
    /// Owns the server task; dropped last, which tears the listener down.
    _rt: Runtime,
}

/// Wire-level listing counters and fault injection, shared across SSH subsystem channels.
#[derive(Default)]
pub struct ListingStats {
    pub opens: AtomicUsize,
    pub reads: AtomicUsize,
    pub closes: AtomicUsize,
    /// Fail this numbered READDIR request; zero disables injection.
    pub fail_read: AtomicUsize,
    /// Force content READ replies to contain at most this many bytes; zero keeps requested size.
    pub content_read_limit: AtomicUsize,
}

impl TestSftpServer {
    /// Start a server on `127.0.0.1:0` serving `root`, accepting `test`/`test`.
    pub fn start(root: impl AsRef<Path>) -> TestSftpServer {
        Self::start_with_credentials(root, "test", "test")
    }

    /// As [`TestSftpServer::start`], with explicit credentials.
    pub fn start_with_credentials(
        root: impl AsRef<Path>,
        username: &str,
        password: &str,
    ) -> TestSftpServer {
        Self::try_start_with_credentials_on(root, username, password, "127.0.0.1:0")
            .expect("bind IPv4 SFTP test server")
    }

    /// Start on IPv6 loopback, or return `None` on hosts where IPv6 is unavailable.
    pub fn start_ipv6(root: impl AsRef<Path>) -> Option<TestSftpServer> {
        Self::try_start_with_credentials_on(root, "test", "test", "[::1]:0")
    }

    fn try_start_with_credentials_on(
        root: impl AsRef<Path>,
        username: &str,
        password: &str,
        bind_address: &str,
    ) -> Option<TestSftpServer> {
        let root = root
            .as_ref()
            .canonicalize()
            .expect("sftp test server root must exist");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build sftp test server runtime");

        let listener = rt.block_on(TcpListener::bind(bind_address)).ok()?;
        let address = listener.local_addr().expect("local_addr");
        let host = address.ip().to_string();
        let port = address.port();

        let config = Arc::new(russh::server::Config {
            // Keep rejections snappy: a test that mistypes a password shouldn't sit for a second.
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::from_millis(0)),
            // Ephemeral host key — never written to disk, regenerated per server.
            keys: vec![
                PrivateKey::random(&mut OsRng, Algorithm::Ed25519).expect("generate host key")
            ],
            inactivity_timeout: Some(Duration::from_secs(60)),
            nodelay: true,
            ..Default::default()
        });

        let listing = Arc::new(ListingStats::default());
        let mut server = SshServer {
            root: root.clone(),
            username: username.to_string(),
            password: password.to_string(),
            listing: listing.clone(),
        };
        rt.spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });

        Some(TestSftpServer {
            host,
            port,
            root,
            username: username.to_string(),
            password: password.to_string(),
            listing,
            _rt: rt,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// The (canonicalised) directory the server serves. Assert against this.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    /// An [`SftpConfig`](dam_sources::SftpConfig) pointing at this server, rooted at `/`.
    pub fn config(&self) -> dam_sources::SftpConfig {
        self.config_at("/")
    }

    /// As [`TestSftpServer::config`], with a `base_path` other than the served root.
    pub fn config_at(&self, base_path: &str) -> dam_sources::SftpConfig {
        dam_sources::SftpConfig {
            host: self.host().to_string(),
            port: self.port,
            username: self.username.clone(),
            base_path: base_path.to_string(),
            password: Some(self.password.clone()),
            private_key: None,
            passphrase: None,
            credential_ref: None,
        }
    }

    /// Open an `SftpSource` against this server. `SftpSource` is private to the crate, so this
    /// goes through the public `open_source` seam and hands back the trait object.
    pub fn connect(
        &self,
        scratch: &Path,
    ) -> Result<Box<dyn dam_sources::FileSource>, dam_api::LibError> {
        dam_sources::open_source(&dam_sources::SourceConnection::Sftp(self.config()), scratch)
    }
}

// ---------------------------------------------------------------------------
// SSH layer
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SshServer {
    root: PathBuf,
    username: String,
    password: String,
    listing: Arc<ListingStats>,
}

impl russh::server::Server for SshServer {
    type Handler = SshSession;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> SshSession {
        SshSession {
            channels: Arc::new(Mutex::new(HashMap::new())),
            root: self.root.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            listing: self.listing.clone(),
        }
    }
}

struct SshSession {
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
    root: PathBuf,
    username: String,
    password: String,
    listing: Arc<ListingStats>,
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == self.username && password == self.password {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        Ok(true)
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let channel = match self.channels.lock().await.remove(&channel_id) {
            Some(c) => c,
            None => {
                session.channel_failure(channel_id)?;
                return Ok(());
            }
        };
        session.channel_success(channel_id)?;
        russh_sftp::server::run(
            channel.into_stream(),
            FsSftpHandler::new(self.root.clone(), self.listing.clone()),
        )
        .await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SFTP layer — a thin mapping onto std::fs, chrooted at `root`
// ---------------------------------------------------------------------------

enum OpenHandle {
    Dir {
        /// Remaining entries; returned in bounded pages, then EOF.
        pending: Vec<File>,
    },
    File(fs::File),
}

struct FsSftpHandler {
    root: PathBuf,
    handles: HashMap<String, OpenHandle>,
    next_handle: u64,
    listing: Arc<ListingStats>,
}

impl FsSftpHandler {
    fn new(root: PathBuf, listing: Arc<ListingStats>) -> Self {
        FsSftpHandler {
            root,
            handles: HashMap::new(),
            next_handle: 0,
            listing,
        }
    }

    fn fresh_handle(&mut self) -> String {
        self.next_handle += 1;
        format!("h{}", self.next_handle)
    }

    /// Split a remote path into normalised segments, refusing to escape the root.
    fn segments(path: &str) -> Result<Vec<String>, StatusCode> {
        let mut out: Vec<String> = Vec::new();
        for seg in path.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    if out.pop().is_none() {
                        // `..` above the served root: the real thing would leak the host FS.
                        return Err(StatusCode::PermissionDenied);
                    }
                }
                s => out.push(s.to_string()),
            }
        }
        Ok(out)
    }

    /// Remote path → absolute path on disk under `root`.
    fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let mut p = self.root.clone();
        for seg in Self::segments(path)? {
            p.push(seg);
        }
        Ok(p)
    }

    /// Remote path → its canonical `/`-rooted remote form (for `realpath`).
    fn canonical(path: &str) -> Result<String, StatusCode> {
        let segs = Self::segments(path)?;
        Ok(if segs.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", segs.join("/"))
        })
    }

    fn file_mut(&mut self, handle: &str) -> Result<&mut fs::File, StatusCode> {
        match self.handles.get_mut(handle) {
            Some(OpenHandle::File(f)) => Ok(f),
            Some(OpenHandle::Dir { .. }) => Err(StatusCode::Failure),
            None => Err(StatusCode::NoSuchFile),
        }
    }

    fn ok(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        }
    }
}

/// io::Error → the closest SFTP status. `AlreadyExists` maps to `Failure` because SFTPv3 has no
/// dedicated code — that is what a real server reports for a failed `CREATE|EXCLUDE`.
fn io_status(err: &std::io::Error) -> StatusCode {
    match err.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl russh_sftp::server::Handler for FsSftpHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(Self::canonical(&path)?)],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        self.listing.opens.fetch_add(1, Ordering::Relaxed);
        let dir = self.resolve(&path)?;
        let mut files = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| io_status(&e))? {
            let entry = entry.map_err(|e| io_status(&e))?;
            let meta = match entry.metadata() {
                Ok(m) => m,
                // Fail-soft: a vanished entry mid-listing shouldn't kill the whole readdir.
                Err(_) => continue,
            };
            files.push(File::new(
                entry.file_name().to_string_lossy().to_string(),
                FileAttributes::from(&meta),
            ));
        }
        let handle = self.fresh_handle();
        self.handles
            .insert(handle.clone(), OpenHandle::Dir { pending: files });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let request = self.listing.reads.fetch_add(1, Ordering::Relaxed) + 1;
        if self.listing.fail_read.load(Ordering::Relaxed) == request {
            return Err(StatusCode::Failure);
        }
        match self.handles.get_mut(&handle) {
            Some(OpenHandle::Dir { pending }) if !pending.is_empty() => Ok(Name {
                id,
                files: pending.drain(..pending.len().min(32)).collect(),
            }),
            // Empty dir, or everything already sent: the spec says signal EOF.
            Some(OpenHandle::Dir { .. }) => Err(StatusCode::Eof),
            Some(OpenHandle::File(_)) => Err(StatusCode::Failure),
            None => Err(StatusCode::NoSuchFile),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.resolve(&filename)?;
        // russh-sftp's own conversion already maps CREATE|EXCLUDE → create_new(true).
        let mut opts: fs::OpenOptions = pflags.into();
        if !pflags.contains(OpenFlags::READ) && !pflags.contains(OpenFlags::WRITE) {
            opts.read(true);
        }
        let file = opts.open(&path).map_err(|e| io_status(&e))?;
        let handle = self.fresh_handle();
        self.handles.insert(handle.clone(), OpenHandle::File(file));
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        // A close for an unknown handle is not worth an error: the client drops handles
        // fire-and-forget (`close_nowait`), so races are normal.
        if matches!(self.handles.remove(&handle), Some(OpenHandle::Dir { .. })) {
            self.listing.closes.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Self::ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let read_limit = self.listing.content_read_limit.load(Ordering::Relaxed);
        let len = if read_limit == 0 {
            len as usize
        } else {
            (len as usize).min(read_limit)
        };
        let file = self.file_mut(&handle)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| io_status(&e))?;
        let mut buf = vec![0u8; len];
        let mut filled = 0usize;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_status(&e)),
            }
        }
        if filled == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(filled);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let file = self.file_mut(&handle)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| io_status(&e))?;
        file.write_all(&data).map_err(|e| io_status(&e))?;
        Ok(Self::ok(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let p = self.resolve(&path)?;
        let meta = fs::metadata(&p).map_err(|e| io_status(&e))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let p = self.resolve(&path)?;
        let meta = fs::symlink_metadata(&p).map_err(|e| io_status(&e))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let file = self.file_mut(&handle)?;
        let meta = file.metadata().map_err(|e| io_status(&e))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        // Accepted and ignored: clients set mode/mtime after an upload and treat a failure as fatal.
        Ok(Self::ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        _handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(Self::ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let p = self.resolve(&path)?;
        // Deliberately not `create_dir_all`: "already exists" must be observable.
        fs::create_dir(&p).map_err(|e| io_status(&e))?;
        Ok(Self::ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let p = self.resolve(&path)?;
        fs::remove_dir(&p).map_err(|e| io_status(&e))?;
        Ok(Self::ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let p = self.resolve(&filename)?;
        fs::remove_file(&p).map_err(|e| io_status(&e))?;
        Ok(Self::ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let from = self.resolve(&oldpath)?;
        let to = self.resolve(&newpath)?;
        // **Fail if the destination exists.** `std::fs::rename` replaces it on Unix; SSH_FXP_RENAME
        // is specified not to (draft-ietf-secsh-filexfer-02 §6.5), and OpenSSH implements the
        // refusal. Using the POSIX behaviour here would make the harness demonstrate the exact
        // data-loss path `SftpSource::put` names as load-bearing, while the suite stayed green —
        // the create-only assertion would be pinning the client's pre-check and nothing else.
        if to.exists() {
            return Err(StatusCode::Failure);
        }
        fs::rename(&from, &to).map_err(|e| io_status(&e))?;
        Ok(Self::ok(id))
    }
}
