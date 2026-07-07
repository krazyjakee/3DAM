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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-test-{}-{}", std::process::id(), nanos))
}

#[tokio::test]
async fn scan_indexes_files_and_emits_events() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    std::fs::create_dir_all(assets.join("sub")).unwrap();
    std::fs::write(assets.join("a.wav"), b"RIFF....WAVE").unwrap();
    std::fs::write(assets.join("b.png"), b"\x89PNG\r\n").unwrap();
    std::fs::write(assets.join("sub/c.gltf"), b"{\"asset\":{}}").unwrap();
    std::fs::write(assets.join("note.txt"), b"not an asset").unwrap(); // must be skipped

    let lib = EmbeddedLibrary::open(&tmp.join("data")).await.unwrap();
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
        added, 3,
        "exactly the 3 recognised files emit AssetAdded (txt skipped)"
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
    assert_eq!(page.total, Some(3));
    assert_eq!(page.items.len(), 3);

    let stats = lib.library_stats(&ctx).await.unwrap();
    assert_eq!(stats.total, 3);
    assert_eq!(stats.by_media.get("audio"), Some(&1));
    assert_eq!(stats.by_media.get("image"), Some(&1));
    assert_eq!(stats.by_media.get("model"), Some(&1));

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
    let stats2 = lib.library_stats(&ctx).await.unwrap();
    assert_eq!(stats2.total, 3, "re-scan is idempotent");

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

    let lib = EmbeddedLibrary::open(&tmp.join("data")).await.unwrap();
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
    assert_eq!(lib.library_stats(&ctx).await.unwrap().total, 1);

    // A full re-scan must NOT re-import the blocked bytes (they're still on disk).
    scan(ScanMode::Full).await;
    assert_eq!(
        lib.library_stats(&ctx).await.unwrap().total,
        1,
        "blocked hash is skipped on rescan"
    );

    // Lift the block, and the next scan re-imports the file.
    let blocked = lib.list_blocklist(&ctx).await.unwrap();
    lib.unblock(&ctx, &blocked[0].hash).await.unwrap();
    scan(ScanMode::Full).await;
    assert_eq!(
        lib.library_stats(&ctx).await.unwrap().total,
        2,
        "unblocked content is re-imported"
    );

    std::fs::remove_dir_all(&tmp).ok();
}
