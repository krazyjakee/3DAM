//! The convert pipeline orchestration (tech-spec 08). Runs a plan over the embedded store: resolve
//! each input, plan its output path under the user's `output_dir`, and (on commit) decode + encode
//! via `dam-media` and write **atomically**. Two hard invariants hold regardless of flags:
//!
//! - **Source-safety (§5.1):** an output path inside a registered source tree is always rejected —
//!   3DAM never writes over a catalogued original.
//! - **Fail-soft (§1.1):** one unreadable/unencodable input fails its own item; the batch continues.
//!
//! CLI-first in v1: this returns the full `ConvertReport` synchronously (run on the blocking pool by
//! the caller). The job/progress-streamed model layers on later without changing these types.

use dam_api::dto::*;
use dam_api::id::{AssetId, SourceId};
use dam_api::LibError;
use dam_store::Store;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Run a convert plan against the store. `dry_run` writes nothing.
pub(crate) fn run_convert(
    store: &Store,
    secrets: &crate::credentials::SecretVault,
    req: ConvertRequest,
    scratch: &Path,
) -> Result<ConvertReport, LibError> {
    if req.output_dir.trim().is_empty() {
        return Err(LibError::BadRequest("output_dir is required".into()));
    }
    let output_dir = PathBuf::from(&req.output_dir);

    // Source-safety (§5.1): outputs may not land inside any registered source tree. All per-item
    // paths live under `output_dir`, so validating the root once is sufficient and cheap.
    let source_roots = source_roots(store)?;
    let out_check = canonical_or_self(&output_dir);
    if source_roots.iter().any(|r| path_within(&out_check, r)) {
        return Err(LibError::BadRequest(format!(
            "output_dir {} lies inside a registered source; convert is non-destructive and refuses to write there",
            output_dir.display()
        )));
    }

    let target_media = req.target.media();
    let ext = output_ext(&req.target);
    let mut report = ConvertReport {
        dry_run: req.dry_run,
        output_dir: output_dir.to_string_lossy().into_owned(),
        ..Default::default()
    };

    // One backend per distinct source across the batch, opened on first use (issue #48). Converting
    // ten files off one SFTP host must not mean ten SSH handshakes.
    let mut backends: Backends = HashMap::new();

    for input in &req.inputs {
        let item = plan_and_maybe_encode(
            store,
            secrets,
            *input,
            &req,
            &output_dir,
            target_media,
            &ext,
            &mut backends,
            scratch,
        );
        tally(&mut report, &item);
        report.items.push(item);
    }
    Ok(report)
}

/// Lazily-opened `FileSource` per source id, including the failure — a host that is down should
/// report the same reason on every item from it, not be retried once per file.
type Backends = HashMap<SourceId, Result<Arc<dyn dam_sources::FileSource>, String>>;

/// Rebuild (or recall) the backend for one source.
fn backend_for<'a>(
    store: &Store,
    secrets: &crate::credentials::SecretVault,
    backends: &'a mut Backends,
    source_id: &SourceId,
    scratch: &Path,
) -> &'a Result<Arc<dyn dam_sources::FileSource>, String> {
    backends.entry(*source_id).or_insert_with(|| {
        let opened = store
            .get_source_connection(source_id)
            .and_then(|connection| secrets.resolve(connection))
            .and_then(|c| dam_sources::open_source(&c, scratch));
        match opened {
            Ok(fs) => Ok(Arc::from(fs)),
            Err(e) => {
                let msg = e.to_string();
                // Surface the offline state where a user will see it, not only in the log.
                let _ = store.set_source_error(source_id, &msg);
                Err(msg)
            }
        }
    })
}

#[allow(clippy::too_many_arguments)] // the per-item plan context; a struct would just rename it
fn plan_and_maybe_encode(
    store: &Store,
    secrets: &crate::credentials::SecretVault,
    input: AssetId,
    req: &ConvertRequest,
    output_dir: &Path,
    target_media: MediaType,
    ext: &str,
    backends: &mut Backends,
    scratch: &Path,
) -> ConvertItemReport {
    // Resolve the asset + its on-disk source path.
    let asset = match store.get_asset(&input) {
        Ok(a) => a,
        Err(e) => return failed_item(input, String::new(), String::new(), e.to_string()),
    };
    let input_bytes = asset.summary.size;
    let src_root = match store.get_source(&asset.source_id) {
        Ok(Some(s)) => s.uri,
        Ok(None) => {
            return failed_item(
                input,
                asset.path.clone(),
                String::new(),
                "source missing".into(),
            )
        }
        Err(e) => return failed_item(input, asset.path.clone(), String::new(), e.to_string()),
    };
    // The asset's *logical* location, used for every reported `input_path`. For a remote asset the
    // bytes are read from a temp file with a random name, which would be meaningless (and alarming)
    // in a report — what the user converted is `sftp://host/root/path`, and that is what is shown.
    let abs_input = PathBuf::from(&src_root).join(&asset.path);

    // Media-type mismatch → unsupported (fail-soft, not a batch abort — §4.1).
    if asset.summary.media != target_media {
        return ConvertItemReport {
            input,
            input_path: abs_input.to_string_lossy().into_owned(),
            planned_output: String::new(),
            disposition: Disposition::Unsupported,
            input_bytes,
            output_bytes: None,
            ratio: None,
            error: Some(format!(
                "asset is {}, target is {}",
                asset.summary.media.as_str(),
                target_media.as_str()
            )),
        };
    }

    let stem = Path::new(&asset.summary.name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    let base_output = output_dir.join(format!("{stem}.{ext}"));

    // Resolve collisions per the rule (§5.3).
    let (planned, skip) = match resolve_collision(&base_output, req.on_collision) {
        CollisionOutcome::Path(p) => (p, false),
        CollisionOutcome::Skip => (base_output.clone(), true),
        CollisionOutcome::Fail => {
            return ConvertItemReport {
                input,
                input_path: abs_input.to_string_lossy().into_owned(),
                planned_output: base_output.to_string_lossy().into_owned(),
                disposition: Disposition::Collision,
                input_bytes,
                output_bytes: None,
                ratio: None,
                error: Some(
                    "output already exists (use --on-collision suffix|skip|overwrite)".into(),
                ),
            };
        }
    };
    let planned_str = planned.to_string_lossy().into_owned();

    if skip {
        return ConvertItemReport {
            input,
            input_path: abs_input.to_string_lossy().into_owned(),
            planned_output: planned_str,
            disposition: Disposition::Skipped,
            input_bytes,
            output_bytes: None,
            ratio: None,
            error: None,
        };
    }

    if req.dry_run {
        return ConvertItemReport {
            input,
            input_path: abs_input.to_string_lossy().into_owned(),
            planned_output: planned_str,
            disposition: Disposition::Write,
            input_bytes,
            output_bytes: None,
            ratio: None,
            error: None,
        };
    }

    // Commit. Materialise the input locally first (issue #48): in place for a local source, a temp
    // download for SFTP/SMB. Non-destructive either way — `fetch` only ever reads, and the output
    // still goes to `output_dir` via temp→atomic-rename, never back to the source (§5.1).
    let fetched = match backend_for(store, secrets, backends, &asset.source_id, scratch) {
        Ok(fs) => match fs.fetch(&asset.path) {
            Ok(f) => f,
            Err(e) => {
                return failed_item(
                    input,
                    abs_input.to_string_lossy().into_owned(),
                    planned_str,
                    e.to_string(),
                )
            }
        },
        Err(e) => {
            return failed_item(
                input,
                abs_input.to_string_lossy().into_owned(),
                planned_str,
                e.clone(),
            )
        }
    };

    // Encode into memory, then write atomically under output_dir. The temp download (if any) lives
    // exactly as long as this item — one asset's worth of scratch, whatever the batch size.
    match encode(fetched.path(), &asset.summary.format, &req.target) {
        Ok(bytes) => match atomic_write(&planned, &bytes) {
            Ok(()) => {
                let out_len = bytes.len() as u64;
                ConvertItemReport {
                    input,
                    input_path: abs_input.to_string_lossy().into_owned(),
                    planned_output: planned_str,
                    disposition: Disposition::Done,
                    input_bytes,
                    output_bytes: Some(out_len),
                    ratio: (input_bytes > 0).then(|| out_len as f32 / input_bytes as f32),
                    error: None,
                }
            }
            Err(e) => failed_item(
                input,
                abs_input.to_string_lossy().into_owned(),
                planned_str,
                e,
            ),
        },
        Err(e) => failed_item(
            input,
            abs_input.to_string_lossy().into_owned(),
            planned_str,
            e.to_string(),
        ),
    }
}

/// Dispatch to the media encoder (tech-spec 08 §2 — convert reuses `dam-media` decode/encode).
fn encode(
    abs_input: &Path,
    source_format: &str,
    target: &ConvertTarget,
) -> Result<Vec<u8>, dam_media::HandlerError> {
    match target {
        ConvertTarget::Image {
            format,
            max_edge,
            quality,
        } => dam_media::convert_image(abs_input, format, *max_edge, *quality),
        ConvertTarget::Audio { format } => {
            dam_media::convert_audio(abs_input, source_format, format)
        }
        // 3D container transcode (issue #49). `source_format` is deliberately unused: Assimp
        // identifies the input from its own contents, and trusting the catalogued extension over
        // the file's signature would be the wrong call for a family where mislabelled extensions
        // are common.
        ConvertTarget::Model { format, optimize } => {
            dam_media::convert_model(abs_input, format, *optimize)
        }
    }
}

fn output_ext(target: &ConvertTarget) -> String {
    match target.format() {
        "jpeg" => "jpg".into(),
        other => other.into(),
    }
}

enum CollisionOutcome {
    Path(PathBuf),
    Skip,
    Fail,
}

fn resolve_collision(base: &Path, rule: CollisionRule) -> CollisionOutcome {
    if !base.exists() {
        return CollisionOutcome::Path(base.to_path_buf());
    }
    match rule {
        CollisionRule::Fail => CollisionOutcome::Fail,
        CollisionRule::Skip => CollisionOutcome::Skip,
        CollisionRule::Overwrite => CollisionOutcome::Path(base.to_path_buf()),
        CollisionRule::Suffix => {
            let stem = base
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = base
                .extension()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let dir = base.parent().unwrap_or_else(|| Path::new("."));
            for n in 1..10_000 {
                let candidate = dir.join(format!("{stem}-{n}.{ext}"));
                if !candidate.exists() {
                    return CollisionOutcome::Path(candidate);
                }
            }
            CollisionOutcome::Fail
        }
    }
}

/// Write to a temp file in the destination dir, then rename into place (§5.2). A crash leaves the
/// temp file, never a half-written output at the real path — so commit is safe to retry.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    let tmp = dir.join(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, bytes).map_err(|e| format!("write temp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename into place: {e}")
    })?;
    Ok(())
}

fn failed_item(
    input: AssetId,
    input_path: String,
    planned_output: String,
    error: String,
) -> ConvertItemReport {
    ConvertItemReport {
        input,
        input_path,
        planned_output,
        disposition: Disposition::Failed,
        input_bytes: 0,
        output_bytes: None,
        ratio: None,
        error: Some(error),
    }
}

fn tally(report: &mut ConvertReport, item: &ConvertItemReport) {
    report.total_input_bytes += item.input_bytes;
    if let Some(b) = item.output_bytes {
        report.total_output_bytes += b;
    }
    match item.disposition {
        Disposition::Done => report.done += 1,
        Disposition::Failed => report.failed += 1,
        Disposition::Collision => report.collisions += 1,
        Disposition::Unsupported => report.unsupported += 1,
        Disposition::Write | Disposition::Skipped => {}
    }
}

// ── path helpers ─────────────────────────────────────────────────────────────

fn source_roots(store: &Store) -> Result<Vec<PathBuf>, LibError> {
    Ok(store
        .list_sources()?
        .into_iter()
        .filter(|s| s.kind == SourceKind::LocalFs)
        .map(|s| canonical_or_self(Path::new(&s.uri)))
        .collect())
}

fn canonical_or_self(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| normalise(p))
}

/// Lexically normalise a path (drop `.`, resolve `..`) when it cannot be canonicalised (e.g. it
/// does not exist yet). Enough to make the within-source check meaningful for a fresh output dir.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True if `path` is equal to or nested under `root`.
fn path_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}
