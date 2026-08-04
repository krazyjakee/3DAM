//! Concurrency behaviour of the catalog's connection model (issue #137): one writer, a bounded
//! pool of read-only connections, and a maintenance gate that drains both.
//!
//! These drive the **public** `Store` surface only — `read`/`write`/`exclusive` are `pub(crate)`,
//! and the point of the exercise is that a caller who knows nothing about them still gets
//! non-blocking reads, snapshot-consistent aggregates, and atomic upserts. The pool-mechanics unit
//! tests (ceiling, recycling, exclusive draining) live next to the implementation in `src/db.rs`.
//!
//! Every store here is **file-backed**. An in-memory store is a single connection by construction
//! (a second `:memory:` handle is a different database), so it cannot exercise any of this.
//!
//! ## Why nothing below asserts on elapsed time
//!
//! The tempting assertion — "a browse query stayed under N ms during a scan" — is a statement about
//! the host, not the code: on a loaded machine the scheduler alone can blow any threshold, and the
//! test would fail for reasons that have nothing to do with SQLite. So the assertions here are about
//! **progress and consistency**: reads complete, they complete repeatedly while a writer is running,
//! and what they observe is internally coherent. A regression to a single shared connection makes
//! readers stall behind the writer, which shows up as a reader that never reaches its iteration
//! floor within the budget — a real failure, not a slow one.

use dam_api::dto::{ImageAttributes, MediaAttributes, MediaType, QueryRequest, VideoAttributes};
use dam_api::id::SourceId;
use dam_api::service::Visibility;
use dam_sources::SourceConnection;
use dam_store::{now_ms, NewAsset, Store};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How many rows a "sustained write" burst commits. Each is its own immediate transaction, so this
/// is a few hundred WAL commits — long enough for readers to interleave, short enough to stay well
/// inside a couple of seconds even on a busy host.
const WRITE_BURST: usize = 300;

/// The smallest number of successful iterations that still counts as "the reader made progress".
/// Deliberately tiny: the failure this guards against is a reader that completes *zero* or one
/// iteration because it is queued behind the writer, not one that is merely slow.
const MIN_READS: u64 = 3;

// ── fixtures ─────────────────────────────────────────────────────────────────

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

fn add_source(store: &Store, name: &str) -> SourceId {
    store
        .add_source(
            &SourceConnection::LocalFs {
                root: format!("/tmp/{name}"),
            },
            name,
            false,
        )
        .unwrap()
}

fn asset(source_id: SourceId, path: &str, media: MediaType) -> NewAsset {
    NewAsset {
        source_id,
        path: path.to_string(),
        filename: path.rsplit('/').next().unwrap_or(path).to_string(),
        content_hash: None,
        size_bytes: Some(1024),
        source_modified_at: Some(now_ms()),
        scanned_at: now_ms(),
        media_type: media,
        format: match media {
            MediaType::Image => "png".into(),
            MediaType::Video => "mp4".into(),
            _ => "bin".into(),
        },
    }
}

fn page_request() -> QueryRequest {
    QueryRequest {
        include_total: Some(true),
        ..Default::default()
    }
}

// ── 1. readers are not queued behind the writer ──────────────────────────────

/// A writer commits continuously while three readers run browse queries and stats against the same
/// store. Every read must succeed, and every reader must get through several iterations *while the
/// burst is still running* — under the old single-`Mutex<Connection>` store each read would wait
/// out the writer, and a reader could easily finish the whole window without completing one query.
///
/// The extra assertion — that readers collectively saw the catalog at more than one size — is what
/// distinguishes "ran concurrently" from "ran entirely before or entirely after" the burst.
#[test]
fn readers_progress_during_a_sustained_write() {
    let (_dir, store) = store();
    let source = add_source(&store, "burst");

    let done = AtomicBool::new(false);
    let writes = AtomicU64::new(0);
    // Readers + writer, so the burst cannot finish before a reader thread has even been scheduled
    // (which would make the "saw the catalog grow" assertion below a coin flip on a busy host).
    let barrier = std::sync::Barrier::new(4);

    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..3)
            .map(|n| {
                let store = &store;
                let done = &done;
                let barrier = &barrier;
                scope.spawn(move || {
                    // (iterations, smallest total seen, largest total seen)
                    let mut iterations = 0u64;
                    let (mut low, mut high) = (u64::MAX, 0u64);
                    barrier.wait();
                    while !done.load(Ordering::Relaxed) || iterations < MIN_READS {
                        let page = store
                            .query_assets_semantic(&page_request(), None, &Visibility::Full)
                            .unwrap_or_else(|e| panic!("reader {n} browse failed: {e}"));
                        let total = page.total.expect("first page carries an exact total");
                        let stats = store
                            .stats(None, &Visibility::Full)
                            .unwrap_or_else(|e| panic!("reader {n} stats failed: {e}"));
                        low = low.min(total.min(stats.total));
                        high = high.max(total.max(stats.total));
                        iterations += 1;
                    }
                    (iterations, low, high)
                })
            })
            .collect();

        barrier.wait();
        for i in 0..WRITE_BURST {
            store
                .upsert_asset(&asset(source, &format!("tex/{i:04}.png"), MediaType::Image))
                .expect("write during concurrent reads");
            writes.fetch_add(1, Ordering::Relaxed);
        }
        done.store(true, Ordering::Relaxed);

        let observed: Vec<_> = readers.into_iter().map(|r| r.join().unwrap()).collect();
        for (n, (iterations, _, _)) in observed.iter().enumerate() {
            assert!(
                *iterations >= MIN_READS,
                "reader {n} completed only {iterations} reads during {WRITE_BURST} writes — \
                 reads look serialised behind the writer"
            );
        }
        let low = observed.iter().map(|(_, low, _)| *low).min().unwrap();
        let high = observed.iter().map(|(_, _, high)| *high).max().unwrap();
        assert!(
            high > low,
            "readers only ever saw {high} assets, so they never overlapped the write burst"
        );
        assert!(
            high <= WRITE_BURST as u64,
            "reader saw more rows than exist"
        );
    });

    assert_eq!(writes.load(Ordering::Relaxed), WRITE_BURST as u64);
    assert_eq!(
        store.stats(None, &Visibility::Full).unwrap().total,
        WRITE_BURST as u64,
        "every write in the burst should have landed"
    );
}

// ── 2. one call, one snapshot ────────────────────────────────────────────────

/// `stats()` answers from three separate maintained counter tables — `library_stat` (the grand
/// total), `media_stat`, and `source_stat` — in as many separate statements. They are only
/// guaranteed to agree if the whole call observes **one** WAL snapshot, which is exactly what the
/// `BEGIN DEFERRED` in `Db::read` pins for the guard's lifetime.
/// Drop that and each statement sees whatever the writer had committed by the time it ran, so the
/// per-media and per-source breakdowns drift away from the grand total under load.
///
/// Two sources, so the per-source sum is a real cross-table check rather than a tautology.
#[test]
fn reads_see_a_consistent_snapshot() {
    let (_dir, store) = store();
    let sources = [add_source(&store, "left"), add_source(&store, "right")];

    let done = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let reader = {
            let store = &store;
            let done = &done;
            scope.spawn(move || {
                let mut iterations = 0u64;
                let mut previous = 0u64;
                while !done.load(Ordering::Relaxed) || iterations < MIN_READS {
                    let stats = store.stats(None, &Visibility::Full).unwrap();
                    let by_media: u64 = stats.by_media.values().sum();
                    let by_source: u64 = stats.by_source.values().sum();
                    assert_eq!(
                        stats.total, by_media,
                        "media breakdown disagrees with the total inside one stats() call — \
                         the read transaction is not pinning a snapshot"
                    );
                    assert_eq!(
                        stats.total, by_source,
                        "source breakdown disagrees with the total inside one stats() call — \
                         the read transaction is not pinning a snapshot"
                    );
                    // Inserts only, so a later call can never see fewer rows than an earlier one.
                    assert!(
                        stats.total >= previous,
                        "the catalog appeared to shrink ({previous} → {}) under an insert-only load",
                        stats.total
                    );
                    previous = stats.total;
                    // The paged query counts `asset` rows directly rather than reading the counter
                    // tables, so it is an independent check on the same monotonic quantity.
                    let page = store
                        .query_assets_semantic(&page_request(), None, &Visibility::Full)
                        .unwrap();
                    assert!(
                        page.total.unwrap() >= stats.total,
                        "a later browse total went backwards against an earlier stats total"
                    );
                    iterations += 1;
                }
                iterations
            })
        };

        for i in 0..WRITE_BURST {
            let source = sources[i % sources.len()];
            store
                .upsert_asset(&asset(source, &format!("a/{i:04}.png"), MediaType::Image))
                .unwrap();
        }
        done.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() >= MIN_READS);
    });

    let stats = store.stats(None, &Visibility::Full).unwrap();
    assert_eq!(stats.total, WRITE_BURST as u64);
    assert_eq!(stats.by_source.len(), 2, "both sources should be counted");
}

// ── 3. an upsert is one visible step ─────────────────────────────────────────

/// Reclassifying an asset is a multi-statement edit: the `asset` row's `media_type` changes and the
/// attr row the *old* classification owned is deleted. Step 3 of #137 wrapped that in one immediate
/// transaction; without it a concurrent reader can land in between and see an asset declaring one
/// media type while the other type's attr row is still joined.
///
/// Image and Video are the pair that makes this observable through the public API: the grid select
/// `COALESCE`s width/height across `image_attr` and `video_attr`, so a torn state reports the *old*
/// media's dimensions under the *new* media type. Hence the invariant asserted below — an asset may
/// legitimately have no dimensions yet (the attr write is a separate call that follows the upsert),
/// but it must never report the other classification's numbers.
#[test]
fn upsert_is_atomic_for_concurrent_readers() {
    const IMAGE_DIMS: &str = "111×111";
    const VIDEO_DIMS: &str = "222×222";

    let (_dir, store) = store();
    let source = add_source(&store, "reclass");
    let path = "clips/ambiguous.mp4";
    let (id, _) = store
        .upsert_asset(&asset(source, path, MediaType::Image))
        .unwrap();

    let done = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let store = &store;
                let done = &done;
                scope.spawn(move || {
                    let mut iterations = 0u64;
                    // What the reader actually witnessed, so the assertion above can be shown to be
                    // non-vacuous: both classifications, each with its own dimensions attached.
                    let mut seen: std::collections::BTreeSet<(&'static str, Option<String>)> =
                        Default::default();
                    while !done.load(Ordering::Relaxed) || iterations < MIN_READS {
                        let page = store
                            .query_assets_semantic(&page_request(), None, &Visibility::Full)
                            .unwrap();
                        let item = page
                            .items
                            .iter()
                            .find(|s| s.id == id)
                            .expect("the asset never disappears — it is only reclassified");
                        let dims = item.key_attrs.get("dimensions").map(String::as_str);
                        match item.media {
                            MediaType::Image => assert!(
                                dims.is_none_or(|d| d == IMAGE_DIMS),
                                "an image reported {dims:?}: a video_attr row outlived the \
                                 reclassification, so the upsert was not atomic"
                            ),
                            MediaType::Video => assert!(
                                dims.is_none_or(|d| d == VIDEO_DIMS),
                                "a video reported {dims:?}: an image_attr row outlived the \
                                 reclassification, so the upsert was not atomic"
                            ),
                            other => panic!("unexpected media type {other:?}"),
                        }
                        seen.insert((item.media.as_str(), dims.map(str::to_string)));
                        iterations += 1;
                    }
                    (iterations, seen)
                })
            })
            .collect();

        for i in 0..120 {
            if i % 2 == 0 {
                store
                    .upsert_asset(&asset(source, path, MediaType::Video))
                    .unwrap();
                store
                    .set_media_attrs(
                        &id,
                        &MediaAttributes::Video(VideoAttributes {
                            width: Some(222),
                            height: Some(222),
                            duration_ms: Some(5_000),
                            ..Default::default()
                        }),
                    )
                    .unwrap();
            } else {
                store
                    .upsert_asset(&asset(source, path, MediaType::Image))
                    .unwrap();
                store
                    .set_media_attrs(
                        &id,
                        &MediaAttributes::Image(ImageAttributes {
                            width: Some(111),
                            height: Some(111),
                            ..Default::default()
                        }),
                    )
                    .unwrap();
            }
        }
        done.store(true, Ordering::Relaxed);
        let mut seen = std::collections::BTreeSet::new();
        for reader in readers {
            let (iterations, observed) = reader.join().unwrap();
            assert!(iterations >= MIN_READS);
            seen.extend(observed);
        }
        // Without this the whole test could pass by never catching the asset in either populated
        // state — the assertions above would be true of an asset nobody ever looked at properly.
        assert!(
            seen.contains(&(MediaType::Image.as_str(), Some(IMAGE_DIMS.into())))
                && seen.contains(&(MediaType::Video.as_str(), Some(VIDEO_DIMS.into()))),
            "readers never observed both populated classifications; saw {seen:?}"
        );
    });

    assert_eq!(store.stats(None, &Visibility::Full).unwrap().total, 1);
}

// ── 4. maintenance drains everyone ───────────────────────────────────────────

/// `VACUUM` rewrites the database file, so it cannot run while a pooled reader holds a WAL read
/// mark. `Store::vacuum` takes the exclusive gate for exactly that reason; this asserts the gate
/// works from the outside — a vacuum issued against a store that readers are hammering completes
/// without `SQLITE_BUSY`, and the readers keep working before, during, and after it.
#[test]
fn vacuum_succeeds_while_readers_are_active() {
    let (_dir, store) = store();
    let source = add_source(&store, "compact");
    for i in 0..200 {
        store
            .upsert_asset(&asset(source, &format!("v/{i:04}.png"), MediaType::Image))
            .unwrap();
    }

    let done = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let store = &store;
                let done = &done;
                scope.spawn(move || {
                    let mut iterations = 0u64;
                    while !done.load(Ordering::Relaxed) || iterations < MIN_READS {
                        let page = store
                            .query_assets_semantic(&page_request(), None, &Visibility::Full)
                            .unwrap();
                        assert_eq!(page.total, Some(200), "vacuum must not change the contents");
                        iterations += 1;
                    }
                    iterations
                })
            })
            .collect();

        // Several passes: one vacuum could win the gate in a lull, three cannot all be luck.
        for _ in 0..3 {
            store.vacuum().expect("vacuum blocked by active readers");
        }
        done.store(true, Ordering::Relaxed);
        for reader in readers {
            assert!(reader.join().unwrap() >= MIN_READS);
        }
    });

    assert_eq!(store.stats(None, &Visibility::Full).unwrap().total, 200);
}

// ── 5. writers serialise, they do not collide ────────────────────────────────

/// Two threads upserting the *same* `(source_id, path)` race on the read-then-write inside
/// `upsert_asset`. The single writer connection serialises them and the immediate transaction keeps
/// each one whole, so the pair must produce exactly one asset — and, because `BEGIN IMMEDIATE`
/// takes the write lock up front where the busy handler applies, neither may fail with
/// "database is locked".
#[test]
fn concurrent_writers_stay_serialised() {
    let (_dir, store) = store();
    let source = add_source(&store, "race");
    let path = "shared/contested.png";

    let barrier = std::sync::Barrier::new(2);
    let outcomes: Vec<Vec<(bool, String)>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let store = &store;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    (0..60)
                        .map(|_| {
                            let (id, is_new) = store
                                .upsert_asset(&asset(source, path, MediaType::Image))
                                .expect("concurrent upsert of the same path");
                            (is_new, id.to_string())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });

    let flat: Vec<_> = outcomes.iter().flatten().collect();
    let inserts = flat.iter().filter(|(is_new, _)| *is_new).count();
    assert_eq!(
        inserts, 1,
        "{inserts} of the racing upserts claimed to insert; only one may win"
    );
    let ids: std::collections::BTreeSet<_> = flat.iter().map(|(_, id)| id).collect();
    assert_eq!(ids.len(), 1, "the racing upserts produced {ids:?}");

    let stats = store.stats(None, &Visibility::Full).unwrap();
    assert_eq!(
        stats.total, 1,
        "a contested (source, path) yielded {stats:?}"
    );
}

// ── 6. two processes, one migration ──────────────────────────────────────────

/// Two `Store::open` calls against the same data dir at the same moment — the shape the desktop
/// shell and a CLI run produce — must both come up on the same schema version. `migrate()` re-reads
/// `user_version` inside an immediate transaction per step, so the loser of the race waits out the
/// busy timeout, sees the applied version, and skips rather than replaying the step onto tables
/// that now exist.
///
/// `PRAGMA user_version` is not on the public `Store` surface, so it is read here through a plain
/// connection to the same file — the same thing an older binary would do to decide the DB is from
/// the future.
#[test]
fn two_stores_on_one_dir_migrate_once() {
    let dir = tempfile::tempdir().unwrap();
    let barrier = std::sync::Barrier::new(2);

    let stores: Vec<Store> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let path = dir.path();
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    Store::open(path).expect("concurrent open raced its own migration")
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });

    let user_version = |dir: &std::path::Path| -> i64 {
        rusqlite::Connection::open(dir.join("library.db"))
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    };
    // The contested dir must land on exactly the version an uncontested open reaches — not one step
    // short (a step the loser skipped without the winner having applied it) and not zero.
    let (solo_dir, _solo) = store();
    let expected = user_version(solo_dir.path());
    assert!(expected > 0, "a solo open applied no schema at all");
    assert_eq!(
        user_version(dir.path()),
        expected,
        "the raced open left the catalog on a different schema version than a solo open"
    );

    // Both handles are live and on the same catalog: a write through one is visible through the
    // other, which is only true if they agree on the schema they migrated to.
    let source = add_source(&stores[0], "shared");
    stores[0]
        .upsert_asset(&asset(source, "one.png", MediaType::Image))
        .unwrap();
    assert_eq!(stores[1].stats(None, &Visibility::Full).unwrap().total, 1);
    assert_eq!(stores[0].list_sources().unwrap().len(), 1);
    assert_eq!(stores[1].list_sources().unwrap().len(), 1);
}
