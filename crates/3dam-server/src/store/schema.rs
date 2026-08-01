//! Forward-only migrations for the host-local `server.db`.
//!
//! This database predates schema versioning, so V1-V3 deliberately use `IF NOT EXISTS`: an
//! unversioned database from any shipped era can enter the sequence without replacing tables or
//! rewriting security-sensitive rows. New migrations are append-only; never edit or reorder a
//! migration once released.

use dam_api::LibError;
use rusqlite::{Connection, DatabaseName, OpenFlags, TransactionBehavior};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// Ordered schema changes. Append V4, V5, … here; never fold a later change into an old step.
const MIGRATIONS: &[&str] = &[
    // V1: the original flags, bearer tokens, and audit log store.
    r#"
    CREATE TABLE IF NOT EXISTS feature_flag (
      key        TEXT PRIMARY KEY,
      value      TEXT NOT NULL,
      version    INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      updated_by TEXT
    );
    CREATE TABLE IF NOT EXISTS token (
      token_id    TEXT PRIMARY KEY,
      label       TEXT NOT NULL,
      secret_hash TEXT NOT NULL UNIQUE,
      scopes      TEXT NOT NULL,
      created     INTEGER NOT NULL,
      expires     INTEGER,
      last_used   INTEGER
    );
    CREATE INDEX IF NOT EXISTS token_secret ON token(secret_hash);
    CREATE TABLE IF NOT EXISTS audit_log (
      id     INTEGER PRIMARY KEY AUTOINCREMENT,
      at     INTEGER NOT NULL,
      actor  TEXT NOT NULL,
      action TEXT NOT NULL,
      target TEXT,
      detail TEXT
    );
    "#,
    // V2: local accounts, sessions, groups, and resource shares (issue #42).
    r#"
    CREATE TABLE IF NOT EXISTS account (
      account_id    TEXT PRIMARY KEY,
      username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
      display_name  TEXT,
      password_hash TEXT,
      role          TEXT NOT NULL,
      disabled      INTEGER NOT NULL DEFAULT 0,
      created       INTEGER NOT NULL,
      last_login    INTEGER
    );
    CREATE TABLE IF NOT EXISTS session (
      session_id    TEXT PRIMARY KEY,
      account_id    TEXT NOT NULL REFERENCES account(account_id) ON DELETE CASCADE,
      secret_hash   TEXT NOT NULL,
      csrf          TEXT NOT NULL,
      created       INTEGER NOT NULL,
      last_seen     INTEGER NOT NULL,
      absolute_exp  INTEGER NOT NULL,
      user_agent    TEXT
    );
    CREATE INDEX IF NOT EXISTS session_account ON session(account_id);
    CREATE TABLE IF NOT EXISTS login_failure (
      username TEXT NOT NULL COLLATE NOCASE,
      at       INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS login_failure_user ON login_failure(username, at);
    CREATE TABLE IF NOT EXISTS group_ (
      group_id TEXT PRIMARY KEY,
      name     TEXT NOT NULL UNIQUE COLLATE NOCASE,
      created  INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS group_member (
      group_id   TEXT NOT NULL REFERENCES group_(group_id)    ON DELETE CASCADE,
      account_id TEXT NOT NULL REFERENCES account(account_id) ON DELETE CASCADE,
      PRIMARY KEY (group_id, account_id)
    );
    CREATE TABLE IF NOT EXISTS share (
      share_id    TEXT PRIMARY KEY,
      resource    TEXT NOT NULL,
      resource_id TEXT NOT NULL,
      account_id  TEXT REFERENCES account(account_id) ON DELETE CASCADE,
      group_id    TEXT REFERENCES group_(group_id)    ON DELETE CASCADE,
      access      TEXT NOT NULL,
      granted_by  TEXT NOT NULL,
      created     INTEGER NOT NULL,
      CHECK ((account_id IS NULL) != (group_id IS NULL))
    );
    CREATE INDEX IF NOT EXISTS share_resource ON share(resource, resource_id);
    "#,
    // V3: OIDC provider configuration, identity links, and in-flight logins (issue #41).
    r#"
    CREATE TABLE IF NOT EXISTS oidc_provider (
      id            INTEGER PRIMARY KEY CHECK (id = 1),
      issuer        TEXT NOT NULL,
      client_id     TEXT NOT NULL,
      client_secret TEXT,
      redirect_url  TEXT NOT NULL,
      scopes        TEXT NOT NULL,
      provisioning  TEXT NOT NULL,
      updated_at    INTEGER NOT NULL,
      updated_by    TEXT
    );
    CREATE TABLE IF NOT EXISTS oidc_identity (
      issuer     TEXT NOT NULL,
      subject    TEXT NOT NULL,
      account_id TEXT NOT NULL REFERENCES account(account_id) ON DELETE CASCADE,
      linked_at  INTEGER NOT NULL,
      PRIMARY KEY (issuer, subject)
    );
    CREATE INDEX IF NOT EXISTS oidc_identity_account ON oidc_identity(account_id);
    CREATE TABLE IF NOT EXISTS oidc_login (
      state         TEXT PRIMARY KEY,
      nonce         TEXT NOT NULL,
      pkce_verifier TEXT NOT NULL,
      return_to     TEXT,
      browser_hash  TEXT NOT NULL,
      created       INTEGER NOT NULL
    );
    "#,
];

/// Apply every pending migration as one atomic upgrade.
///
/// `BEGIN IMMEDIATE` serializes concurrent openers before either inspects or changes the version.
/// A non-empty on-disk database gets a SQLite-online-backup snapshot while that lock is held. If
/// any statement fails, dropping the transaction restores both schema and `user_version`; a restart
/// safely retries the same ordered sequence.
pub(super) fn migrate(conn: &mut Connection, path: Option<&Path>) -> Result<(), LibError> {
    migrate_with(conn, path, MIGRATIONS)
}

fn migrate_with(
    conn: &mut Connection,
    path: Option<&Path>,
    migrations: &[&str],
) -> Result<(), LibError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| migration_error("could not lock server.db for migration", error))?;
    let current: i64 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| migration_error("could not read server.db schema version", error))?;
    let target = migrations.len() as i64;

    if current > target {
        return Err(LibError::Internal(format!(
            "server.db is schema v{current}, but this build only understands through v{target}; upgrade 3dam before opening this data directory"
        )));
    }
    if current == target {
        return Ok(());
    }

    let has_schema: bool = tx
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_schema
                 WHERE type IN ('table', 'index', 'trigger', 'view')
                   AND name NOT LIKE 'sqlite_%'
             )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| migration_error("could not inspect server.db before migration", error))?;
    let backup = if has_schema {
        path.map(|path| create_backup(&tx, path, current, target))
            .transpose()?
    } else {
        None
    };

    for (index, sql) in migrations.iter().enumerate().skip(current as usize) {
        let version = (index + 1) as i64;
        if let Err(error) = tx.execute_batch(sql) {
            let recovery = backup
                .as_ref()
                .map(|path| format!("; pre-migration backup: {}", path.display()))
                .unwrap_or_default();
            return Err(LibError::Internal(format!(
                "server.db migration to v{version} failed; the upgrade transaction was rolled back{recovery}: {error}"
            )));
        }
        tx.pragma_update(None, "user_version", version)
            .map_err(|error| {
                migration_error(
                    &format!("could not record server.db schema v{version}; migration rolled back"),
                    error,
                )
            })?;
    }
    tx.commit()
        .map_err(|error| migration_error("could not commit server.db migration; upgrade rolled back", error))?;
    tracing::info!(from = current, to = target, backup = ?backup, "migrated server.db");
    Ok(())
}

fn migration_error(context: &str, error: rusqlite::Error) -> LibError {
    LibError::Internal(format!("{context}: {error}"))
}

/// Reserve a unique, operator-visible path and fill it through SQLite's backup API. A raw file copy
/// is unsafe for a WAL database; the backup API produces one self-contained, consistent database.
fn create_backup(
    _migration_conn: &Connection,
    database_path: &Path,
    current: i64,
    target: i64,
) -> Result<PathBuf, LibError> {
    let parent = database_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = database_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("server.db"));
    let mut stem = OsString::from(file_name);
    stem.push(format!(".pre-migration-v{current}-to-v{target}"));

    let backup_path = reserve_backup_path(parent, &stem)?;

    // `sqlite3_backup_step` cannot use the connection that owns our IMMEDIATE write transaction
    // as its source (SQLite reports that source as locked). A second read-only connection is still
    // allowed through the reserved lock and sees the exact pre-migration snapshot; meanwhile no
    // competing writer can change it before this transaction commits or rolls back.
    let result = (|| {
        let source = Connection::open_with_flags(
            database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| {
            LibError::Internal(format!(
                "could not open server.db for migration backup {}: {error}",
                database_path.display()
            ))
        })?;
        source
            .backup(DatabaseName::Main, &backup_path, None)
            .map_err(|error| {
                LibError::Internal(format!(
                    "could not create server.db migration backup {}: {error}",
                    backup_path.display()
                ))
            })?;
        secure_and_sync_backup(&backup_path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&backup_path);
        return Err(error);
    }
    Ok(backup_path)
}

fn reserve_backup_path(parent: &Path, stem: &std::ffi::OsStr) -> Result<PathBuf, LibError> {
    for attempt in 0_u32.. {
        let mut name = OsString::from(stem);
        if attempt > 0 {
            name.push(format!(".{attempt}"));
        }
        name.push(".bak");
        let candidate = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                let metadata = match file.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        drop(file);
                        let _ = std::fs::remove_file(&candidate);
                        return Err(LibError::Internal(format!(
                            "could not inspect reserved server.db backup {}: {error}",
                            candidate.display()
                        )));
                    }
                };
                if !metadata.file_type().is_file() {
                    drop(file);
                    let _ = std::fs::remove_file(&candidate);
                    return Err(LibError::Internal(format!(
                        "refusing non-regular server.db backup path {}",
                        candidate.display()
                    )));
                }
                drop(file);
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(LibError::Internal(format!(
                    "could not reserve server.db migration backup {}: {error}",
                    candidate.display()
                )));
            }
        }
    }
    unreachable!("the unbounded backup suffix loop cannot end")
}

fn secure_and_sync_backup(path: &Path) -> Result<(), LibError> {
    let path_metadata = std::fs::symlink_metadata(path).map_err(|error| {
        LibError::Internal(format!(
            "could not inspect completed server.db backup path {}: {error}",
            path.display()
        ))
    })?;
    if !path_metadata.file_type().is_file() || path_metadata.file_type().is_symlink() {
        return Err(LibError::Internal(format!(
            "completed server.db backup path is not a regular file: {}",
            path.display()
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
            LibError::Internal(format!(
                "could not restrict server.db backup {} to mode 0600: {error}",
                path.display()
            ))
        })?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            LibError::Internal(format!(
                "could not reopen server.db backup {} for durability check: {error}",
                path.display()
            ))
        })?;
    let metadata = file.metadata().map_err(|error| {
        LibError::Internal(format!(
            "could not inspect completed server.db backup {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(LibError::Internal(format!(
            "completed server.db backup is not a regular file: {}",
            path.display()
        )));
    }
    file.sync_all().map_err(|error| {
        LibError::Internal(format!(
            "could not sync server.db backup {}: {error}",
            path.display()
        ))
    })?;

    // Persist the directory entry as well as the file contents on Unix. Some other platforms do
    // not permit opening/syncing directories; their inherited ACL/durability rules apply instead.
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        let sync_result = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
        match sync_result {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
                ) =>
            {
                tracing::warn!(path = %parent.display(), %error, "filesystem cannot sync backup directory");
            }
            Err(error) => {
                return Err(LibError::Internal(format!(
                    "could not sync server.db backup directory {}: {error}",
                    parent.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::migrate_with;
    use rusqlite::Connection;

    #[test]
    fn failed_upgrade_rolls_back_and_retries_from_the_same_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        let broken = [
            "CREATE TABLE first (secret TEXT NOT NULL); INSERT INTO first VALUES ('kept');",
            "CREATE TABLE partial (value TEXT); THIS IS NOT SQL;",
        ];

        let error = migrate_with(&mut conn, None, &broken).unwrap_err();
        assert!(error.to_string().contains("rolled back"));
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 0, "the complete upgrade is one transaction");
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('first', 'partial')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "failed schema statements must not survive");

        let repaired = [
            "CREATE TABLE first (secret TEXT NOT NULL); INSERT INTO first VALUES ('kept');",
            "CREATE TABLE second (value TEXT);",
        ];
        migrate_with(&mut conn, None, &repaired).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(
            conn.query_row("SELECT secret FROM first", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "kept"
        );
    }

    #[test]
    fn failed_disk_upgrade_leaves_an_openable_pre_migration_backup() {
        let dir = tempfile::tempdir().unwrap();
        let database_path = dir.path().join("server.db");
        let mut conn = Connection::open(&database_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE legacy_secret (value TEXT NOT NULL);
             INSERT INTO legacy_secret VALUES ('credential-material');",
        )
        .unwrap();

        let error = migrate_with(
            &mut conn,
            Some(&database_path),
            &["CREATE TABLE partial (value TEXT); NOT VALID SQL;"],
        )
        .unwrap_err();
        assert!(error.to_string().contains("pre-migration backup"));
        assert_eq!(
            conn.query_row("SELECT value FROM legacy_secret", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "credential-material"
        );
        let partial_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='partial')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!partial_exists);

        let backup_path = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "bak"))
            .expect("failed migration should retain its recovery backup");
        let backup = Connection::open(backup_path).unwrap();
        assert_eq!(
            backup
                .query_row("SELECT value FROM legacy_secret", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "credential-material"
        );
        let backup_version: i64 = backup
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(backup_version, 0);
    }
}
