//! Writing a file *into* a source, and cataloguing it (issue #80, tech-spec 08 §5.1).
//!
//! This is the only module in 3DAM that creates a file inside a registered source tree. It is
//! deliberately **not** built on the convert pipeline: convert's §5.1 guard stays absolute and
//! never grows an "unless this is an upload" clause, and the two paths share no code, so the
//! non-destructive invariant holds structurally rather than by remembering to check a flag.
//!
//! Three things carry that invariant here:
//!
//! 1. **Collision handling is retry, not probe-then-write.** [`FileSource::put`] is create-only and
//!    answers [`LibError::Conflict`] when the name is taken, so `Suffix` simply asks for the next
//!    name and `Skip` stops. There is no `exists()` check whose result could go stale before the
//!    write — the filesystem's own atomic create is the arbiter, so two concurrent uploads of
//!    `brick.png` produce `brick.png` and `brick-1.png` rather than one silently eating the other.
//! 2. **Ingest reuses `detect_for_ingest`, exactly as a scan does.** Uploading must not mint a row
//!    that a later scan would decline to create — otherwise the next rescan would delete the
//!    asset the user just uploaded.
//! 3. **The blocklist still applies.** Bytes the user removed-and-blocked stay out of the catalog,
//!    so upload cannot become a way to smuggle a blocked hash back in. The *file* is still written:
//!    the user asked for it on disk, and only the catalog row is refused.

use dam_api::dto::*;
use dam_api::event::LibraryEvent;
use dam_api::id::{AssetId, SourceId};
use dam_api::LibError;
use dam_sources::{safe_name, FileSource, SourceConnection};
use dam_store::{NewAsset, Store};
use std::path::Path;
use tokio::sync::broadcast;

/// How many suffixed names to try before giving up, matching convert's `resolve_collision`.
const MAX_SUFFIX_ATTEMPTS: u32 = 10_000;

/// Write one staged file into a source and catalogue it.
///
/// `staged` holds the bytes already — the transport streamed them to scratch — so nothing here
/// buffers the asset in memory, and a failed write costs a temp file rather than a partial asset.
pub(crate) fn run_upload(
    store: &Store,
    secrets: &crate::credentials::SecretVault,
    events: &broadcast::Sender<LibraryEvent>,
    req: UploadRequest,
    staged: &Path,
    scratch: &Path,
) -> Result<UploadOutcome, LibError> {
    let conn = secrets.resolve(store.get_source_connection(&req.source)?)?;

    // A peer is refused here as well as in the destination picker. The picker keeps it out of the
    // UI; this keeps it out of the *API*, so a hand-written request cannot try to push bytes into
    // someone else's library. `open_source` would reject a federated connection anyway, but with a
    // message about catalog rows that would read as a bug rather than a decision.
    if matches!(conn, SourceConnection::Federated(_)) {
        return Err(LibError::Forbidden(
            "a federated peer's library is read-only; upload into a local source instead".into(),
        ));
    }

    let fs = dam_sources::open_source(&conn, scratch)?;
    if !fs.writable() {
        return Err(LibError::Forbidden(format!(
            "source {} is not writable",
            req.source
        )));
    }

    let rel_dir = normalize_folder(&req.folder)?;
    // The filename must be exactly one path component. Without this, a `name` of `../../etc/passwd`
    // would be checked as a *path* and could walk out of the chosen destination folder even though
    // it stays inside the source root — the folder the user picked is part of the promise.
    safe_name::check_component(&req.name)?;

    // No explicit `mkdir` here: `put` creates the parent chain itself, and only *after* it has
    // established the name is free. Creating it up front would leave a directory tree behind in the
    // user's source every time the upload was then refused — a typo'd folder name, a collision
    // under `Fail`, or a `Skip` that writes nothing would each litter the tree — which would make
    // this path mutate a source on its failure path. `mkdir` stays on the seam for the create-folder
    // affordance, where making a directory *is* the request.

    // One stat, reused for the outcome, the catalog row, and the event. Taking it three times
    // against a path this process does not exclusively own could report three different sizes for
    // one upload.
    let meta =
        std::fs::metadata(staged).map_err(|e| LibError::Internal(format!("staged upload: {e}")))?;
    let size = meta.len();
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);

    let Some(written) =
        put_resolving_collisions(fs.as_ref(), &rel_dir, &req.name, staged, req.collision)?
    else {
        return Ok(UploadOutcome {
            path: join_rel(&rel_dir, &req.name),
            skipped: true,
            size,
            asset: None,
            uncatalogued_reason: None,
        });
    };

    // The bytes are committed. From here nothing may fail the upload: the file exists in the
    // user's source whatever the catalog decides, and reporting a failure would invite a retry
    // that then collides with the file we just wrote.
    //
    // Catalogue from the file at its *destination*, not from `staged`. Metadata extraction resolves
    // a model's external references relative to the file's own directory (`scene.gltf` → its `.bin`
    // and textures), so reading the scratch copy would look for those siblings in the scratch dir,
    // find none, and record a `dependency_bytes` — and an `AssetAdded` size — that under-reports
    // the asset by its whole texture set. `fetch` is the seam's own answer for "give me these bytes
    // locally" and pins local reads before handing path-based media code a private copy.
    let landed = fs.fetch(&written);
    let read_from = match &landed {
        Ok(f) => f.path(),
        // Fail-soft: if the just-written file cannot be re-opened, catalogue from the staged copy
        // rather than losing the row entirely. Only sibling-relative extraction degrades.
        Err(e) => {
            tracing::warn!(path = %written, error = %e, "re-reading the uploaded file failed");
            staged
        }
    };
    let (asset, uncatalogued_reason) = ingest_one(
        store,
        events,
        &req.source,
        &written,
        read_from,
        size,
        modified_ms,
    );

    Ok(UploadOutcome {
        path: written,
        skipped: false,
        size,
        asset,
        uncatalogued_reason,
    })
}

/// Validate the destination folder and return it in `a/b` form (empty means the source root).
///
/// Only *trailing* slashes are trimmed. Stripping a leading one too would quietly turn `/etc` into
/// the relative `etc` and accept it — reinterpreting a path the caller wrote as absolute instead of
/// refusing it, which is the sanitise-don't-reject mistake the name battery exists to avoid. A bare
/// `/` is still the root, since that is the one absolute spelling with an unambiguous meaning here.
fn normalize_folder(folder: &str) -> Result<String, LibError> {
    let trimmed = folder.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    safe_name::check_rel_path(trimmed)
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Try to create the file, resolving a taken name per the request's rule.
///
/// `Ok(None)` means `Skip` found the name taken and left the existing file alone. Every attempt
/// re-opens `staged` because a create-only `put` consumes the reader on the attempt that fails.
fn put_resolving_collisions(
    fs: &dyn FileSource,
    dir: &str,
    name: &str,
    staged: &Path,
    rule: UploadCollision,
) -> Result<Option<String>, LibError> {
    let first = join_rel(dir, name);
    match put_once(fs, &first, staged) {
        Ok(()) => return Ok(Some(first)),
        // Only a *taken name* is recoverable. A malformed name or an I/O failure propagates, so a
        // `Suffix` upload cannot spin through ten thousand names that were never going to work.
        Err(LibError::Conflict(_)) => {}
        Err(e) => return Err(e),
    }

    match rule {
        UploadCollision::Fail => Err(LibError::Conflict(format!(
            "{first} already exists; upload never replaces an existing file"
        ))),
        UploadCollision::Skip => Ok(None),
        UploadCollision::Suffix => {
            for n in 1..MAX_SUFFIX_ATTEMPTS {
                // `safe_name::suffixed`, not a local spelling of the same rule: it exists so the
                // disambiguated name is identical whichever backend resolves it, and a second copy
                // here would let local and (slice 7) remote uploads drift apart.
                let rel = join_rel(dir, &safe_name::suffixed(name, n));
                match put_once(fs, &rel, staged) {
                    Ok(()) => return Ok(Some(rel)),
                    Err(LibError::Conflict(_)) => continue,
                    // A suffixed candidate can outgrow the name/path ceilings even though the
                    // original fit (`{stem}-9999.png` is 5 bytes longer). That is a rejection the
                    // *suffixing* caused, not something the user can act on, so it ends the search
                    // as "no free suffix" rather than surfacing a byte count for a name they never
                    // typed.
                    Err(LibError::BadRequest(_)) => break,
                    Err(e) => return Err(e),
                }
            }
            Err(LibError::Conflict(format!(
                "{first} already exists and no free suffix was found"
            )))
        }
    }
}

fn put_once(fs: &dyn FileSource, rel: &str, staged: &Path) -> Result<(), LibError> {
    let mut f = std::fs::File::open(staged)
        .map_err(|e| LibError::Internal(format!("staged bytes: {e}")))?;
    fs.put(rel, &mut f)
}

/// Catalogue one just-written file. Returns the new asset id, or the reason there isn't one.
///
/// Idempotent by `(source_id, path)` through `upsert_asset`, so a watch event racing this call for
/// the same path updates the row rather than duplicating it.
fn ingest_one(
    store: &Store,
    events: &broadcast::Sender<LibraryEvent>,
    source: &SourceId,
    rel_path: &str,
    bytes: &Path,
    size: u64,
    modified_ms: Option<i64>,
) -> (Option<AssetId>, Option<String>) {
    // `detect_for_ingest`, not `detect`: it answers "should the catalog hold this?", and using the
    // looser check here would create rows the next scan of the same tree would decline to make.
    let Some(det) = dam_media::detect_for_ingest(Path::new(rel_path)) else {
        // The two ways that can answer `None` need different explanations. `detect` succeeding
        // where `detect_for_ingest` declines means the format *is* supported and the file was
        // dropped for *where* it is — a document under a build/dependency directory. Reporting
        // "unsupported format" for a PDF would send the user hunting for a format problem that
        // does not exist, when the fix is the destination folder.
        let reason = if dam_media::detect(Path::new(rel_path)).is_some() {
            "stored, but not catalogued: 3DAM does not catalogue documents inside \
             build/dependency folders"
        } else {
            "stored, but not catalogued: unsupported format"
        };
        return (None, Some(reason.into()));
    };
    let det = dam_media::refine_with_content(&det, bytes).unwrap_or(det);

    let hash = crate::scan::hash_file(bytes);
    if let Some(h) = &hash {
        if store.is_blocked(h).unwrap_or(false) {
            return (
                None,
                Some("stored, but not catalogued: these contents are on the blocklist".into()),
            );
        }
    }

    let na = NewAsset {
        source_id: *source,
        path: rel_path.to_string(),
        filename: crate::scan::file_name(rel_path),
        content_hash: hash,
        size_bytes: Some(size as i64),
        // Carried through rather than left `None`, which `scan::unchanged` reads as "cannot tell,
        // assume changed" — so every uploaded asset would be re-fetched and fully re-hashed by the
        // next delta scan of its source. On a 2 GB upload that is a gigabyte-scale re-read of a
        // file 3DAM itself just wrote.
        source_modified_at: modified_ms,
        scanned_at: dam_store::now_ms(),
        media_type: det.media,
        format: det.format.clone(),
    };

    match store.upsert_asset(&na) {
        Ok((id, inserted)) => {
            let attrs = dam_media::extract_metadata(bytes, &det);
            if let Err(e) = store.set_media_attrs(&id, &attrs) {
                tracing::warn!(path = %rel_path, error = %e, "attr persist failed");
            }
            if inserted {
                let dep = match &attrs {
                    MediaAttributes::Model(m) => m.dependency_bytes.unwrap_or(0).max(0) as u64,
                    _ => 0,
                };
                let _ = events.send(LibraryEvent::AssetAdded(AssetSummary {
                    id,
                    name: na.filename.clone(),
                    media: det.media,
                    format: det.format.clone(),
                    size: size + dep,
                    license: LicenseBadge::default(),
                    top_tags: Vec::new(),
                    origin: Origin::Local,
                    key_attrs: crate::scan::key_attrs_of(&attrs),
                    favorite: false,
                    source_id: Some(*source),
                }));
            }
            (Some(id), None)
        }
        Err(e) => {
            tracing::warn!(path = %rel_path, error = %e, "upload ingest failed");
            (None, Some(format!("stored, but not catalogued: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// You cannot upload into a peer's library. Asserted here rather than in the integration tests
    /// because registering a federated source through `add_source` probes the peer for real, so a
    /// fake endpoint cannot be registered — the store, however, will hold a row of any kind.
    ///
    /// The refusal is explicit rather than left to `open_source`, which also rejects a federated
    /// connection but with a message about catalog rows that would read to a user as a bug.
    #[test]
    fn a_federated_source_refuses_uploads() {
        let store = Store::open_in_memory().unwrap();
        let sid = store
            .add_source(
                &SourceConnection::Federated(dam_sources::FederatedConfig {
                    endpoint: "http://127.0.0.1:9".into(),
                    token: None,
                    credential_ref: None,
                }),
                "peer",
                false,
            )
            .unwrap();
        let (events, _rx) = broadcast::channel(8);
        // Deliberately a path that does not exist: the refusal must come before anything touches
        // the staged bytes, so a peer upload costs nothing and cannot half-happen.
        let staged = Path::new("/nonexistent/staged.bin");

        let err = run_upload(
            &store,
            &crate::credentials::SecretVault::memory(),
            &events,
            UploadRequest {
                source: sid,
                folder: String::new(),
                name: "brick.png".into(),
                collision: UploadCollision::Fail,
            },
            staged,
            Path::new("/tmp"),
        )
        .unwrap_err();
        assert!(matches!(err, LibError::Forbidden(_)), "got {err:?}");
    }

    #[test]
    fn normalize_folder_accepts_root_and_rejects_traversal() {
        assert_eq!(normalize_folder("").unwrap(), "");
        assert_eq!(normalize_folder("/").unwrap(), "");
        assert_eq!(
            normalize_folder("Textures/brick").unwrap(),
            "Textures/brick"
        );
        assert_eq!(normalize_folder("Textures/").unwrap(), "Textures");
        assert!(normalize_folder("../escape").is_err());
        // Rejected, *not* quietly re-read as the relative `etc`.
        assert!(normalize_folder("/etc").is_err());
        assert!(normalize_folder("//etc").is_err());
    }
}
