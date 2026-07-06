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
    let mut events = lib.subscribe(&ctx, SubscribeRequest::default()).await.unwrap();

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
    assert_eq!(added, 3, "exactly the 3 recognised files emit AssetAdded (txt skipped)");

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
        .submit_scan(&ctx, ScanRequest { sources: vec![sid], mode: ScanMode::Full })
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
