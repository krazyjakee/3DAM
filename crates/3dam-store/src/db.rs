//! Connection ownership for `library.db` (issue #137, tech-spec 02).
//!
//! The catalog used to be one `Mutex<Connection>`, so every read serialised behind every write and
//! WAL's whole point — readers that never block the writer — was unreachable. This module owns the
//! shape that fixes it: **one writer connection** plus a **small bounded pool of read-only
//! connections**, with a `maint` `RwLock` that lets vacuum/checkpoint/migration drain everyone and
//! run alone.
//!
//! Three accessors, three contracts:
//!
//! | accessor | holds | for |
//! |---|---|---|
//! | [`Db::read`] | `maint.read()` + one pooled read-only connection inside `BEGIN DEFERRED` | queries |
//! | [`Db::write`] | `maint.read()` + the writer mutex | anything that mutates rows |
//! | [`Db::exclusive`] | `maint.write()` + the writer mutex | `VACUUM`, `wal_checkpoint(TRUNCATE)`, migrations |
//!
//! Lock order is always `maint` → connection, so `write` and `exclusive` cannot deadlock against
//! each other, and a reader parked waiting for a free pooled connection still holds only
//! `maint.read()`.
//!
//! ## Writer transactions are always `Immediate`
//!
//! Two rules follow from readers no longer being excluded by the writer lock.
//!
//! **Every multi-statement write is a transaction.** The writer mutex only serialises *writers*; a
//! pooled reader can land between two of its statements and observe a half-applied change (an
//! asset whose `media_type` no longer matches its `*_attr` row, deleted rows whose blocklist entry
//! is not there yet). One transaction makes the whole edit a single visible step.
//!
//! **Every one of those transactions takes `TransactionBehavior::Immediate`**, never rusqlite's
//! default `BEGIN DEFERRED`. A deferred transaction that reads before it writes pins a WAL snapshot
//! at its first read and only asks for the write lock later; if anything else committed in between,
//! that upgrade fails with `SQLITE_BUSY_SNAPSHOT` — and SQLite does **not** invoke the busy handler
//! for that case, so `busy_timeout` cannot rescue it and the caller sees a bare "database is
//! locked". `BEGIN IMMEDIATE` takes the write lock up front, where the busy handler *does* apply,
//! so contention costs a wait instead of an error. In-process the single writer mutex already
//! serialises us, but a second process on the same data dir (the desktop shell plus a CLI run) has
//! no such thing. The cost is nil either way, so the convention is unconditional: **if it writes,
//! it begins immediate.** Deferred `BEGIN` is reserved for [`Db::read`], which never writes.
//!
//! `SQLITE_BUSY_SNAPSHOT` is not the only place the busy handler goes missing: `PRAGMA
//! journal_mode` is the other, and [`enter_wal`] hand-rolls the wait it should have had.
//!
//! ## In-memory stores are the sharp edge
//!
//! [`Db::Memory`] cannot be pooled: a second `:memory:` handle is a *different database*, and the
//! shared-cache URI that would join them forbids WAL and swaps `SQLITE_BUSY` (which `busy_timeout`
//! handles) for table-level `SQLITE_LOCKED` (which it does not). So a memory store aliases `read`
//! and `write` onto the same mutex — correct, but it means holding a read guard and a write guard
//! at once *hangs*. A debug-only per-thread guard-depth counter turns that hang into an immediate
//! panic; see [`GuardDepth`].

use crate::BUSY_TIMEOUT;
use dam_api::{internal, LibError};
use rusqlite::{Connection, ErrorCode, OpenFlags};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Pool ceiling when `3DAM_DB_READERS` is unset. Two is the floor that keeps a browse query off the
/// writer at all; beyond four, extra readers mostly queue on the same page cache and disk.
const READER_LIMITS: (usize, usize) = (2, 4);

/// What a connection is allowed to do, which decides its PRAGMA set.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The single read-write connection (and the in-memory store's only connection).
    Writer,
    /// A pooled, read-only connection.
    Reader,
}

/// Apply the per-connection PRAGMAs. Every connection gets the busy timeout (one data dir can
/// legitimately be open in two processes — the desktop shell and a CLI run — and rusqlite's default
/// is to fail instantly with `SQLITE_BUSY` rather than wait; the contended windows are short and a
/// spurious "database is locked" would surface as a failed job) and foreign keys.
///
/// `journal_mode` is deliberately writer-only: it is a persisted *database header* property, so
/// issuing it on a read-only handle would attempt a write and fail. `query_only` is the reader's
/// belt-and-braces against a stray write slipping through a handle the OS would otherwise let
/// write (see the CANTOPEN fallback in [`ReadPool::open_reader`]).
pub(crate) fn configure(conn: &Connection, role: Role) -> Result<(), LibError> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(internal)?;
    if role == Role::Writer {
        enter_wal(conn)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(internal)?;
    }
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(internal)?;
    if role == Role::Reader {
        conn.pragma_update(None, "query_only", "ON")
            .map_err(internal)?;
    }
    Ok(())
}

/// Switch the database file into WAL, retrying while another connection holds it.
///
/// `journal_mode` is the one PRAGMA `busy_timeout` cannot cover. Changing it rewrites the database
/// *header*, so it needs a brief exclusive lock — and SQLite answers `SQLITE_BUSY` for that case
/// **without invoking the busy handler**, exactly as it does for `SQLITE_BUSY_SNAPSHOT`. Two
/// processes opening the same data dir at the same instant (the desktop shell booting its
/// in-process server while a CLI run starts — the scenario this module's `BEGIN IMMEDIATE`
/// convention exists for) therefore raced here, and the loser failed its whole `Store::open` with a
/// bare "database is locked". So the wait is hand-rolled, bounded by the same [`BUSY_TIMEOUT`] the
/// handler would have used.
///
/// The window is only ever open on the *first* open of a fresh (or legacy rollback-journal) file:
/// once the file is in WAL, this pragma is a no-op that takes no lock at all.
fn enter_wal(conn: &Connection) -> Result<(), LibError> {
    /// Short enough that the common uncontended path costs one sleep at worst, long enough not to
    /// spin on a migration that holds the write lock for a while.
    const RETRY_EVERY: std::time::Duration = std::time::Duration::from_millis(20);
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(err) if busy(&err) && std::time::Instant::now() < deadline => {
                std::thread::sleep(RETRY_EVERY);
            }
            Err(err) => return Err(internal(err)),
        }
    }
}

fn busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == ErrorCode::DatabaseBusy || e.code == ErrorCode::DatabaseLocked
    )
}

/// How many pooled readers to allow. `3DAM_DB_READERS` overrides (mirroring the `3DAM_BG_THREADS`
/// convention in `dam-core`'s resource governor); otherwise CPU count, clamped.
fn reader_limit() -> usize {
    limit_from(std::env::var("3DAM_DB_READERS").ok())
}

/// The override is honoured verbatim (floored at one — a pool of zero can never serve anyone);
/// an unset or unparseable value falls back to the clamped CPU count.
fn limit_from(configured: Option<String>) -> usize {
    if let Some(n) = configured.and_then(|v| v.trim().parse::<usize>().ok()) {
        return n.max(1);
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(READER_LIMITS.0)
        .clamp(READER_LIMITS.0, READER_LIMITS.1)
}

/// The catalog's connections.
pub(crate) enum Db {
    /// A real `library.db`: one writer, a lazy read pool, and the maintenance gate.
    File {
        path: PathBuf,
        writer: Mutex<Connection>,
        readers: ReadPool,
        /// Held shared by every read/write guard and exclusively by [`Db::exclusive`], which is how
        /// a vacuum drains in-flight readers and blocks new checkouts without a global mutex.
        maint: RwLock<()>,
    },
    /// An in-memory store (tests, and `Store::from_conn`). One connection *is* the database.
    Memory { conn: Mutex<Connection> },
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Db::File { path, readers, .. } => f
                .debug_struct("Db::File")
                .field("path", path)
                .field("max_readers", &readers.max)
                .finish(),
            Db::Memory { .. } => f.write_str("Db::Memory"),
        }
    }
}

impl Db {
    #[cfg(feature = "ann")]
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Db::File { path, .. } => Some(path),
            Db::Memory { .. } => None,
        }
    }

    /// Open (creating if needed) the catalog at `path`: writer now, readers lazily.
    pub(crate) fn open_file(path: &Path) -> Result<Db, LibError> {
        Db::open_file_with_readers(path, reader_limit())
    }

    fn open_file_with_readers(path: &Path, max: usize) -> Result<Db, LibError> {
        let writer = Connection::open(path).map_err(internal)?;
        configure(&writer, Role::Writer)?;
        tracing::debug!(path = %path.display(), max_readers = max, "opened catalog");
        Ok(Db::File {
            path: path.to_path_buf(),
            writer: Mutex::new(writer),
            readers: ReadPool::new(path.to_path_buf(), max),
            maint: RwLock::new(()),
        })
    }

    /// Adopt an already-open connection as a single-connection (in-memory) catalog.
    pub(crate) fn in_memory(conn: Connection) -> Result<Db, LibError> {
        configure(&conn, Role::Writer)?;
        Ok(Db::Memory {
            conn: Mutex::new(conn),
        })
    }

    /// Check out a read-only connection wrapped in a deferred read transaction, so the whole guard
    /// observes one WAL snapshot. On a memory store this is the shared connection, untransacted —
    /// there is nothing to isolate it from.
    pub(crate) fn read(&self) -> Result<ReadGuard<'_>, LibError> {
        let depth = GuardDepth::enter();
        match self {
            Db::Memory { conn } => Ok(ReadGuard {
                inner: ReadInner::Shared(conn.lock().unwrap()),
                _depth: depth,
            }),
            Db::File { readers, maint, .. } => {
                let maint = maint.read().unwrap();
                let conn = readers.checkout()?;
                // A deferred BEGIN takes no lock until the first statement, so this never blocks
                // the writer; it just pins the snapshot for the guard's lifetime.
                if let Err(err) = conn.execute_batch("BEGIN DEFERRED") {
                    drop(conn);
                    readers.discard();
                    return Err(internal(err));
                }
                Ok(ReadGuard {
                    inner: ReadInner::Pooled {
                        pool: readers,
                        conn: Some(conn),
                        _maint: maint,
                    },
                    _depth: depth,
                })
            }
        }
    }

    /// Lock the writer. Readers keep running; only another writer waits.
    pub(crate) fn write(&self) -> WriteGuard<'_> {
        let depth = GuardDepth::enter();
        match self {
            Db::Memory { conn } => WriteGuard {
                conn: conn.lock().unwrap(),
                _maint: None,
                _depth: depth,
            },
            Db::File { writer, maint, .. } => {
                let maint = maint.read().unwrap();
                WriteGuard {
                    conn: writer.lock().unwrap(),
                    _maint: Some(maint),
                    _depth: depth,
                }
            }
        }
    }

    /// Take the whole catalog: drain active readers, block new checkouts, then lock the writer.
    /// Required by anything that rewrites the database file itself — `VACUUM`,
    /// `wal_checkpoint(TRUNCATE)`, schema migration.
    pub(crate) fn exclusive(&self) -> ExclusiveGuard<'_> {
        let depth = GuardDepth::enter();
        match self {
            Db::Memory { conn } => ExclusiveGuard {
                conn: conn.lock().unwrap(),
                _maint: None,
                _depth: depth,
            },
            Db::File { writer, maint, .. } => {
                let maint = maint.write().unwrap();
                ExclusiveGuard {
                    conn: writer.lock().unwrap(),
                    _maint: Some(maint),
                    _depth: depth,
                }
            }
        }
    }
}

// ── the read pool ────────────────────────────────────────────────────────────

/// A bounded pool of read-only connections, hand-rolled rather than r2d2 (rusqlite is pinned to
/// 0.39 for MSRV reasons, and r2d2_sqlite tracks newer releases).
///
/// Connections are created **lazily**: a freshly opened store has none until something actually
/// reads. The credential migration does read first (`source_credentials_migrated`), so its
/// `wal_checkpoint(TRUNCATE)` runs with pooled connections already open; it still sees `busy == 0`
/// because [`Db::exclusive`] drains every *active* reader through `maint`, and an idle pooled
/// connection sits outside a read transaction and therefore holds no WAL read mark.
pub(crate) struct ReadPool {
    path: PathBuf,
    state: Mutex<PoolState>,
    max: usize,
    /// Signalled whenever a connection returns to `idle` or an open slot is freed.
    available: Condvar,
}

struct PoolState {
    idle: Vec<Connection>,
    /// Connections in existence (idle **or** checked out), so the ceiling counts both.
    open: usize,
}

impl ReadPool {
    fn new(path: PathBuf, max: usize) -> ReadPool {
        ReadPool {
            path,
            state: Mutex::new(PoolState {
                idle: Vec::new(),
                open: 0,
            }),
            max,
            available: Condvar::new(),
        }
    }

    /// Idle connection if there is one, else a new one while under the ceiling, else wait.
    fn checkout(&self) -> Result<Connection, LibError> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(conn) = state.idle.pop() {
                return Ok(conn);
            }
            if state.open < self.max {
                // Reserve the slot, then open outside the lock — a file open should not stall the
                // other pool users, and a failure gives the slot straight back.
                state.open += 1;
                drop(state);
                return match Self::open_reader(&self.path) {
                    Ok(conn) => Ok(conn),
                    Err(err) => {
                        self.discard();
                        Err(err)
                    }
                };
            }
            state = self.available.wait(state).unwrap();
        }
    }

    /// Return a healthy connection.
    fn release(&self, conn: Connection) {
        self.state.lock().unwrap().idle.push(conn);
        self.available.notify_one();
    }

    /// Give up a connection's slot without returning it — it has already been dropped (or never
    /// opened), and the pool should recreate it cleanly on the next checkout.
    fn discard(&self) {
        let mut state = self.state.lock().unwrap();
        state.open = state.open.saturating_sub(1);
        drop(state);
        self.available.notify_one();
    }

    fn open_reader(path: &Path) -> Result<Connection, LibError> {
        const SHARED: OpenFlags = OpenFlags::SQLITE_OPEN_URI.union(OpenFlags::SQLITE_OPEN_NO_MUTEX);
        let conn = match Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY.union(SHARED),
        ) {
            Ok(conn) => conn,
            // A read-only handle needs the *directory* to be writable anyway (WAL and its shm file
            // live beside the DB) and some filesystems/permission setups refuse it outright. Rather
            // than lose pooling entirely, fall back to a read-write handle; `query_only` in
            // `configure` still keeps it read-only in practice.
            Err(err) if cantopen(&err) => {
                READONLY_FALLBACK.call_once(|| {
                    tracing::warn!(
                        path = %path.display(),
                        "read-only catalog handles unavailable; pooling read-write handles with PRAGMA query_only"
                    );
                });
                Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE.union(SHARED))
                    .map_err(internal)?
            }
            Err(err) => return Err(internal(err)),
        };
        configure(&conn, Role::Reader)?;
        Ok(conn)
    }
}

static READONLY_FALLBACK: std::sync::Once = std::sync::Once::new();

fn cantopen(err: &rusqlite::Error) -> bool {
    matches!(err, rusqlite::Error::SqliteFailure(e, _) if e.code == ErrorCode::CannotOpen)
}

// ── guards ───────────────────────────────────────────────────────────────────

/// A checked-out read connection inside a deferred transaction. Rolled back and returned to the
/// pool on drop.
pub(crate) struct ReadGuard<'a> {
    inner: ReadInner<'a>,
    _depth: GuardDepth,
}

enum ReadInner<'a> {
    Pooled {
        pool: &'a ReadPool,
        /// `Some` until `Drop` takes it back.
        conn: Option<Connection>,
        _maint: RwLockReadGuard<'a, ()>,
    },
    /// The in-memory store's single connection.
    Shared(MutexGuard<'a, Connection>),
}

impl Deref for ReadGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match &self.inner {
            ReadInner::Pooled { conn, .. } => conn.as_ref().expect("read connection checked out"),
            ReadInner::Shared(conn) => conn,
        }
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        let ReadInner::Pooled { pool, conn, .. } = &mut self.inner else {
            return;
        };
        let Some(conn) = conn.take() else { return };
        match conn.execute_batch("ROLLBACK") {
            Ok(()) => pool.release(conn),
            // The connection's transaction state is now unknown; recycling it would poison the next
            // borrower. Drop it and free the slot so the pool opens a clean replacement.
            Err(err) => {
                tracing::warn!(error = %err, "dropping read connection after a failed rollback");
                drop(conn);
                pool.discard();
            }
        }
    }
}

/// The writer connection. Concurrent reads continue; maintenance waits.
pub(crate) struct WriteGuard<'a> {
    conn: MutexGuard<'a, Connection>,
    /// `None` for a memory store, which has no maintenance gate.
    _maint: Option<RwLockReadGuard<'a, ()>>,
    _depth: GuardDepth,
}

/// The writer connection with every reader drained — for whole-file operations.
pub(crate) struct ExclusiveGuard<'a> {
    conn: MutexGuard<'a, Connection>,
    _maint: Option<RwLockWriteGuard<'a, ()>>,
    _depth: GuardDepth,
}

macro_rules! deref_conn {
    ($guard:ident) => {
        impl Deref for $guard<'_> {
            type Target = Connection;
            fn deref(&self) -> &Connection {
                &self.conn
            }
        }
        impl DerefMut for $guard<'_> {
            fn deref_mut(&mut self) -> &mut Connection {
                &mut self.conn
            }
        }
    };
}
deref_conn!(WriteGuard);
deref_conn!(ExclusiveGuard);

// ── debug-only nesting detector ──────────────────────────────────────────────

#[cfg(debug_assertions)]
thread_local! {
    static GUARD_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Panics (debug builds only) when a thread tries to hold two store connection guards at once.
///
/// On a file store that would merely be sloppy; on a memory store — where `read` and `write` are
/// the *same* mutex — it is an unrecoverable hang, and a hang in a test suite is far harder to
/// diagnose than a panic with a name on it. Zero cost in release.
pub(crate) struct GuardDepth;

impl GuardDepth {
    fn enter() -> GuardDepth {
        #[cfg(debug_assertions)]
        GUARD_DEPTH.with(|depth| {
            assert!(
                depth.get() == 0,
                "nested store connection guard: this thread already holds one; \
                 on an in-memory store the second acquisition deadlocks"
            );
            depth.set(depth.get() + 1);
        });
        GuardDepth
    }
}

impl Drop for GuardDepth {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        GUARD_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// How long a "this should not block" assertion waits before calling it a hang.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);
    /// How long a "this should block" assertion waits before believing it.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

    /// A file-backed catalog with one trivial table, in a directory that cleans itself up.
    fn file_db() -> (tempfile::TempDir, Db) {
        file_db_with_readers(reader_limit())
    }

    fn file_db_with_readers(max: usize) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_file_with_readers(&dir.path().join("library.db"), max).unwrap();
        {
            let conn = db.write();
            conn.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
                .unwrap();
        }
        (dir, db)
    }

    /// A pooled reader sees committed writer rows and hands its connection back on drop, so the
    /// pool never grows past one connection for a serial workload.
    #[test]
    fn a_pooled_reader_sees_committed_writes_and_is_recycled() {
        let (_dir, db) = file_db();
        {
            let conn = db.write();
            conn.execute("INSERT INTO t(v) VALUES ('one')", params![])
                .unwrap();
        }
        for _ in 0..3 {
            let conn = db.read().unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1, "reader missed the committed row");
        }
        let Db::File { readers, .. } = &db else {
            unreachable!()
        };
        let state = readers.state.lock().unwrap();
        assert_eq!(state.open, 1, "serial reads should reuse one connection");
        assert_eq!(state.idle.len(), 1, "the connection was not returned");
    }

    /// Pooled connections are read-only in practice, even where the OS grants a writable handle.
    #[test]
    fn a_pooled_reader_cannot_write() {
        let (_dir, db) = file_db();
        let conn = db.read().unwrap();
        assert!(
            conn.execute("INSERT INTO t(v) VALUES ('nope')", params![])
                .is_err(),
            "a pooled reader accepted a write"
        );
    }

    /// Readers overlap a held writer instead of serialising behind it — the whole point of #137.
    /// Asserted with a timeout rather than a bare `join`, so a regression fails instead of hanging.
    #[test]
    fn a_reader_runs_while_the_writer_is_held() {
        let (_dir, db) = file_db();
        let db = std::sync::Arc::new(db);
        let held = db.write();
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || {
                let conn = db.read().unwrap();
                let n = conn
                    .query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                    .unwrap();
                let _ = tx.send(n);
            });
        }
        assert_eq!(
            rx.recv_timeout(PATIENCE),
            Ok(0),
            "a read blocked on the writer lock"
        );
        drop(held);
    }

    /// The pool respects its ceiling: with `max = 1`, a second concurrent reader waits for the
    /// first to return its connection rather than opening another.
    #[test]
    fn the_pool_bounds_concurrent_readers() {
        let (_dir, db) = file_db_with_readers(1);
        let db = std::sync::Arc::new(db);
        let first = db.read().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let second = {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || {
                let _conn = db.read().unwrap();
                let _ = tx.send(());
            })
        };
        assert_eq!(
            rx.recv_timeout(SETTLE),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "the pool ceiling was not enforced"
        );
        drop(first);
        second.join().unwrap();
        let Db::File { readers, .. } = &*db else {
            unreachable!()
        };
        assert_eq!(readers.state.lock().unwrap().open, 1, "pool grew past max");
    }

    /// Exclusive access waits for in-flight readers, then runs alone.
    #[test]
    fn exclusive_waits_for_active_readers() {
        let (_dir, db) = file_db();
        let db = std::sync::Arc::new(db);
        let reader = db.read().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let vacuum = {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || {
                let conn = db.exclusive();
                conn.execute_batch("VACUUM").unwrap();
                let _ = tx.send(());
            })
        };
        assert_eq!(
            rx.recv_timeout(SETTLE),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "maintenance ran with a reader in flight"
        );
        drop(reader);
        vacuum.join().unwrap();
    }

    /// The memory store aliases both accessors onto one connection; nesting them would hang, so
    /// debug builds panic instead.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "nested store connection guard")]
    fn nested_guards_panic_instead_of_hanging() {
        let db = Db::in_memory(Connection::open_in_memory().unwrap()).unwrap();
        let _first = db.write();
        let _second = db.write();
    }

    /// Two writers opening the *same fresh file* both come up in WAL. The second one's `PRAGMA
    /// journal_mode = WAL` lands while the first holds the file's write lock, and SQLite refuses
    /// that with `SQLITE_BUSY` without ever calling the busy handler — so [`enter_wal`] has to wait
    /// by hand or `Store::open` fails outright (see `tests/concurrency.rs`, which reproduces the
    /// two-process form).
    #[test]
    fn a_second_writer_waits_out_the_journal_mode_switch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");

        // Connection A converts the file and parks inside a write transaction, holding the lock.
        let first = Connection::open(&path).unwrap();
        configure(&first, Role::Writer).unwrap();
        first
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY)")
            .unwrap();
        first.execute_batch("BEGIN IMMEDIATE").unwrap();

        // B's `journal_mode` is a no-op on an already-WAL file, so it must not even need the wait.
        let second = Connection::open(&path).unwrap();
        configure(&second, Role::Writer).expect("configuring a second writer must not fail");
        first.execute_batch("ROLLBACK").unwrap();

        let mode: String = second
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    /// `3DAM_DB_READERS` overrides the CPU-derived ceiling verbatim (never to zero); without it the
    /// pool stays inside its clamp whatever the host's CPU count is.
    #[test]
    fn the_reader_limit_honours_its_override() {
        assert_eq!(limit_from(Some("7".into())), 7);
        assert_eq!(limit_from(Some(" 1 ".into())), 1);
        assert_eq!(
            limit_from(Some("0".into())),
            1,
            "a pool of zero serves nobody"
        );
        assert!((READER_LIMITS.0..=READER_LIMITS.1).contains(&limit_from(Some("nonsense".into()))));
        assert!((READER_LIMITS.0..=READER_LIMITS.1).contains(&limit_from(None)));
    }
}
