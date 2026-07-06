//! The scan job: walk each file source, hash bytes (BLAKE3), detect media, upsert the row, and
//! emit live events + job progress. Runs on a blocking thread (tech-spec 14: DB + file I/O off the
//! async runtime). Fail-soft: an unreadable file degrades that item, never the job.

use dam_api::dto::*;
use dam_api::event::LibraryEvent;
use dam_api::id::JobId;
use dam_store::{NewAsset, Store};
use dam_sources::{FileSource, LocalFsSource};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

const PROGRESS_EVERY: u64 = 16;

pub(crate) fn run_scan(
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    job: JobId,
    sources: Vec<SourceInfo>,
    cancel: Arc<AtomicBool>,
) {
    let _ = store.update_job_progress(&job, JobState::Running, 0, None, None);
    let mut done: u64 = 0;
    let mut warnings: u64 = 0;

    for src in sources {
        if src.kind != SourceKind::LocalFs {
            continue; // only local FS in phase 1
        }
        let sid = src.id;
        let fs = LocalFsSource::new(&src.uri);
        let scanned_at = dam_store::now_ms();

        let walk_result = fs.walk(&mut |entry| {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            match entry {
                Ok(fe) => {
                    let Some(det) = dam_media::detect(&fe.abs_path) else {
                        return true; // unhandled type: skip (fail-soft, DG §6)
                    };
                    let hash = match hash_file(&fe.abs_path) {
                        Some(h) => Some(h),
                        None => {
                            warnings += 1;
                            return true;
                        }
                    };
                    let na = NewAsset {
                        source_id: sid,
                        path: fe.rel_path.clone(),
                        filename: file_name(&fe.abs_path),
                        content_hash: hash,
                        size_bytes: Some(fe.size as i64),
                        source_modified_at: fe.modified_ms,
                        scanned_at,
                        media_type: det.media,
                        format: det.format.clone(),
                    };
                    match store.upsert_asset(&na) {
                        Ok((id, inserted)) => {
                            done += 1;
                            if inserted {
                                let summary = AssetSummary {
                                    id,
                                    name: na.filename.clone(),
                                    media: det.media,
                                    format: det.format,
                                    size: fe.size,
                                    license: LicenseBadge::default(),
                                    top_tags: Vec::new(),
                                    origin: Origin::Local,
                                    key_attrs: SmallMap::new(),
                                };
                                let _ = events.send(LibraryEvent::AssetAdded(summary));
                            }
                            if done.is_multiple_of(PROGRESS_EVERY) {
                                let _ = store.update_job_progress(
                                    &job,
                                    JobState::Running,
                                    done,
                                    None,
                                    Some(&fe.rel_path),
                                );
                                emit_progress(&store, &events, &job);
                            }
                        }
                        Err(e) => {
                            warnings += 1;
                            tracing::warn!(path = %fe.rel_path, error = %e, "skipped asset");
                        }
                    }
                    true
                }
                Err(_) => {
                    warnings += 1;
                    true
                }
            }
        });

        match walk_result {
            Ok(()) => {
                let _ = store.set_source_scanned(&sid, dam_store::now_ms());
            }
            Err(e) => {
                let _ = store.set_source_error(&sid, &e.to_string());
                tracing::warn!(source = %sid, error = %e, "source scan failed");
            }
        }
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = store.set_job_state(&job, JobState::Cancelled, None);
    } else {
        let _ = store.update_job_progress(&job, JobState::Done, done, Some(done), None);
        let note = if warnings > 0 {
            Some(format!("{warnings} item(s) skipped"))
        } else {
            None
        };
        let _ = store.set_job_state(&job, JobState::Done, note.as_deref());
    }
    emit_progress(&store, &events, &job);
    tracing::info!(%job, done, warnings, "scan finished");
}

fn emit_progress(store: &Store, events: &broadcast::Sender<LibraryEvent>, job: &JobId) {
    if let Ok(js) = store.get_job(job) {
        let _ = events.send(LibraryEvent::JobProgress(js));
    }
}

fn hash_file(path: &Path) -> Option<dam_api::id::ContentHash> {
    let mut hasher = blake3::Hasher::new();
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    std::io::copy(&mut reader, &mut hasher).ok()?;
    Some(dam_api::id::ContentHash(*hasher.finalize().as_bytes()))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
