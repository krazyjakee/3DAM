//! Durable discovery is separate from hash admission; restart and changed-content serving use
//! the same production scheduler and service methods as an embedded client.

use dam_api::dto::*;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::id::{ContentHash, JobId, SourceId};
use dam_api::service::{AuthContext, LibraryService};
use dam_api::LibError;
use dam_core::{EmbeddedLibrary, ResourceOptions};
use futures::StreamExt;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(40);

fn png(path: &Path, width: u32, padded_size: usize) -> Vec<u8> {
    image::RgbaImage::from_pixel(width, 1, image::Rgba([90, 130, 170, 255]))
        .save_with_format(path, image::ImageFormat::Png)
        .unwrap();
    let mut bytes = std::fs::read(path).unwrap();
    // Trailing bytes leave the PNG valid while exercising the entire-file hash path.
    bytes.resize(bytes.len().max(padded_size), 0x5a);
    std::fs::write(path, &bytes).unwrap();
    bytes
}

async fn add_source(lib: &EmbeddedLibrary, ctx: &AuthContext, root: &Path) -> SourceId {
    lib.add_source(
        ctx,
        AddSource {
            kind: SourceKind::LocalFs,
            uri: root.to_string_lossy().into_owned(),
            name: Some("quick lifecycle".into()),
            options: SourceOptions::default(),
        },
    )
    .await
    .unwrap()
}

async fn scan(lib: &EmbeddedLibrary, ctx: &AuthContext, source: SourceId, mode: ScanMode) -> JobId {
    lib.submit_scan(
        ctx,
        ScanRequest {
            sources: vec![source],
            mode,
        },
    )
    .await
    .unwrap()
}

async fn wait_job(lib: &EmbeddedLibrary, ctx: &AuthContext, id: JobId) -> JobStatus {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let job = lib.get_job(ctx, &id).await.unwrap();
            match job.state {
                JobState::Done => {
                    assert!(
                        job.summary.is_some(),
                        "terminal result must be published together"
                    );
                    assert!(
                        job.warnings.is_empty(),
                        "unexpected warnings: {:?}",
                        job.warnings
                    );
                    return job;
                }
                JobState::Failed | JobState::Cancelled => panic!("job failed: {job:?}"),
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("job did not finish before the bounded deadline")
}

async fn enrich_jobs(lib: &EmbeddedLibrary, ctx: &AuthContext) -> Vec<JobStatus> {
    lib.list_jobs(
        ctx,
        JobListRequest {
            kinds: vec![JobKind::Enrich],
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .items
}

async fn wait_new_enrich(
    lib: &EmbeddedLibrary,
    ctx: &AuthContext,
    source: SourceId,
    previous: &BTreeSet<JobId>,
) -> JobStatus {
    tokio::time::timeout(DEADLINE, async {
        loop {
            for job in enrich_jobs(lib, ctx).await {
                if !previous.contains(&job.id) && job.sources.contains(&source) {
                    match job.state {
                        JobState::Done => {
                            assert!(job.summary.is_some());
                            assert!(
                                job.warnings.is_empty(),
                                "unexpected warnings: {:?}",
                                job.warnings
                            );
                            return job;
                        }
                        JobState::Failed | JobState::Cancelled => {
                            panic!("enrichment failed: {job:?}")
                        }
                        _ => {}
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("automatic enrichment did not finish before the bounded deadline")
}

fn pending_count(data: &Path) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        data.join("library.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row("SELECT count(*) FROM pending_ingest", [], |row| row.get(0))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_finishes_before_paced_hash_admission_and_enrichment_obeys_blocklist() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let data = tmp.path().join("data");
    std::fs::create_dir(&source).unwrap();
    let bytes = png(&source.join("paced.png"), 1, 1024 * 1024);
    let expected_hash = ContentHash(*blake3::hash(&bytes).as_bytes());
    let mut options = ResourceOptions::ungoverned();
    options.io.max_mib_per_sec = Some(1);
    options.io.concurrency = Some(1);
    let lib = EmbeddedLibrary::open_with(&data, options).await.unwrap();
    let ctx = AuthContext::embedded();
    let mut events = lib
        .subscribe(&ctx, SubscribeRequest::default())
        .await
        .unwrap();
    let sid = add_source(&lib, &ctx, &source).await;
    let quick = scan(&lib, &ctx, sid, ScanMode::Quick).await;

    tokio::time::timeout(DEADLINE, async {
        loop {
            match events.next().await.expect("event stream closed") {
                LibraryEvent::AssetAdded(_) => panic!("unverified discovery entered the catalog"),
                LibraryEvent::JobProgress(job) if job.id == quick => match job.state {
                    JobState::Done => break,
                    JobState::Failed | JobState::Cancelled => {
                        panic!("quick discovery failed: {job:?}")
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    })
    .await
    .expect("quick discovery did not finish");
    assert_eq!(lib.library_stats(&ctx, None).await.unwrap().total, 0);
    assert_eq!(pending_count(&data), 1);
    wait_job(&lib, &ctx, quick).await;
    wait_new_enrich(&lib, &ctx, sid, &BTreeSet::new()).await;
    assert_eq!(pending_count(&data), 0);
    let page = lib.query(&ctx, QueryRequest::default()).await.unwrap();
    assert_eq!(page.items.len(), 1);
    let asset = lib.get_asset(&ctx, &page.items[0].id).await.unwrap();
    assert_eq!(asset.hash, Some(expected_hash));

    let previous = enrich_jobs(&lib, &ctx)
        .await
        .into_iter()
        .map(|job| job.id)
        .collect();
    lib.remove_asset(&ctx, &asset.summary.id, RemoveAsset { block: true })
        .await
        .unwrap();
    let quick = scan(&lib, &ctx, sid, ScanMode::Quick).await;
    wait_job(&lib, &ctx, quick).await;
    wait_new_enrich(&lib, &ctx, sid, &previous).await;
    assert_eq!(pending_count(&data), 0);
    assert_eq!(lib.library_stats(&ctx, None).await.unwrap().total, 0);
    assert!(lib
        .list_blocklist(&ctx)
        .await
        .unwrap()
        .iter()
        .any(|entry| entry.hash == expected_hash));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_resumes_durable_discovery_without_another_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let data = tmp.path().join("data");
    std::fs::create_dir(&source).unwrap();
    let bytes = png(&source.join("resume.png"), 1, 0);
    let options = ResourceOptions {
        defer_ingest: true,
        ..ResourceOptions::ungoverned()
    };
    let lib = EmbeddedLibrary::open_with(&data, options).await.unwrap();
    let ctx = AuthContext::embedded();
    let sid = add_source(&lib, &ctx, &source).await;
    let quick = scan(&lib, &ctx, sid, ScanMode::Quick).await;
    wait_job(&lib, &ctx, quick).await;
    assert_eq!(lib.library_stats(&ctx, None).await.unwrap().total, 0);
    assert_eq!(pending_count(&data), 1);
    assert!(enrich_jobs(&lib, &ctx).await.is_empty());
    drop(lib);

    let resumed = EmbeddedLibrary::open_with(&data, ResourceOptions::ungoverned())
        .await
        .unwrap();
    wait_new_enrich(&resumed, &ctx, sid, &BTreeSet::new()).await;
    assert_eq!(pending_count(&data), 0);
    let page = resumed.query(&ctx, QueryRequest::default()).await.unwrap();
    assert_eq!(page.items.len(), 1);
    let asset = resumed.get_asset(&ctx, &page.items[0].id).await.unwrap();
    assert_eq!(
        asset.hash,
        Some(ContentHash(*blake3::hash(&bytes).as_bytes()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_pending_content_rejects_cached_reads_and_preserves_user_annotations() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let data = tmp.path().join("data");
    std::fs::create_dir(&source).unwrap();
    let path = source.join("changed.png");
    png(&path, 1, 0);
    let options = ResourceOptions {
        defer_ingest: true,
        ..ResourceOptions::ungoverned()
    };
    let lib = EmbeddedLibrary::open_with(&data, options).await.unwrap();
    let ctx = AuthContext::embedded();
    let sid = add_source(&lib, &ctx, &source).await;
    let full = scan(&lib, &ctx, sid, ScanMode::Full).await;
    wait_job(&lib, &ctx, full).await;
    let page = lib.query(&ctx, QueryRequest::default()).await.unwrap();
    let id = page.items[0].id;
    let old_hash = lib.get_asset(&ctx, &id).await.unwrap().hash;
    lib.read_thumbnail(&ctx, &id, 64).await.unwrap();
    lib.set_favorite(
        &ctx,
        FavoriteRequest {
            asset: id,
            favorite: true,
        },
    )
    .await
    .unwrap();
    lib.set_note(
        &ctx,
        &id,
        NoteRequest {
            body: "Keep the client annotation".into(),
        },
    )
    .await
    .unwrap();

    // A different size makes revision detection deterministic even on coarse-mtime filesystems.
    let changed = png(&path, 2, 512);
    let quick = scan(&lib, &ctx, sid, ScanMode::Quick).await;
    wait_job(&lib, &ctx, quick).await;
    let pending = lib.get_asset(&ctx, &id).await.unwrap();
    assert!(pending.hash.is_none());
    assert!(matches!(pending.attributes, MediaAttributes::None));
    assert!(pending.summary.favorite);
    assert_eq!(pending.note.unwrap().body, "Keep the client annotation");
    assert!(matches!(
        lib.read_content(&ctx, &id).await,
        Err(LibError::Conflict(_))
    ));
    assert!(matches!(
        lib.content_metadata(&ctx, &id).await,
        Err(LibError::Conflict(_))
    ));
    assert!(matches!(
        lib.read_thumbnail(&ctx, &id, 64).await,
        Err(LibError::Conflict(_))
    ));
    assert_eq!(pending_count(&data), 1);
    drop(lib);

    let resumed = EmbeddedLibrary::open_with(&data, ResourceOptions::ungoverned())
        .await
        .unwrap();
    wait_new_enrich(&resumed, &ctx, sid, &BTreeSet::new()).await;
    let verified = resumed.get_asset(&ctx, &id).await.unwrap();
    assert_eq!(
        verified.hash,
        Some(ContentHash(*blake3::hash(&changed).as_bytes()))
    );
    assert_ne!(verified.hash, old_hash);
    assert!(verified.summary.favorite);
    assert_eq!(verified.note.unwrap().body, "Keep the client annotation");
    match verified.attributes {
        MediaAttributes::Image(image) => assert_eq!(image.width, Some(2)),
        attributes => panic!("image metadata was not enriched: {attributes:?}"),
    }
    resumed.read_content(&ctx, &id).await.unwrap();
    resumed.read_thumbnail(&ctx, &id, 64).await.unwrap();
    assert_eq!(pending_count(&data), 0);
}
