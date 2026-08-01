//! Host-local storage for outbound source credentials (issue #103).
//!
//! The portable catalog carries only `source.auth_ref`. Desktop/interactive roles resolve that
//! opaque key through the native OS credential store. A headless deployment may explicitly set
//! `3DAM_SOURCE_SECRET_DIR` to a service-owned, non-portable directory; that backend refuses symlinks,
//! group/world-accessible Unix permissions, and locations inside the library data directory.

use dam_api::id::SourceId;
use dam_api::LibError;
use dam_sources::{SourceConnection, SourceCredentials};
use dam_store::Store;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
const KEYRING_SERVICE: &str = "3dam-source";
const FILE_BACKEND_ENV: &str = "3DAM_SOURCE_SECRET_DIR";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecretError {
    Missing,
    Locked,
    Unavailable,
    Invalid,
}

trait SecretBackend: Send + Sync {
    fn put(&self, credential_ref: &str, value: &str) -> Result<(), SecretError>;
    fn get(&self, credential_ref: &str) -> Result<String, SecretError>;
    fn delete(&self, credential_ref: &str) -> Result<(), SecretError>;
}

#[cfg(test)]
#[derive(Default)]
struct MemoryBackend {
    values: Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(test)]
impl SecretBackend for MemoryBackend {
    fn put(&self, credential_ref: &str, value: &str) -> Result<(), SecretError> {
        self.values
            .lock()
            .map_err(|_| SecretError::Unavailable)?
            .insert(credential_ref.into(), value.into());
        Ok(())
    }

    fn get(&self, credential_ref: &str) -> Result<String, SecretError> {
        self.values
            .lock()
            .map_err(|_| SecretError::Unavailable)?
            .get(credential_ref)
            .cloned()
            .ok_or(SecretError::Missing)
    }

    fn delete(&self, credential_ref: &str) -> Result<(), SecretError> {
        self.values
            .lock()
            .map_err(|_| SecretError::Unavailable)?
            .remove(credential_ref);
        Ok(())
    }
}

/// A serialised-access wrapper around the platform backend. `keyring` documents that rapid access
/// from multiple threads is unreliable on some Windows/Linux providers, so the mutex is a security
/// and correctness boundary rather than a performance concern (source opens are coarse-grained).
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
struct KeyringBackend {
    access: Mutex<()>,
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl KeyringBackend {
    fn new() -> Self {
        Self {
            access: Mutex::new(()),
        }
    }

    fn entry(credential_ref: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(KEYRING_SERVICE, credential_ref).map_err(classify_keyring)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl SecretBackend for KeyringBackend {
    fn put(&self, credential_ref: &str, value: &str) -> Result<(), SecretError> {
        let _guard = self.access.lock().map_err(|_| SecretError::Unavailable)?;
        Self::entry(credential_ref)?
            .set_password(value)
            .map_err(classify_keyring)
    }

    fn get(&self, credential_ref: &str) -> Result<String, SecretError> {
        let _guard = self.access.lock().map_err(|_| SecretError::Unavailable)?;
        Self::entry(credential_ref)?
            .get_password()
            .map_err(classify_keyring)
    }

    fn delete(&self, credential_ref: &str) -> Result<(), SecretError> {
        let _guard = self.access.lock().map_err(|_| SecretError::Unavailable)?;
        match Self::entry(credential_ref)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(classify_keyring(error)),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn classify_keyring(error: keyring::Error) -> SecretError {
    match error {
        keyring::Error::NoEntry => SecretError::Missing,
        keyring::Error::NoStorageAccess(_) => SecretError::Locked,
        keyring::Error::BadEncoding(_) => SecretError::Invalid,
        _ => SecretError::Unavailable,
    }
}

/// Permission-contained file backend for services without a desktop secret-service session.
/// Values are plaintext because the OS account boundary is the encryption boundary in a headless
/// service: Unix mode 0700 on the directory and 0600 on each file. An attacker with that account or
/// root can also inspect 3DAM process memory, so application-level encryption with a colocated key
/// would not strengthen the threat model.
struct FileBackend {
    root: PathBuf,
    access: Mutex<()>,
}

impl FileBackend {
    fn open(root: &Path, data_dir: &Path) -> Result<Self, LibError> {
        if !root.is_absolute() {
            return Err(secret_configuration_error(
                "3DAM_SOURCE_SECRET_DIR must be an absolute path",
            ));
        }
        std::fs::create_dir_all(root).map_err(|_| {
            secret_configuration_error("could not create the source credential directory")
        })?;
        let metadata = std::fs::symlink_metadata(root).map_err(|_| {
            secret_configuration_error("could not inspect the source credential directory")
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(secret_configuration_error(
                "source credential directory must be a real directory, not a symlink",
            ));
        }
        secure_directory_permissions(root, &metadata)?;

        let canonical_root = root.canonicalize().map_err(|_| {
            secret_configuration_error("could not resolve the source credential directory")
        })?;
        let canonical_data = data_dir.canonicalize().map_err(|_| {
            secret_configuration_error("could not resolve the library data directory")
        })?;
        if canonical_root.starts_with(&canonical_data) {
            return Err(secret_configuration_error(
                "source credentials must live outside the portable library data directory",
            ));
        }
        Ok(Self {
            root: canonical_root,
            access: Mutex::new(()),
        })
    }

    fn path(&self, credential_ref: &str) -> PathBuf {
        // A fixed-width digest makes arbitrary/legacy opaque refs safe as filenames and avoids
        // exposing source UUIDs in directory listings.
        self.root.join(format!(
            "{}.secret",
            blake3::hash(credential_ref.as_bytes()).to_hex()
        ))
    }
}

impl SecretBackend for FileBackend {
    fn put(&self, credential_ref: &str, value: &str) -> Result<(), SecretError> {
        use std::io::Write;
        let _guard = self.access.lock().map_err(|_| SecretError::Unavailable)?;
        let path = self.path(credential_ref);
        let mut staged = tempfile::Builder::new()
            .prefix(".3dam-source-secret-")
            .tempfile_in(&self.root)
            .map_err(|_| SecretError::Unavailable)?;
        set_secret_file_permissions(staged.as_file()).map_err(|_| SecretError::Unavailable)?;
        staged
            .write_all(value.as_bytes())
            .and_then(|_| staged.as_file().sync_all())
            .map_err(|_| SecretError::Unavailable)?;
        staged
            .persist(&path)
            .map_err(|_| SecretError::Unavailable)?;
        // `fsync(file)` protects the bytes; syncing the containing directory protects the renamed
        // directory entry. Only after both succeed may migration safely redact the database row.
        std::fs::File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| SecretError::Unavailable)?;
        Ok(())
    }

    fn get(&self, credential_ref: &str) -> Result<String, SecretError> {
        let _guard = self.access.lock().map_err(|_| SecretError::Unavailable)?;
        let path = self.path(credential_ref);
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SecretError::Missing
            } else {
                SecretError::Unavailable
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SecretError::Invalid);
        }
        validate_secret_file_permissions(&metadata)?;
        std::fs::read_to_string(path).map_err(|_| SecretError::Unavailable)
    }

    fn delete(&self, credential_ref: &str) -> Result<(), SecretError> {
        let _guard = self.access.lock().map_err(|_| SecretError::Unavailable)?;
        match std::fs::remove_file(self.path(credential_ref)) {
            Ok(()) => std::fs::File::open(&self.root)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| SecretError::Unavailable),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(SecretError::Unavailable),
        }
    }
}

#[cfg(unix)]
fn secure_directory_permissions(path: &Path, metadata: &std::fs::Metadata) -> Result<(), LibError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(secret_configuration_error(
            "source credential directory must be owned by the service account",
        ));
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions).map_err(|_| {
        secret_configuration_error("could not restrict the source credential directory to mode 0700")
    })?;
    let mode = std::fs::metadata(path)
        .map_err(|_| secret_configuration_error("could not verify source credential permissions"))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(secret_configuration_error(
            "source credential directory must not be accessible by group or other users",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory_permissions(_path: &Path, _metadata: &std::fs::Metadata) -> Result<(), LibError> {
    Err(secret_configuration_error(
        "the file credential backend is supported only on Unix; use the Windows credential store",
    ))
}

#[cfg(unix)]
fn set_secret_file_permissions(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_secret_file_permissions(_file: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "file secret backend requires Unix permissions",
    ))
}

#[cfg(unix)]
fn validate_secret_file_permissions(metadata: &std::fs::Metadata) -> Result<(), SecretError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let owned = metadata.uid() == unsafe { libc::geteuid() };
    (owned && metadata.permissions().mode() & 0o777 == 0o600)
        .then_some(())
        .ok_or(SecretError::Invalid)
}

#[cfg(not(unix))]
fn validate_secret_file_permissions(_metadata: &std::fs::Metadata) -> Result<(), SecretError> {
    Err(SecretError::Unavailable)
}

fn secret_configuration_error(message: &str) -> LibError {
    LibError::SourceUnavailable(format!("source credential store unavailable: {message}"))
}

/// Cloneable resolver shared by request paths, background workers, and source watchers.
#[derive(Clone)]
pub(crate) struct SecretVault {
    backend: Arc<dyn SecretBackend>,
}

impl SecretVault {
    pub(crate) fn for_host(data_dir: &Path) -> Result<Self, LibError> {
        if let Some(root) = std::env::var_os(FILE_BACKEND_ENV) {
            return Ok(Self {
                backend: Arc::new(FileBackend::open(Path::new(&root), data_dir)?),
            });
        }
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            return Ok(Self {
                backend: Arc::new(KeyringBackend::new()),
            });
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        Err(secret_configuration_error(
            "no native credential-store backend is available on this platform",
        ))
    }

    #[cfg(all(test, unix))]
    fn file(root: &Path, data_dir: &Path) -> Result<Self, LibError> {
        Ok(Self {
            backend: Arc::new(FileBackend::open(root, data_dir)?),
        })
    }

    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        Self {
            backend: Arc::new(MemoryBackend::default()),
        }
    }

    pub(crate) fn reference(source: &SourceId) -> String {
        format!("3dam.source.{source}")
    }

    pub(crate) fn put(
        &self,
        credential_ref: &str,
        credentials: &SourceCredentials,
    ) -> Result<(), LibError> {
        let value = serde_json::to_string(credentials).map_err(|_| {
            LibError::Internal("could not encode source credential material".into())
        })?;
        self.backend
            .put(credential_ref, &value)
            .map_err(secret_operation_error)
    }

    pub(crate) fn resolve(&self, mut connection: SourceConnection) -> Result<SourceConnection, LibError> {
        let Some(credential_ref) = connection.credential_ref().map(str::to_owned) else {
            return Ok(connection);
        };
        let value = self
            .backend
            .get(&credential_ref)
            .map_err(secret_operation_error)?;
        let credentials: SourceCredentials = serde_json::from_str(&value)
            .map_err(|_| secret_operation_error(SecretError::Invalid))?;
        connection.apply_credentials(credentials)?;
        Ok(connection)
    }

    pub(crate) fn delete(&self, credential_ref: &str) -> Result<(), LibError> {
        self.backend
            .delete(credential_ref)
            .map_err(secret_operation_error)
    }
}

fn secret_operation_error(error: SecretError) -> LibError {
    let message = match error {
        SecretError::Missing => "source credentials are missing; re-enter them",
        SecretError::Locked => "source credential store is locked; unlock it and retry",
        SecretError::Unavailable => "source credential store is unavailable; retry or configure the headless backend",
        SecretError::Invalid => "source credential entry is invalid; re-enter the source credentials",
    };
    LibError::SourceUnavailable(message.into())
}

/// Extract all credentials from legacy connection JSON. All secret entries are written first and
/// all SQLite rows are then redacted in one transaction. Any pre-commit failure leaves the old rows
/// untouched and open fails explicitly, so retrying after unlocking/fixing the backend cannot lose
/// source availability.
pub(crate) fn migrate_legacy_credentials(store: &Store, vault: &SecretVault) -> Result<(), LibError> {
    if store.source_credentials_migrated()? {
        return Ok(());
    }
    let mut updates = Vec::new();
    let rows = store.source_connections()?;
    for (source, mut connection, _stored_ref) in rows.iter().cloned() {
        let Some(credentials) = connection.take_credentials() else {
            continue;
        };
        // Canonical deterministic refs cannot contain legacy/user-controlled credential material.
        let credential_ref = SecretVault::reference(&source);
        vault.put(&credential_ref, &credentials)?;
        updates.push((source, connection, credential_ref));
    }
    if !updates.is_empty() {
        store.rewrite_source_credentials(&updates)?;
        tracing::info!(sources = updates.len(), "migrated source credentials out of library.db");
    } else if !rows.is_empty() {
        // Retry path: a prior process may have committed the redacted rows but failed during the
        // physical free-page/WAL scrub before it could set the completion marker.
        store.scrub_source_storage()?;
    }
    store.mark_source_credentials_migrated()?;
    Ok(())
}

/// Drain durable opaque-ref tombstones created by source removal/catalog reset. Deleting the host
/// secret first and acknowledging the row second is retry-safe: a crash in between repeats an
/// idempotent delete, while a locked backend leaves the reference queued for the next open.
pub(crate) fn cleanup_pending_credentials(store: &Store, vault: &SecretVault) -> Result<(), LibError> {
    for credential_ref in store.pending_source_credential_cleanup()? {
        vault.delete(&credential_ref)?;
        store.complete_source_credential_cleanup(&credential_ref)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::dto::{ExportFormat, ExportRequest, MediaType};
    use dam_api::service::Visibility;
    use dam_sources::{SftpConfig, SourceConnection};
    use dam_store::NewAsset;
    use rusqlite::Connection;

    struct LockedBackend;

    impl SecretBackend for LockedBackend {
        fn put(&self, _credential_ref: &str, _value: &str) -> Result<(), SecretError> {
            Err(SecretError::Locked)
        }
        fn get(&self, _credential_ref: &str) -> Result<String, SecretError> {
            Err(SecretError::Locked)
        }
        fn delete(&self, _credential_ref: &str) -> Result<(), SecretError> {
            Err(SecretError::Locked)
        }
    }

    fn legacy_sftp(secret: &str) -> SourceConnection {
        SourceConnection::Sftp(SftpConfig {
            host: "example.invalid".into(),
            port: 22,
            username: "legacy".into(),
            base_path: "/assets".into(),
            password: Some(secret.into()),
            private_key: Some("/private/legacy/id_ed25519".into()),
            passphrase: Some("legacy-passphrase".into()),
            credential_ref: None,
        })
    }

    #[test]
    fn legacy_migration_redacts_raw_rows_and_preserves_resolution() {
        const SENTINEL: &str = "issue-103-legacy-sentinel";
        let data = tempfile::tempdir().unwrap();
        let source = {
            let store = Store::open(data.path()).unwrap();
            let mut redacted = legacy_sftp(SENTINEL);
            let _ = redacted.take_credentials();
            store.add_source(&redacted, "legacy", false).unwrap()
        };
        // Recreate the exact pre-103 state: the connection JSON contains plaintext and auth_ref is
        // NULL. Opening the current engine must extract it before normal source use begins.
        let legacy_json = serde_json::json!({
            "kind": "sftp",
            "host": "example.invalid",
            "port": 22,
            "username": "legacy",
            "base_path": "/assets",
            "password": SENTINEL,
            "private_key": "/private/legacy/id_ed25519",
            "passphrase": "legacy-passphrase"
        })
        .to_string();
        Connection::open(data.path().join("library.db"))
            .unwrap()
            .execute(
                "UPDATE source SET connection = ?2, auth_ref = NULL WHERE id = ?1",
                rusqlite::params![source.as_bytes().to_vec(), legacy_json],
            )
            .unwrap();

        let store = Store::open(data.path()).unwrap();
        let vault = SecretVault::memory();
        migrate_legacy_credentials(&store, &vault).unwrap();

        let (connection_json, auth_ref): (String, String) = Connection::open(data.path().join("library.db"))
            .unwrap()
            .query_row(
                "SELECT connection, auth_ref FROM source WHERE id = ?1",
                rusqlite::params![source.as_bytes().to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(auth_ref, SecretVault::reference(&source));
        assert!(!connection_json.contains(SENTINEL));
        assert!(!connection_json.contains("password"));

        for forbidden in [SENTINEL, "/private/legacy", "legacy-passphrase"] {
            for suffix in ["", "-wal", "-shm"] {
                let path = data.path().join(format!("library.db{suffix}"));
                if let Ok(raw) = std::fs::read(&path) {
                    assert!(
                        !raw.windows(forbidden.len()).any(|window| window == forbidden.as_bytes()),
                        "SQLite artifact {} leaked {forbidden}",
                        path.display()
                    );
                }
            }
        }
        let hydrated = vault.resolve(store.get_source_connection(&source).unwrap()).unwrap();
        let SourceConnection::Sftp(config) = hydrated else { panic!() };
        assert_eq!(config.password.as_deref(), Some(SENTINEL));
        assert_eq!(config.passphrase.as_deref(), Some("legacy-passphrase"));

        // Metadata export walks assets/source attribution but must never acquire or serialise the
        // credential payload. A sentinel-bearing remote source makes this a regression guard rather
        // than an assertion over an unrelated local-only library.
        let (asset, _) = store
            .upsert_asset(&NewAsset {
                source_id: source,
                path: "textures/brick.png".into(),
                filename: "brick.png".into(),
                content_hash: None,
                size_bytes: Some(1),
                source_modified_at: None,
                scanned_at: dam_store::now_ms(),
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        let manifest = data.path().join("manifest.json");
        crate::export::run_export(
            &store,
            ExportRequest {
                assets: vec![asset],
                format: ExportFormat::Json,
                output: manifest.to_string_lossy().into_owned(),
                ..ExportRequest::default()
            },
            &Visibility::Full,
        )
        .unwrap();
        let artifact = std::fs::read(manifest).unwrap();
        assert!(!artifact.windows(SENTINEL.len()).any(|window| window == SENTINEL.as_bytes()));
    }

    #[cfg(unix)]
    #[test]
    fn file_backend_is_nonportable_and_owner_only() {
        let data = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("source-secrets");
        let vault = SecretVault::file(&root, data.path()).unwrap();
        let reference = SecretVault::reference(&SourceId::new());
        vault.put(&reference, &SourceCredentials::Federated { token: "sentinel".into() }).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
            let entry = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap();
            assert_eq!(entry.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(SecretVault::file(&data.path().join("secrets"), data.path()).is_err());
    }

    #[test]
    fn catalog_reset_durably_deletes_host_credentials() {
        let store = Store::open_in_memory().unwrap();
        let vault = SecretVault::memory();
        let source = SourceId::new();
        let reference = SecretVault::reference(&source);
        let connection = SourceConnection::Federated(dam_sources::FederatedConfig {
            endpoint: "https://peer.invalid".into(),
            token: None,
            credential_ref: None,
        });
        vault
            .put(
                &reference,
                &SourceCredentials::Federated {
                    token: "reset-sentinel".into(),
                },
            )
            .unwrap();
        store
            .add_source_with_auth(source, &connection, "peer", false, Some(&reference))
            .unwrap();

        store.wipe_catalog().unwrap();
        assert_eq!(store.pending_source_credential_cleanup().unwrap(), vec![reference.clone()]);
        cleanup_pending_credentials(&store, &vault).unwrap();
        assert!(store.pending_source_credential_cleanup().unwrap().is_empty());
        assert_eq!(vault.backend.get(&reference), Err(SecretError::Missing));
    }

    #[test]
    fn locked_store_error_is_explicit_and_redacted() {
        let source = SourceId::new();
        let reference = SecretVault::reference(&source);
        let vault = SecretVault {
            backend: Arc::new(LockedBackend),
        };
        let mut connection = legacy_sftp("must-not-appear");
        let _ = connection.take_credentials();
        connection.set_credential_ref(Some(reference.clone()));
        let error = vault.resolve(connection).unwrap_err().to_string();
        assert!(error.contains("locked"));
        assert!(!error.contains("must-not-appear"));
        assert!(!error.contains(&reference));
    }
}
