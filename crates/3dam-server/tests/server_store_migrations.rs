use dam_server::ServerStore;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const PRE_ACCOUNTS: &str = include_str!("fixtures/server-db/pre_accounts.sql");
const ACCOUNTS_SHARES: &str = include_str!("fixtures/server-db/accounts_shares.sql");
const OIDC: &str = include_str!("fixtures/server-db/oidc.sql");
const CURRENT_VERSION: i64 = 3;

#[derive(Clone, Copy)]
enum Era {
    PreAccounts,
    AccountsShares,
    Oidc,
}

fn write_fixture(path: &Path, era: Era) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(PRE_ACCOUNTS).unwrap();
    if matches!(era, Era::AccountsShares | Era::Oidc) {
        conn.execute_batch(ACCOUNTS_SHARES).unwrap();
    }
    if matches!(era, Era::Oidc) {
        conn.execute_batch(OIDC).unwrap();
    }
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 0, "historical fixtures must remain unversioned");
}

fn backup_for(path: &Path) -> PathBuf {
    let prefix = format!(
        "{}.pre-migration-v0-to-v{CURRENT_VERSION}",
        path.file_name().unwrap().to_string_lossy()
    );
    let mut matches = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|candidate| {
            let name = candidate.file_name().unwrap().to_string_lossy();
            name.starts_with(&prefix) && name.ends_with(".bak")
        });
    let backup = matches.next().expect("migration should leave a backup");
    assert!(matches.next().is_none(), "one opener creates one backup");
    backup
}

fn assert_current_schema(conn: &Connection) {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);
    for table in [
        "feature_flag",
        "token",
        "audit_log",
        "account",
        "session",
        "login_failure",
        "group_",
        "group_member",
        "share",
        "oidc_provider",
        "oidc_identity",
        "oidc_login",
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "current schema is missing {table}");
    }
}

#[test]
fn every_unversioned_historical_shape_upgrades_without_losing_secrets() {
    for era in [Era::PreAccounts, Era::AccountsShares, Era::Oidc] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.db");
        write_fixture(&path, era);

        drop(ServerStore::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        assert_current_schema(&conn);
        let flag: (String, i64, String) = conn
            .query_row(
                "SELECT value, version, updated_by FROM feature_flag WHERE key='authentication'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(flag, ("\"token\"".into(), 7, "fixture-admin".into()));
        assert_eq!(
            conn.query_row(
                "SELECT secret_hash FROM token WHERE token_id='token-1'",
                [],
                |row| { row.get::<_, String>(0) }
            )
            .unwrap(),
            "token-secret-hash"
        );
        assert_eq!(
            conn.query_row("SELECT detail FROM audit_log WHERE id=1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "{\"ip\":\"192.0.2.1\"}"
        );

        if matches!(era, Era::AccountsShares | Era::Oidc) {
            let row: (String, String, String, String) = conn
                .query_row(
                    "SELECT a.password_hash, s.secret_hash, s.csrf, sh.resource_id
                     FROM account a
                     JOIN session s ON s.account_id = a.account_id
                     JOIN group_member gm ON gm.account_id = a.account_id
                     JOIN share sh ON sh.group_id = gm.group_id",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(
                row,
                (
                    "$argon2id$fixture-password-hash".into(),
                    "session-secret-hash".into(),
                    "csrf-secret".into(),
                    "source-1".into()
                )
            );
        }
        if matches!(era, Era::Oidc) {
            let row: (String, String, String, String) = conn
                .query_row(
                    "SELECT p.client_secret, l.pkce_verifier, l.browser_hash, i.account_id
                     FROM oidc_provider p, oidc_login l, oidc_identity i",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(
                row,
                (
                    "oidc-client-secret".into(),
                    "pkce-verifier-secret".into(),
                    "browser-cookie-hash".into(),
                    "account-1".into()
                )
            );
        }

        let backup = Connection::open(backup_for(&path)).unwrap();
        let backup_version: i64 = backup
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(backup_version, 0);
        if matches!(era, Era::PreAccounts) {
            let accounts_in_backup: bool = backup
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='account')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                !accounts_in_backup,
                "backup must capture the source before V2 creates account tables"
            );
        }
        if matches!(era, Era::Oidc) {
            assert_eq!(
                backup
                    .query_row("SELECT client_secret FROM oidc_provider", [], |row| {
                        row.get::<_, String>(0)
                    })
                    .unwrap(),
                "oidc-client-secret"
            );
            assert_eq!(
                backup
                    .query_row("SELECT pkce_verifier FROM oidc_login", [], |row| {
                        row.get::<_, String>(0)
                    })
                    .unwrap(),
                "pkce-verifier-secret"
            );
        }
        assert_eq!(
            backup
                .query_row("SELECT secret_hash FROM token", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "token-secret-hash"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(backup_for(&path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "credential-bearing backup must be owner-only");
        }
    }
}

#[test]
fn reopening_current_schema_is_idempotent_and_does_not_make_another_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    write_fixture(&path, Era::Oidc);
    drop(ServerStore::open(&path).unwrap());
    let backup = backup_for(&path);

    drop(ServerStore::open(&path).unwrap());
    assert_eq!(backup_for(&path), backup);
    let conn = Connection::open(path).unwrap();
    assert_current_schema(&conn);
    let counts: (i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM token),
                    (SELECT COUNT(*) FROM account),
                    (SELECT COUNT(*) FROM oidc_identity)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(counts, (1, 1, 1));
}

#[test]
fn schema_from_a_newer_binary_is_refused_without_partial_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE future_secret (value TEXT NOT NULL); INSERT INTO future_secret VALUES ('keep-me');")
        .unwrap();
    conn.pragma_update(None, "user_version", CURRENT_VERSION + 1)
        .unwrap();
    drop(conn);

    let error = match ServerStore::open(&path) {
        Ok(_) => panic!("a newer server.db schema must be refused"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("schema v4"));
    assert!(message.contains("only understands through v3"));

    let conn = Connection::open(path).unwrap();
    assert_eq!(
        conn.query_row("SELECT value FROM future_secret", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "keep-me"
    );
    let introduced: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='feature_flag')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!introduced, "refusal must happen before migrations run");
}
