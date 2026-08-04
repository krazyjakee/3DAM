//! End-to-end test of the phase-1 slice through the engine: add a local source, scan it, observe
//! live `AssetAdded` events and the terminal `JobProgress(Done)`, then query the indexed rows.
//!
//! The second half of the file covers the chunked persistence of issue #138: a scan no longer
//! writes one asset per transaction, so what has to be proved is that batching moved *when* rows
//! commit without moving what a client is allowed to conclude from an event.

use dam_api::dto::*;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::id::{JobId, SourceId};
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use futures::StreamExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unique_tmp() -> std::path::PathBuf {
    // Per-process atomic counter as well as a timestamp: tests run in parallel within one process,
    // and `as_nanos()` can coincide for two tests that start in the same clock tick — which would
    // silently share a data dir (schema "already exists", cross-contaminated asset counts).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-test-{}-{}-{}", std::process::id(), nanos, n))
}

#[tokio::test]
async fn scan_indexes_files_and_emits_events() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    std::fs::create_dir_all(assets.join("sub")).unwrap();
    std::fs::create_dir_all(assets.join("node_modules/dep")).unwrap();
    std::fs::write(assets.join("a.wav"), b"RIFF....WAVE").unwrap();
    std::fs::write(assets.join("b.png"), b"\x89PNG\r\n").unwrap();
    std::fs::write(assets.join("sub/c.gltf"), b"{\"asset\":{}}").unwrap();
    // A `.txt` is a document now (PRODUCT_SPEC §9 phase 2b) — it used to be skipped.
    std::fs::write(assets.join("note.txt"), b"a project note").unwrap();
    // …but the ingest ignore policy still keeps dependency boilerplate out of the catalog, which
    // is what stops one `npm install` from outnumbering a project's actual assets.
    std::fs::write(assets.join("node_modules/dep/README.md"), b"# dep").unwrap();
    // Structured data is deliberately not a document, so it remains unindexable.
    std::fs::write(assets.join("data.csv"), b"a,b\n1,2\n").unwrap();

    let lib =
        EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap();
    let ctx = AuthContext::embedded();

    // Subscribe before scanning so no events are missed.
    let mut events = lib
        .subscribe(&ctx, SubscribeRequest::default())
        .await
        .unwrap();

    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: assets.to_string_lossy().into_owned(),
                name: Some("t".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let job = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![sid],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();

    let mut added = 0;
    let mut finished = false;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(10), events.next()).await {
        match ev {
            LibraryEvent::AssetAdded(_) => added += 1,
            LibraryEvent::JobProgress(js) if js.id == job && js.state == JobState::Done => {
                finished = true;
                break;
            }
            _ => {}
        }
    }
    assert!(finished, "scan job should reach Done");
    assert_eq!(
        added, 4,
        "the 4 recognised files emit AssetAdded (node_modules doc + csv skipped)"
    );

    let page = lib
        .query(
            &ctx,
            QueryRequest {
                page: PageParams {
                    after: None,
                    limit: 50,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, Some(4));
    assert_eq!(page.items.len(), 4);

    let stats = lib.library_stats(&ctx, None).await.unwrap();
    assert_eq!(stats.total, 4);
    assert_eq!(stats.by_media.get("audio"), Some(&1));
    assert_eq!(stats.by_media.get("image"), Some(&1));
    assert_eq!(stats.by_media.get("model"), Some(&1));
    assert_eq!(stats.by_media.get("document"), Some(&1));

    // A re-scan must not duplicate rows (reconcile on source_id+path).
    let job2 = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![sid],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    loop {
        let j = lib.get_job(&ctx, &job2).await.unwrap();
        if matches!(j.state, JobState::Done | JobState::Failed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stats2 = lib.library_stats(&ctx, None).await.unwrap();
    assert_eq!(stats2.total, 4, "re-scan is idempotent");

    std::fs::remove_dir_all(&tmp).ok();
}

/// Issue #21: removing an asset with `block` records its content hash so a later scan never
/// re-imports the same bytes; lifting the block lets the next scan pick it back up.
#[tokio::test]
async fn remove_and_block_survives_rescan() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("keep.png"), b"\x89PNG\r\nKEEP").unwrap();
    std::fs::write(assets.join("drop.png"), b"\x89PNG\r\nDROP").unwrap();

    let lib =
        EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap();
    let ctx = AuthContext::embedded();

    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: assets.to_string_lossy().into_owned(),
                name: Some("t".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();

    let scan = |mode| {
        let lib = &lib;
        let ctx = &ctx;
        async move {
            let job = lib
                .submit_scan(
                    ctx,
                    ScanRequest {
                        sources: vec![sid],
                        mode,
                    },
                )
                .await
                .unwrap();
            loop {
                let j = lib.get_job(ctx, &job).await.unwrap();
                if matches!(j.state, JobState::Done | JobState::Failed) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };

    scan(ScanMode::Full).await;
    let page = lib
        .query(
            &ctx,
            QueryRequest {
                page: PageParams {
                    after: None,
                    limit: 50,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2, "both files import on first scan");
    let drop_id = page
        .items
        .iter()
        .find(|a| a.name == "drop.png")
        .expect("drop.png present")
        .id;

    // Remove + block: the row goes and the hash is recorded.
    lib.remove_asset(&ctx, &drop_id, RemoveAsset { block: true })
        .await
        .unwrap();
    assert_eq!(lib.list_blocklist(&ctx).await.unwrap().len(), 1);
    assert_eq!(lib.library_stats(&ctx, None).await.unwrap().total, 1);

    // A full re-scan must NOT re-import the blocked bytes (they're still on disk).
    scan(ScanMode::Full).await;
    assert_eq!(
        lib.library_stats(&ctx, None).await.unwrap().total,
        1,
        "blocked hash is skipped on rescan"
    );

    // Lift the block, and the next scan re-imports the file.
    let blocked = lib.list_blocklist(&ctx).await.unwrap();
    lib.unblock(&ctx, &blocked[0].hash).await.unwrap();
    scan(ScanMode::Full).await;
    assert_eq!(
        lib.library_stats(&ctx, None).await.unwrap().total,
        2,
        "unblocked content is re-imported"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

/// Remove + block is content-addressed dedup disposal: blocking one member of an exact-duplicate
/// group (byte-identical copies sharing a content hash) purges the *whole* group, not just the
/// clicked copy. A distinct asset is untouched, and the block still gates a rescan.
#[tokio::test]
async fn remove_and_block_purges_all_identical_copies() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    // Three byte-identical copies (one exact-dup group) + one distinct file.
    let dup = b"\x89PNG\r\nDUP-BYTES";
    std::fs::write(assets.join("a.png"), dup).unwrap();
    std::fs::write(assets.join("b.png"), dup).unwrap();
    std::fs::write(assets.join("c.png"), dup).unwrap();
    std::fs::write(assets.join("other.png"), b"\x89PNG\r\nOTHER").unwrap();

    let lib =
        EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap();
    let ctx = AuthContext::embedded();
    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: assets.to_string_lossy().into_owned(),
                name: Some("t".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();

    let job = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![sid],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    loop {
        let j = lib.get_job(&ctx, &job).await.unwrap();
        if matches!(j.state, JobState::Done | JobState::Failed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let page = lib
        .query(
            &ctx,
            QueryRequest {
                page: PageParams {
                    after: None,
                    limit: 50,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4, "all four files import");
    // Pick any one of the three identical copies to remove + block.
    let one_copy = page
        .items
        .iter()
        .find(|a| a.name == "b.png")
        .expect("b.png present")
        .id;

    lib.remove_asset(&ctx, &one_copy, RemoveAsset { block: true })
        .await
        .unwrap();

    // All three byte-identical copies are gone; the distinct file remains.
    assert_eq!(
        lib.library_stats(&ctx, None).await.unwrap().total,
        1,
        "the whole exact-duplicate group is purged, not just the clicked copy"
    );
    let remaining = lib
        .query(
            &ctx,
            QueryRequest {
                page: PageParams {
                    after: None,
                    limit: 50,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(remaining.items.len(), 1);
    assert_eq!(
        remaining.items[0].name, "other.png",
        "distinct asset untouched"
    );
    // One hash blocked (the group's), and a rescan does not re-import the identical bytes.
    assert_eq!(lib.list_blocklist(&ctx).await.unwrap().len(), 1);

    std::fs::remove_dir_all(&tmp).ok();
}

// ── batched persistence (issue #138) ─────────────────────────────────────────────────────────

/// Comfortably more than two 128-row chunks, and deliberately not a multiple of one: the tail is a
/// partial chunk, which is where a flush that ran in the wrong order would show up.
const MANY: usize = 300;

/// A distinct fake PNG per index. The bytes only have to hash differently and be catalogued; the
/// cheap tier fails to parse them and records no attributes, exactly as the older tests rely on.
fn write_pngs(dir: &Path, count: usize) {
    std::fs::create_dir_all(dir).unwrap();
    for i in 0..count {
        std::fs::write(dir.join(format!("item_{i:04}.png")), png_bytes(i, "")).unwrap();
    }
}

/// A PNG signature plus per-index filler, so every file hashes differently and `suffix` can make a
/// rewrite differ in *length* from the original.
fn png_bytes(i: usize, suffix: &str) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n".to_vec();
    bytes.extend_from_slice(format!("item number {i}{suffix}").as_bytes());
    bytes
}

async fn open_lib(tmp: &Path) -> EmbeddedLibrary {
    EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
        .await
        .unwrap()
}

async fn add_local_source(lib: &EmbeddedLibrary, ctx: &AuthContext, dir: &Path) -> SourceId {
    lib.add_source(
        ctx,
        AddSource {
            kind: SourceKind::LocalFs,
            uri: dir.to_string_lossy().into_owned(),
            name: Some("t".into()),
            options: SourceOptions::default(),
        },
    )
    .await
    .unwrap()
}

async fn submit(
    lib: &EmbeddedLibrary,
    ctx: &AuthContext,
    sid: SourceId,
    mode: ScanMode,
) -> dam_api::id::JobId {
    lib.submit_scan(
        ctx,
        ScanRequest {
            sources: vec![sid],
            mode,
        },
    )
    .await
    .unwrap()
}

async fn wait_job(lib: &EmbeddedLibrary, ctx: &AuthContext, job: &JobId) -> JobStatus {
    for _ in 0..600 {
        let status = lib.get_job(ctx, job).await.unwrap();
        if matches!(
            status.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("scan job never reached a terminal state");
}

async fn scan_and_wait(
    lib: &EmbeddedLibrary,
    ctx: &AuthContext,
    sid: SourceId,
    mode: ScanMode,
) -> JobStatus {
    let job = submit(lib, ctx, sid, mode).await;
    wait_job(lib, ctx, &job).await
}

async fn total(lib: &EmbeddedLibrary, ctx: &AuthContext) -> u64 {
    lib.library_stats(ctx, None).await.unwrap().total
}

/// `AssetAdded` is a promise that the row is durable, and batching is exactly the change that could
/// break it: the events for a chunk are published from the outcome of the transaction that carried
/// it, never from inside. Every event is therefore read back the instant it arrives — a publish
/// that had moved above the commit would fail this on the very first one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asset_added_events_are_only_emitted_after_commit() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    write_pngs(&assets, MANY);

    let lib = open_lib(&tmp).await;
    let ctx = AuthContext::embedded();
    let mut events = lib
        .subscribe(&ctx, SubscribeRequest::default())
        .await
        .unwrap();
    let sid = add_local_source(&lib, &ctx, &assets).await;
    let job = submit(&lib, &ctx, sid, ScanMode::Full).await;

    let mut added = 0usize;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(60), events.next())
            .await
            .expect("scan events stopped arriving")
            .expect("event stream ended early");
        match ev {
            LibraryEvent::AssetAdded(summary) => {
                if let Err(error) = lib.get_asset(&ctx, &summary.id).await {
                    panic!(
                        "AssetAdded for “{}” arrived before its row was readable: {error}",
                        summary.name
                    );
                }
                added += 1;
            }
            LibraryEvent::StreamLagged => panic!("the test consumer lagged; counts are unusable"),
            LibraryEvent::JobProgress(js) if js.id == job && js.state == JobState::Done => break,
            _ => {}
        }
    }
    assert_eq!(added, MANY, "every scanned file should be announced once");
    assert_eq!(total(&lib, &ctx).await, MANY as u64);

    std::fs::remove_dir_all(&tmp).ok();
}

/// Cancelling mid-scan keeps whatever the in-flight chunk had already computed: those rows are
/// legitimately ingested (scan is idempotent and non-destructive) and their `AssetAdded` events are
/// already out, so rolling the batch back would make published events lie.
///
/// The file count is large enough that a cancellation issued on the *first* announced asset — i.e.
/// once at least one chunk has committed — still lands with most of the walk to go.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_scan_commits_its_partial_batch() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    write_pngs(&assets, 4000);

    let lib = open_lib(&tmp).await;
    let ctx = AuthContext::embedded();
    let mut events = lib
        .subscribe(&ctx, SubscribeRequest::default())
        .await
        .unwrap();
    let sid = add_local_source(&lib, &ctx, &assets).await;
    let job = submit(&lib, &ctx, sid, ScanMode::Full).await;

    // Drain until the stream goes quiet: the job row reads `cancelled` from the moment the request
    // lands, so the terminal event is not a usable stopping point — the run is over when it stops
    // publishing.
    let mut added = 0usize;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), events.next()).await {
        match ev {
            LibraryEvent::AssetAdded(_) => {
                added += 1;
                if added == 1 {
                    lib.cancel_job(&ctx, &job).await.unwrap();
                }
            }
            LibraryEvent::StreamLagged => panic!("the test consumer lagged; counts are unusable"),
            _ => {}
        }
    }

    let status = wait_job(&lib, &ctx, &job).await;
    assert_eq!(
        status.state,
        JobState::Cancelled,
        "the scan outran the cancellation; the assertions below would be vacuous"
    );
    assert!(added > 0, "cancellation landed before anything committed");
    assert_eq!(
        total(&lib, &ctx).await,
        added as u64,
        "the catalog and the announced assets disagree: a partial batch was rolled back \
         (or committed without being announced)"
    );
    // A cancelled walk is never authoritative, so reconciliation — which marks unseen rows missing
    // and stamps the source's success time in one transaction — must not have run at all.
    let src = lib.get_source(&ctx, &sid).await.unwrap();
    assert!(
        src.stats.last_scanned_at.is_none(),
        "a cancelled scan finalised its source, which is where files get marked missing"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

/// Progress is persisted *inside* the batch transaction, so it must describe only rows that are
/// already durable — never the ones the very same transaction is still writing.
///
/// Asserted off the event log alone, which is a serialised record of the run: each batch publishes
/// its `AssetAdded`s and then its `JobProgress`, so at any progress event the number of assets
/// announced so far *is* the number of committed rows (a first full scan writes only inserts). A
/// `done` that optimistically counted the batch being committed would exceed it by up to a chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_progress_never_exceeds_committed_rows() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    write_pngs(&assets, MANY);

    let lib = open_lib(&tmp).await;
    let ctx = AuthContext::embedded();
    let mut events = lib
        .subscribe(&ctx, SubscribeRequest::default())
        .await
        .unwrap();
    let sid = add_local_source(&lib, &ctx, &assets).await;
    let job = submit(&lib, &ctx, sid, ScanMode::Full).await;

    let mut announced = 0u64;
    let mut peak_running = 0u64;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(60), events.next())
            .await
            .expect("scan events stopped arriving")
            .expect("event stream ended early");
        match ev {
            LibraryEvent::AssetAdded(_) => announced += 1,
            LibraryEvent::StreamLagged => panic!("the test consumer lagged; counts are unusable"),
            LibraryEvent::JobProgress(js) if js.id == job => {
                assert!(
                    js.progress.done <= announced,
                    "progress reported {} done with only {announced} assets committed",
                    js.progress.done
                );
                if js.state == JobState::Running {
                    peak_running = peak_running.max(js.progress.done);
                }
                if js.state == JobState::Done {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        peak_running > 0,
        "no progress was ever persisted mid-scan, so the bound above proved nothing"
    );
    assert_eq!(announced, MANY as u64);

    std::fs::remove_dir_all(&tmp).ok();
}

/// The riskiest half of the change: delta re-scan stamps are no longer written one path at a time,
/// they are written a chunk ahead of the work — and the mark-missing pass at the end of the walk
/// trusts those stamps completely. Flush the tail chunk after reconciliation instead of before and
/// up to 128 present files get marked missing.
///
/// The re-scan therefore has to sort three populations correctly in one pass: files that vanished
/// (missing), files that changed (re-ingested), and files that did not (skipped without opening
/// their bytes) — and then a third pass must find nothing left to reconcile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delta_rescan_batches_stamps_without_breaking_reconciliation() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    write_pngs(&assets, MANY);

    let lib = open_lib(&tmp).await;
    let ctx = AuthContext::embedded();
    let sid = add_local_source(&lib, &ctx, &assets).await;

    let first = scan_and_wait(&lib, &ctx, sid, ScanMode::Full).await;
    assert_eq!(first.state, JobState::Done);
    assert_eq!(first.summary.as_deref(), Some("Scanned 300 item(s)"));
    assert_eq!(total(&lib, &ctx).await, MANY as u64);

    // Five files vanish and three change. Directory order is the filesystem's, not ours, so these
    // land wherever they land across the chunks — which is the point.
    let removed = [0usize, 7, 128, 199, 299];
    for i in removed {
        std::fs::remove_file(assets.join(format!("item_{i:04}.png"))).unwrap();
    }
    let changed = [3usize, 150, 288];
    for i in changed {
        // A different length, so the (size, mtime) change token differs even if the clock's
        // millisecond resolution cannot tell the rewrite from the original write.
        std::fs::write(
            assets.join(format!("item_{i:04}.png")),
            png_bytes(i, " — rewritten with rather more bytes than before"),
        )
        .unwrap();
    }

    let delta = scan_and_wait(&lib, &ctx, sid, ScanMode::Delta).await;
    assert_eq!(delta.state, JobState::Done);
    assert_eq!(
        delta.summary.as_deref(),
        Some("Scanned 295 item(s) (292 unchanged, 5 missing)"),
        "the delta pass mis-sorted vanished / changed / unchanged files"
    );
    assert!(delta.warnings.is_empty(), "{:?}", delta.warnings);
    // Missing is a flag, not a delete: nothing is destroyed by a re-scan.
    assert_eq!(total(&lib, &ctx).await, MANY as u64);

    // And the pass left the catalog settled: every surviving row kept the generation stamp its
    // chunk wrote (or it would be reported missing now), the re-ingested three carry their new
    // change tokens, and the five already flagged are not re-counted.
    let again = scan_and_wait(&lib, &ctx, sid, ScanMode::Delta).await;
    assert_eq!(
        again.summary.as_deref(),
        Some("Scanned 295 item(s) (295 unchanged)"),
        "a settled catalog re-reported work on an idempotent re-scan"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

/// Fail-soft per item survives batching: one file the process cannot read degrades itself into a
/// job warning while every other file in its chunk is still catalogued. (The store-side twin — an
/// item whose *write* aborts inside the transaction — is `dam-store`'s
/// `a_failing_item_does_not_sink_its_batch`; this covers the half that never reaches the batch.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn a_single_unwritable_asset_degrades_only_itself() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    write_pngs(&assets, MANY);
    let unreadable = assets.join("unreadable.png");
    std::fs::write(&unreadable, b"\x89PNG\r\nno one may read this").unwrap();
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&unreadable).is_ok() {
        // Running as root, where a mode of 000 is unenforceable and the assertions below would be
        // asserting nothing.
        std::fs::remove_dir_all(&tmp).ok();
        return;
    }

    let lib = open_lib(&tmp).await;
    let ctx = AuthContext::embedded();
    let sid = add_local_source(&lib, &ctx, &assets).await;
    let status = scan_and_wait(&lib, &ctx, sid, ScanMode::Full).await;

    assert_eq!(
        status.state,
        JobState::Done,
        "one unreadable file must not fail the job"
    );
    assert_eq!(
        total(&lib, &ctx).await,
        MANY as u64,
        "the unreadable file took its chunk-mates down with it"
    );
    assert_eq!(status.summary.as_deref(), Some("Scanned 301 item(s)"));
    assert!(
        status
            .warnings
            .iter()
            .any(|w| w.contains("unreadable.png") && w.contains("could not be read")),
        "the degraded item should be itemised: {:?}",
        status.warnings
    );

    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).ok();
    std::fs::remove_dir_all(&tmp).ok();
}
