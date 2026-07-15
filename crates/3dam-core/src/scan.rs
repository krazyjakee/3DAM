//! The scan job: for each file source, walk it (local FS / SFTP / SMB behind one trait), resolve
//! each entry's bytes to a local path, hash (BLAKE3), detect media, upsert the row, and emit live
//! events + job progress. Runs on a blocking thread (tech-spec 14: DB + file I/O off the async
//! runtime). Fail-soft: an unreadable file degrades that item, an unreachable source pauses it,
//! never the whole job.
//!
//! Two modes (tech-spec 07 §2.1–§2.2). **Full** re-opens every file. **Delta** compares each entry's
//! cheap change token (size + mtime) to the stored row and only opens bytes for new/changed files;
//! either way, files that vanished are marked absent (non-destructive), never deleted.

use crate::emit_progress;
use dam_api::dto::*;
use dam_api::event::LibraryEvent;
use dam_api::id::JobId;
use dam_sources::{open_source, FileEntry};
use dam_store::{NewAsset, Store};
use std::collections::HashSet;
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
    mode: ScanMode,
    cancel: Arc<AtomicBool>,
    governor: &crate::resources::Governor,
) {
    // Establish the progress denominator up front so the job shows a real percentage + ETA rather
    // than an indeterminate bar. A **full** scan re-reads every file (and new files matter), so we
    // do a cheap metadata-only pre-walk to count exactly. A **delta** scan only opens bytes for
    // files whose size/mtime changed, so re-walking the whole tree just to count would double the
    // traversal — a second full network listing for SFTP/SMB — for no benefit. Instead we estimate
    // the total from the catalog's existing row count (already computed when the sources were
    // listed): off only by files added/removed since the last scan, and the bar clamps at 100%.
    // `None` ⇒ nothing countable ⇒ indeterminate bar, which still works.
    let total = match mode {
        ScanMode::Full => count_total(&store, &sources, &cancel),
        ScanMode::Delta => {
            let n: u64 = sources.iter().map(|s| s.stats.asset_count).sum();
            (n > 0).then_some(n)
        }
    };
    let _ = store.update_job_progress(&job, JobState::Running, 0, total, None);
    let mut done: u64 = 0;
    let mut examined: u64 = 0;
    let mut skipped: u64 = 0;
    let mut warnings: u64 = 0;
    let mut removed_total: u64 = 0;

    for src in sources {
        if src.kind == SourceKind::Federated {
            continue; // federated peers yield catalog rows, not bytes — not scanned here (phase 6)
        }
        let sid = src.id;

        // Rebuild the backend from the persisted connection (incl. secret). An unreachable host or
        // bad credentials mark the source offline and move on — degrade one edge, not the job.
        let conn = match store.get_source_connection(&sid) {
            Ok(c) => c,
            Err(e) => {
                let _ = store.set_source_error(&sid, &e.to_string());
                warnings += 1;
                continue;
            }
        };
        let fs = match open_source(&conn) {
            Ok(fs) => fs,
            Err(e) => {
                let _ = store.set_source_error(&sid, &e.to_string());
                tracing::warn!(source = %sid, error = %e, "source unavailable");
                warnings += 1;
                continue;
            }
        };

        // Snapshot existing paths for delta short-circuiting + removal detection (§2.2).
        let index = store.source_path_index(&sid).unwrap_or_default();
        let mut seen: HashSet<String> = HashSet::with_capacity(index.len());
        let scanned_at = dam_store::now_ms();

        let walk_result = fs.walk(&mut |entry| {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            match entry {
                Ok(fe) => {
                    seen.insert(fe.rel_path.clone());
                    // Detect by the logical path's extension (a remote temp file has a random name).
                    let Some(det) = dam_media::detect(Path::new(&fe.rel_path)) else {
                        return true; // unhandled type: skip (fail-soft, DG §6)
                    };
                    // Progress tracks every detectable file we examine — not just new/changed
                    // upserts — so a delta re-scan (mostly unchanged) advances the bar instead of
                    // sitting at 0 the whole time, and the % is against the pre-counted total.
                    examined += 1;
                    if examined.is_multiple_of(PROGRESS_EVERY) {
                        let _ = store.update_job_progress(
                            &job,
                            JobState::Running,
                            examined,
                            total,
                            Some(&fe.rel_path),
                        );
                        emit_progress(&store, &events, &job);
                    }
                    // Delta: unchanged (same size + mtime) → never open bytes (§2.2).
                    if mode == ScanMode::Delta && unchanged(&index, &fe) {
                        skipped += 1;
                        return true;
                    }
                    // Good-neighbour pacing (tech-spec 14 §3.4), placed exactly where the bulk
                    // I/O starts: everything above is directory metadata, everything below opens
                    // and hashes file bytes — the reads that can blockade a slow HDD. Pausing
                    // here lets the walk finish cheap entries while the disk recovers.
                    governor.pace(&cancel);
                    if cancel.load(Ordering::Relaxed) {
                        return false;
                    }
                    // Materialise bytes locally (in place for local, downloaded for remote).
                    let fetched = match fs.fetch(&fe.rel_path) {
                        Ok(f) => f,
                        Err(e) => {
                            warnings += 1;
                            tracing::warn!(path = %fe.rel_path, error = %e, "fetch failed");
                            return true;
                        }
                    };
                    let abs = fetched.path();
                    let hash = match hash_file(abs) {
                        Some(h) => Some(h),
                        None => {
                            warnings += 1;
                            return true;
                        }
                    };
                    // Blocklist (issue #21): a hash the user removed-and-blocked is never
                    // re-imported — skip it before the upsert so remove+block survives re-scans.
                    if let Some(h) = &hash {
                        if store.is_blocked(h).unwrap_or(false) {
                            skipped += 1;
                            return true;
                        }
                    }
                    let na = NewAsset {
                        source_id: sid,
                        path: fe.rel_path.clone(),
                        filename: file_name(&fe.rel_path),
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
                            // CHEAP tier (tech-spec 04 §4): header-only media attributes.
                            let attrs = dam_media::extract_metadata(abs, &det);
                            if let Err(e) = store.set_media_attrs(&id, &attrs) {
                                tracing::warn!(path = %fe.rel_path, error = %e, "attr persist failed");
                            }
                            if inserted {
                                // Whole-asset size: the file itself plus a model's external
                                // companion files (textures/buffers), matching the catalog read path.
                                let dep = match &attrs {
                                    dam_api::dto::MediaAttributes::Model(m) => {
                                        m.dependency_bytes.unwrap_or(0).max(0) as u64
                                    }
                                    _ => 0,
                                };
                                let summary = AssetSummary {
                                    id,
                                    name: na.filename.clone(),
                                    media: det.media,
                                    format: det.format.clone(),
                                    size: fe.size + dep,
                                    license: LicenseBadge::default(),
                                    top_tags: Vec::new(),
                                    origin: Origin::Local,
                                    key_attrs: key_attrs_of(&attrs),
                                    // A freshly-scanned asset is never a favourite yet.
                                    favorite: false,
                                };
                                let _ = events.send(LibraryEvent::AssetAdded(summary));
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
                // Rows we didn't see this pass have vanished from the source → mark absent (§2.2).
                if !cancel.load(Ordering::Relaxed) {
                    let removed: Vec<String> = index
                        .keys()
                        .filter(|p| !seen.contains(*p))
                        .cloned()
                        .collect();
                    match store.mark_paths_missing(&sid, &removed) {
                        Ok(n) => removed_total += n,
                        Err(e) => tracing::warn!(source = %sid, error = %e, "mark-missing failed"),
                    }
                }
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
        let _ = store.update_job_progress(
            &job,
            JobState::Done,
            examined,
            total.or(Some(examined)),
            None,
        );
        let mut notes = Vec::new();
        if skipped > 0 {
            notes.push(format!("{skipped} unchanged"));
        }
        if removed_total > 0 {
            notes.push(format!("{removed_total} missing"));
        }
        if warnings > 0 {
            notes.push(format!("{warnings} skipped"));
        }
        let note = (!notes.is_empty()).then(|| notes.join(", "));
        let _ = store.set_job_state(&job, JobState::Done, note.as_deref());
    }
    emit_progress(&store, &events, &job);
    tracing::info!(%job, done, skipped, removed = removed_total, warnings, "scan finished");
}

/// A delta entry is unchanged when its size and mtime both match the stored change token.
fn unchanged(
    index: &std::collections::HashMap<String, (Option<i64>, Option<i64>)>,
    fe: &FileEntry,
) -> bool {
    match index.get(&fe.rel_path) {
        Some((size, modified)) => {
            *size == Some(fe.size as i64) && *modified == fe.modified_ms && fe.modified_ms.is_some()
        }
        None => false,
    }
}

/// A couple of display attributes for the live-added grid row, mirroring the store's grid map so a
/// freshly scanned asset shows its dimensions/duration/tris immediately (before any refetch).
fn key_attrs_of(attrs: &MediaAttributes) -> SmallMap {
    let mut m = SmallMap::new();
    match attrs {
        MediaAttributes::Image(i) => {
            if let (Some(w), Some(h)) = (i.width, i.height) {
                m.insert("dimensions".into(), format!("{w}×{h}"));
            }
        }
        MediaAttributes::Audio(a) => {
            if let Some(ms) = a.duration_ms {
                let secs = ms as f64 / 1000.0;
                m.insert(
                    "duration".into(),
                    format!("{:.0}:{:02}", (secs / 60.0).floor(), (secs % 60.0) as i64),
                );
            }
        }
        MediaAttributes::Model(md) => {
            if let Some(t) = md.triangle_count {
                m.insert("tris".into(), t.to_string());
            }
        }
        MediaAttributes::None => {}
    }
    m
}

/// Cheap metadata-only pre-pass: count detectable files across all scannable sources so the scan
/// job can show a real percentage + ETA. Best-effort — a source that can't be opened or walked here
/// is simply left out of the estimate (the main pass reports its actual error); if nothing can be
/// counted we return `None` and the job falls back to an indeterminate bar. Walks only list metadata
/// (no byte fetch/hash), so this is far cheaper than the main pass it precedes.
fn count_total(store: &Store, sources: &[SourceInfo], cancel: &AtomicBool) -> Option<u64> {
    let mut total: u64 = 0;
    let mut counted_any = false;
    for src in sources {
        if src.kind == SourceKind::Federated {
            continue; // federated peers yield catalog rows, not bytes — not scanned here (phase 6)
        }
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(conn) = store.get_source_connection(&src.id) else {
            continue;
        };
        let Ok(fs) = open_source(&conn) else {
            continue;
        };
        let mut n: u64 = 0;
        let walked = fs.walk(&mut |entry| {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            if let Ok(fe) = entry {
                if dam_media::detect(Path::new(&fe.rel_path)).is_some() {
                    n += 1;
                }
            }
            true
        });
        if walked.is_ok() {
            total += n;
            counted_any = true;
        }
    }
    (counted_any && total > 0).then_some(total)
}

fn hash_file(path: &Path) -> Option<dam_api::id::ContentHash> {
    let mut hasher = blake3::Hasher::new();
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    std::io::copy(&mut reader, &mut hasher).ok()?;
    Some(dam_api::id::ContentHash(*hasher.finalize().as_bytes()))
}

/// The final path component of a source-relative path (works for `/`-separated remote paths too).
fn file_name(rel_path: &str) -> String {
    rel_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(rel_path)
        .to_string()
}
