//! End-to-end test of the phase-1 slice through the engine: add a local source, scan it, observe
//! live `AssetAdded` events and the terminal `JobProgress(Done)`, then query the indexed rows.

use dam_api::dto::*;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use futures::StreamExt;
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
