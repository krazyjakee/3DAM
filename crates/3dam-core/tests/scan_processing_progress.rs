//! Production scan progress while a backend is still streaming its next file (issue #201).

use dam_api::dto::*;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::service::{AuthContext, LibraryService};
use dam_api::LibError;
use dam_core::{EmbeddedLibrary, ResourceOptions};
use dam_sources::{Fetched, FileEntry, FileSource};
use futures::StreamExt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const CHUNK: usize = 256 * 1024;

struct StreamingSource {
    scratch: PathBuf,
    baseline: bool,
    entered_slow_fetch: AtomicBool,
    finished_slow_fetch: AtomicBool,
    stopped_slow_fetch: AtomicBool,
}

struct Stopped<'a>(&'a AtomicBool);
impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl FileSource for StreamingSource {
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        let entries: &[(&str, u64)] = if self.baseline {
            &[("existing.txt", 16)]
        } else {
            &[("first.txt", 16), ("slow.txt", (CHUNK * 8) as u64)]
        };
        for &(name, size) in entries {
            if !sink(Ok(FileEntry {
                rel_path: name.into(),
                size,
                modified_ms: Some(1),
            })) {
                break;
            }
        }
        Ok(())
    }

    fn fetch(&self, rel: &str) -> Result<Fetched, LibError> {
        self.fetch_paced(rel, &mut |_| Ok(()))
    }

    fn fetch_paced(
        &self,
        rel: &str,
        pace: &mut dyn FnMut(u64) -> Result<(), LibError>,
    ) -> Result<Fetched, LibError> {
        let mut output = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile_in(&self.scratch)
            .map_err(|error| LibError::SourceUnavailable(error.to_string()))?;
        if rel != "slow.txt" {
            pace(16)?;
            output
                .write_all(b"ready small file")
                .map_err(|error| LibError::SourceUnavailable(error.to_string()))?;
            return Ok(Fetched::Temp(output));
        }
        self.entered_slow_fetch.store(true, Ordering::Relaxed);
        let _stopped = Stopped(&self.stopped_slow_fetch);
        let bytes = vec![b'x'; CHUNK];
        for _ in 0..8 {
            pace(CHUNK as u64)?;
            // The production governor's progress callback is exercised before each bounded read.
            // Eight chunks take about one second; assertions concern event order, not host speed.
            std::thread::sleep(Duration::from_millis(125));
            output
                .write_all(&bytes)
                .map_err(|error| LibError::SourceUnavailable(error.to_string()))?;
        }
        self.finished_slow_fetch.store(true, Ordering::Relaxed);
        Ok(Fetched::Temp(output))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_items_commit_during_slow_fetch_and_cancellation_does_not_mark_missing() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("source");
    let data = fixture.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let library = EmbeddedLibrary::open_with(
        &data,
        ResourceOptions {
            defer_ingest: true,
            ..ResourceOptions::ungoverned()
        },
    )
    .await
    .unwrap();
    let ctx = AuthContext::embedded();
    let source_id = library
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: root.to_string_lossy().into_owned(),
                name: Some("streaming fixture".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let backend = |baseline| {
        Arc::new(StreamingSource {
            scratch: library.scratch_dir(),
            baseline,
            entered_slow_fetch: AtomicBool::new(false),
            finished_slow_fetch: AtomicBool::new(false),
            stopped_slow_fetch: AtomicBool::new(false),
        })
    };
    let baseline = library
        .submit_scan_with_source(
            &ctx,
            ScanRequest {
                sources: vec![source_id],
                mode: ScanMode::Full,
            },
            backend(true),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = library.get_job(&ctx, &baseline).await.unwrap();
            if status.state == JobState::Done {
                break;
            }
            assert_ne!(status.state, JobState::Failed, "{:?}", status.error);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let last_success = library
        .get_source(&ctx, &source_id)
        .await
        .unwrap()
        .stats
        .last_scanned_at;
    assert!(last_success.is_some());
    let mut events = library
        .subscribe(&ctx, SubscribeRequest::default())
        .await
        .unwrap();
    let streaming = backend(false);
    let job = library
        .submit_scan_with_source(
            &ctx,
            ScanRequest {
                sources: vec![source_id],
                mode: ScanMode::Full,
            },
            streaming.clone(),
        )
        .await
        .unwrap();
    let mut announced = 0u64;
    let mut committed_during_fetch = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.next().await.expect("event stream ended") {
                LibraryEvent::AssetAdded(asset) => {
                    assert_eq!(
                        asset.name, "first.txt",
                        "slow file finished before cancellation"
                    );
                    assert!(streaming.entered_slow_fetch.load(Ordering::Relaxed));
                    assert!(!streaming.finished_slow_fetch.load(Ordering::Relaxed));
                    library.get_asset(&ctx, &asset.id).await.unwrap();
                    announced += 1;
                }
                LibraryEvent::JobProgress(status) if status.id == job => {
                    assert!(
                        status.progress.done <= announced,
                        "progress exceeded durable announcements"
                    );
                    if announced == 1
                        && status.state == JobState::Running
                        && status
                            .progress
                            .current
                            .as_deref()
                            .is_some_and(|current| current.contains("slow.txt"))
                    {
                        assert!(!streaming.finished_slow_fetch.load(Ordering::Relaxed));
                        committed_during_fetch = true;
                        library.cancel_job(&ctx, &job).await.unwrap();
                        break;
                    }
                    assert!(!matches!(status.state, JobState::Done | JobState::Failed));
                }
                LibraryEvent::StreamLagged => panic!("the event consumer lagged"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(committed_during_fetch);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !streaming.stopped_slow_fetch.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!streaming.finished_slow_fetch.load(Ordering::Relaxed));
    assert_eq!(
        library.get_job(&ctx, &job).await.unwrap().state,
        JobState::Cancelled
    );
    assert_eq!(
        library
            .get_source(&ctx, &source_id)
            .await
            .unwrap()
            .stats
            .last_scanned_at,
        last_success
    );
    let connection = rusqlite::Connection::open_with_flags(
        data.join("library.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let flags: i64 = connection
        .query_row(
            "SELECT flags FROM asset WHERE source_id=?1 AND path='existing.txt'",
            [source_id.as_bytes().to_vec()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        flags & 1,
        0,
        "cancelled listing marked an unseen existing row missing"
    );
}

struct ChangedTokenSource(PathBuf);
impl FileSource for ChangedTokenSource {
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        sink(Ok(FileEntry {
            rel_path: "changed.txt".into(),
            size: 16,
            modified_ms: Some(1),
        }));
        Ok(())
    }
    fn fetch(&self, _: &str) -> Result<Fetched, LibError> {
        let mut file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile_in(&self.0)
            .unwrap();
        file.write_all(b"ready small file").unwrap();
        Ok(Fetched::HashedTemp {
            file,
            content_hash: blake3::hash(b"ready small file").to_hex().to_string(),
            source_stat: Some(dam_sources::ContentStat {
                len: 16,
                modified_ms: Some(2),
            }),
        })
    }
}

#[tokio::test]
async fn a_fetch_of_a_newer_revision_cannot_keep_the_listed_change_token() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("source");
    std::fs::create_dir_all(&root).unwrap();
    let lib = EmbeddedLibrary::open_with(
        &dir.path().join("data"),
        ResourceOptions {
            defer_ingest: true,
            ..ResourceOptions::ungoverned()
        },
    )
    .await
    .unwrap();
    let ctx = AuthContext::embedded();
    let source = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: root.to_string_lossy().into_owned(),
                name: None,
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let job = lib
        .submit_scan_with_source(
            &ctx,
            ScanRequest {
                sources: vec![source],
                mode: ScanMode::Full,
            },
            Arc::new(ChangedTokenSource(lib.scratch_dir())),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = lib.get_job(&ctx, &job).await.unwrap();
            if status.state == JobState::Done {
                assert!(!status.warnings.is_empty());
                break;
            }
            assert_ne!(status.state, JobState::Failed, "{:?}", status.error);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(lib.library_stats(&ctx, None).await.unwrap().total, 0);
}
