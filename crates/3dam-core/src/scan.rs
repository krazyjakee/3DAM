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
const MAX_JOB_WARNING_DETAILS: usize = 20;

fn record_warning(total: &mut u64, details: &mut Vec<String>, message: String) {
    *total += 1;
    if details.len() < MAX_JOB_WARNING_DETAILS {
        details.push(message);
    }
}

#[allow(clippy::too_many_arguments)] // the job runner's full context; a struct would just rename it
pub(crate) fn run_scan(
    store: Arc<Store>,
    secrets: crate::credentials::SecretVault,
    events: broadcast::Sender<LibraryEvent>,
    job: JobId,
    sources: Vec<SourceInfo>,
    mode: ScanMode,
    cancel: Arc<AtomicBool>,
    governor: &crate::resources::Governor,
    scratch: &Path,
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
        ScanMode::Full => count_total(&store, &secrets, &sources, &cancel, scratch),
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
    let mut warning_details = Vec::new();
    let mut removed_total: u64 = 0;

    for src in sources {
        if src.kind == SourceKind::Federated {
            continue; // federated peers yield catalog rows, not bytes — not scanned here (phase 6)
        }
        let sid = src.id;
        let source_label = src.name.clone();

        // Rebuild from the persisted non-secret connection + its resolved host credential. An
        // unreachable host, locked store, or bad credential marks this source offline and moves on.
        let conn = match store
            .get_source_connection(&sid)
            .and_then(|connection| secrets.resolve(connection))
        {
            Ok(c) => c,
            Err(e) => {
                let _ = store.set_source_error(&sid, &e.to_string());
                record_warning(
                    &mut warnings,
                    &mut warning_details,
                    format!("Source “{source_label}” could not be opened; inspect its connection settings"),
                );
                continue;
            }
        };
        let fs = match open_source(&conn, scratch) {
            Ok(fs) => fs,
            Err(e) => {
                let _ = store.set_source_error(&sid, &e.to_string());
                tracing::warn!(source = %sid, error = %e, "source unavailable");
                record_warning(
                    &mut warnings,
                    &mut warning_details,
                    format!("Source “{source_label}” is unavailable; inspect source status and credentials"),
                );
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
                    let Some(det) = dam_media::detect_for_ingest(Path::new(&fe.rel_path)) else {
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
                    // Materialise bytes locally from the backend's pinned/opened source handle.
                    let fetched = match fs.fetch(&fe.rel_path) {
                        Ok(f) => f,
                        Err(e) => {
                            record_warning(
                                &mut warnings,
                                &mut warning_details,
                                format!(
                                    "“{}” could not be read from source “{source_label}”",
                                    fe.rel_path
                                ),
                            );
                            tracing::warn!(path = %fe.rel_path, error = %e, "fetch failed");
                            return true;
                        }
                    };
                    let abs = fetched.path();
                    // Now that real bytes exist locally, settle the container extensions whose
                    // media type the path alone can't determine (`.mp4`/`.mov`/`.m4v` — audio-only
                    // or video?). This is the only point in the scan where that question is
                    // answerable, and it's asked once per asset, before the row is written.
                    let det = dam_media::refine_with_content(&det, abs).unwrap_or(det);
                    let hash = match hash_file(abs) {
                        Some(h) => Some(h),
                        None => {
                            record_warning(
                                &mut warnings,
                                &mut warning_details,
                                format!("“{}” could not be hashed; check file readability", fe.rel_path),
                            );
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
                                record_warning(
                                    &mut warnings,
                                    &mut warning_details,
                                    format!(
                                        "“{}” was catalogued but its media metadata could not be saved",
                                        fe.rel_path
                                    ),
                                );
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
                                    // Attribution for the ceiling check on the way out to
                                    // subscribers (issue #42).
                                    source_id: Some(sid),
                                };
                                let _ = events.send(LibraryEvent::AssetAdded(summary));
                            }
                        }
                        Err(e) => {
                            record_warning(
                                &mut warnings,
                                &mut warning_details,
                                format!("“{}” could not be added to the catalog", fe.rel_path),
                            );
                            tracing::warn!(path = %fe.rel_path, error = %e, "skipped asset");
                        }
                    }
                    true
                }
                Err(e) => {
                    record_warning(
                        &mut warnings,
                        &mut warning_details,
                        format!("An entry in source “{source_label}” could not be listed"),
                    );
                    tracing::warn!(source = %sid, error = %e, "source entry unavailable");
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
                        Err(e) => {
                            record_warning(
                                &mut warnings,
                                &mut warning_details,
                                format!(
                                    "Source “{source_label}” was scanned but missing-file status could not be updated"
                                ),
                            );
                            tracing::warn!(source = %sid, error = %e, "mark-missing failed");
                        }
                    }
                }
                let _ = store.set_source_scanned(&sid, dam_store::now_ms());
            }
            Err(e) => {
                let _ = store.set_source_error(&sid, &e.to_string());
                record_warning(
                    &mut warnings,
                    &mut warning_details,
                    format!("Source “{source_label}” could not be fully walked; inspect source status"),
                );
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
        let omitted = warnings.saturating_sub(warning_details.len() as u64);
        if omitted > 0 {
            warning_details.push(format!(
                "{omitted} additional warning(s) omitted; inspect source status and server logs"
            ));
        }
        let suffix = (!notes.is_empty())
            .then(|| format!(" ({})", notes.join(", ")))
            .unwrap_or_default();
        let summary = format!("Scanned {examined} item(s){suffix}");
        let _ = store.complete_job(&job, &summary, &warning_details);
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
pub(crate) fn key_attrs_of(attrs: &MediaAttributes) -> SmallMap {
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
        MediaAttributes::Video(v) => {
            if let (Some(w), Some(h)) = (v.width, v.height) {
                m.insert("dimensions".into(), format!("{w}×{h}"));
            }
            if let Some(ms) = v.duration_ms {
                let secs = ms as f64 / 1000.0;
                m.insert(
                    "duration".into(),
                    format!("{:.0}:{:02}", (secs / 60.0).floor(), (secs % 60.0) as i64),
                );
            }
        }
        MediaAttributes::Document(d) => {
            if let Some(p) = d.page_count.filter(|p| *p > 0) {
                m.insert("pages".into(), p.to_string());
            }
            if let Some(w) = d.word_count.filter(|w| *w > 0) {
                m.insert("words".into(), w.to_string());
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
fn count_total(
    store: &Store,
    secrets: &crate::credentials::SecretVault,
    sources: &[SourceInfo],
    cancel: &AtomicBool,
    scratch: &Path,
) -> Option<u64> {
    let mut total: u64 = 0;
    let mut counted_any = false;
    for src in sources {
        if src.kind == SourceKind::Federated {
            continue; // federated peers yield catalog rows, not bytes — not scanned here (phase 6)
        }
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(conn) = store
            .get_source_connection(&src.id)
            .and_then(|connection| secrets.resolve(connection))
        else {
            continue;
        };
        let Ok(fs) = open_source(&conn, scratch) else {
            continue;
        };
        let mut n: u64 = 0;
        let walked = fs.walk(&mut |entry| {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            if let Ok(fe) = entry {
                if dam_media::detect_for_ingest(Path::new(&fe.rel_path)).is_some() {
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

pub(crate) fn hash_file(path: &Path) -> Option<dam_api::id::ContentHash> {
    let mut hasher = blake3::Hasher::new();
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    std::io::copy(&mut reader, &mut hasher).ok()?;
    Some(dam_api::id::ContentHash(*hasher.finalize().as_bytes()))
}

/// The final path component of a source-relative path (works for `/`-separated remote paths too).
pub(crate) fn file_name(rel_path: &str) -> String {
    rel_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(rel_path)
        .to_string()
}
