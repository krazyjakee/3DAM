//! Incremental manifest export. Selection and hydration are delegated to `dam-store` so SQLite can
//! produce export-only rows in bounded, set-based batches; encoders write those batches directly to
//! an atomic staging target rather than retaining a whole catalog in memory.

use dam_api::dto::*;
use dam_api::service::Visibility;
use dam_api::LibError;
use dam_store::{ExportAssetRow, ExportSelection, Store};
use serde::Serialize;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// The bound covers hydrated rows and all encoder-side transient allocations. Explicit ids remain
/// part of `ExportRequest` for wire compatibility, but no additional catalog-sized vectors are made.
const EXPORT_BATCH_SIZE: usize = 256;

#[derive(Serialize)]
struct ManifestRow<'a> {
    id: String,
    name: &'a str,
    path: &'a str,
    media: String,
    format: &'a str,
    size_bytes: u64,
    hash: Option<String>,
    license_id: &'a Option<String>,
    license_status: String,
    commercial: Option<bool>,
    modify: Option<bool>,
    redistribute: Option<bool>,
    attribution: Option<bool>,
    attribution_holder: &'a Option<String>,
    attribution_credit: &'a Option<String>,
    license_url: &'a Option<String>,
    tags: &'a str,
    note: &'a str,
}

#[derive(Serialize)]
struct CreditRow<'a> {
    id: String,
    name: &'a str,
    license_id: &'a Option<String>,
    license_status: String,
    holder: &'a Option<String>,
    credit: &'a Option<String>,
    url: &'a Option<String>,
}

pub(crate) fn run_export(
    store: &Store,
    req: ExportRequest,
    vis: &Visibility,
) -> Result<ExportReport, LibError> {
    run_export_with_checkpoint(store, req, vis, |_| Ok(()))
}

/// The callback is the progress/cancellation seam for issue #114's background-job wrapper. It is
/// called before work and after every bounded batch; returning `Cancelled` (or any error) drops the
/// staging target, leaving a previous successful export untouched.
pub(crate) fn run_export_with_checkpoint(
    store: &Store,
    mut req: ExportRequest,
    vis: &Visibility,
    mut checkpoint: impl FnMut(u64) -> Result<(), LibError>,
) -> Result<ExportReport, LibError> {
    let selection = resolve_selection(store, &mut req, vis)?;
    checkpoint(0)?;
    let (assets, files_written) = match req.format {
        ExportFormat::Json => emit_json(store, &selection, &req, vis, &mut checkpoint)?,
        ExportFormat::Csv => emit_csv(store, &selection, &req, vis, &mut checkpoint)?,
        ExportFormat::Sidecar => emit_sidecars(store, &selection, &req, vis, &mut checkpoint)?,
    };
    Ok(ExportReport {
        format: req.format,
        output: req.output,
        assets,
        files_written,
    })
}

/// Resolve only the selector shape. No ids are enumerated here: query and collection membership
/// remain SQL predicates all the way into `stream_export_rows`.
fn resolve_selection(
    store: &Store,
    req: &mut ExportRequest,
    vis: &Visibility,
) -> Result<ExportSelection, LibError> {
    if !req.assets.is_empty() {
        return Ok(ExportSelection::Assets(std::mem::take(&mut req.assets)));
    }
    if let Some(collection) = req.collection {
        if !store.collection_visible(&collection, vis)? {
            return Err(LibError::NotFound(format!("collection {collection}")));
        }
        let record = store.get_collection(&collection, vis)?;
        return Ok(match record.kind {
            CollectionKind::Manual => ExportSelection::ManualCollection(collection),
            CollectionKind::Smart => ExportSelection::Query(record.query.unwrap_or_default()),
        });
    }
    Ok(ExportSelection::Query(req.query.take().unwrap_or_default()))
}

fn emit_json(
    store: &Store,
    selection: &ExportSelection,
    req: &ExportRequest,
    vis: &Visibility,
    checkpoint: &mut impl FnMut(u64) -> Result<(), LibError>,
) -> Result<(u64, u64), LibError> {
    let out = Path::new(&req.output);
    let temp = atomic_file(out)?;
    let mut writer = BufWriter::new(temp);
    writer.write_all(b"{\"assets\":[").map_err(io_err)?;
    let mut first = true;
    let mut processed = 0u64;
    let mut emitted = 0u64;
    store.stream_export_rows(selection, vis, EXPORT_BATCH_SIZE, |batch| {
        for asset in batch {
            if req.attribution_only && !needs_attribution(asset) {
                continue;
            }
            if !first {
                writer.write_all(b",").map_err(io_err)?;
            }
            first = false;
            if req.attribution_only {
                serde_json::to_writer(&mut writer, &credit_row(asset))
            } else {
                serde_json::to_writer(&mut writer, &manifest_row(asset))
            }
            .map_err(|e| LibError::Internal(format!("write json: {e}")))?;
            emitted += 1;
        }
        processed += batch.len() as u64;
        checkpoint(processed)
    })?;
    writer.write_all(b"]}").map_err(io_err)?;
    writer.flush().map_err(io_err)?;
    let temp = writer.into_inner().map_err(|e| io_err(e.into_error()))?;
    persist_file(temp, out)?;
    Ok((emitted, 1))
}

fn emit_csv(
    store: &Store,
    selection: &ExportSelection,
    req: &ExportRequest,
    vis: &Visibility,
    checkpoint: &mut impl FnMut(u64) -> Result<(), LibError>,
) -> Result<(u64, u64), LibError> {
    let out = Path::new(&req.output);
    let temp = atomic_file(out)?;
    let mut writer = csv::Writer::from_writer(temp);
    let mut processed = 0u64;
    let mut emitted = 0u64;
    store.stream_export_rows(selection, vis, EXPORT_BATCH_SIZE, |batch| {
        for asset in batch {
            if req.attribution_only && !needs_attribution(asset) {
                continue;
            }
            if req.attribution_only {
                writer.serialize(credit_row(asset))
            } else {
                writer.serialize(manifest_row(asset))
            }
            .map_err(|e| LibError::Internal(format!("write csv: {e}")))?;
            emitted += 1;
        }
        processed += batch.len() as u64;
        checkpoint(processed)
    })?;
    writer.flush().map_err(io_err)?;
    let temp = writer
        .into_inner()
        .map_err(|e| LibError::Internal(format!("finish csv: {}", e.error())))?;
    persist_file(temp, out)?;
    Ok((emitted, 1))
}

fn emit_sidecars(
    store: &Store,
    selection: &ExportSelection,
    req: &ExportRequest,
    vis: &Visibility,
    checkpoint: &mut impl FnMut(u64) -> Result<(), LibError>,
) -> Result<(u64, u64), LibError> {
    let out = Path::new(&req.output);
    let parent = output_parent(out);
    std::fs::create_dir_all(parent).map_err(io_err)?;
    let stage = tempfile::Builder::new()
        .prefix(".3dam-export-")
        .tempdir_in(parent)
        .map_err(io_err)?;
    let mut processed = 0u64;
    let mut emitted = 0u64;
    store.stream_export_rows(selection, vis, EXPORT_BATCH_SIZE, |batch| {
        for asset in batch {
            if req.attribution_only && !needs_attribution(asset) {
                continue;
            }
            let path = available_sidecar_path(stage.path(), &asset.id.to_string(), &asset.name);
            let mut file = std::fs::File::create(path).map_err(io_err)?;
            if req.attribution_only {
                serde_json::to_writer_pretty(&mut file, &credit_row(asset))
            } else {
                serde_json::to_writer_pretty(&mut file, &manifest_row(asset))
            }
            .map_err(|e| LibError::Internal(format!("encode sidecar: {e}")))?;
            file.flush().map_err(io_err)?;
            emitted += 1;
        }
        processed += batch.len() as u64;
        checkpoint(processed)
    })?;
    replace_directory(stage, out)?;
    Ok((emitted, emitted))
}

fn manifest_row(asset: &ExportAssetRow) -> ManifestRow<'_> {
    ManifestRow {
        id: asset.id.to_string(),
        name: &asset.name,
        path: &asset.path,
        media: asset.media.as_str().to_string(),
        format: &asset.format,
        size_bytes: asset.size_bytes,
        hash: asset.hash.map(|hash| hash.to_hex()),
        license_id: &asset.license_id,
        license_status: asset.license_status.as_str().to_string(),
        commercial: asset.commercial,
        modify: asset.modify,
        redistribute: asset.redistribute,
        attribution: asset.attribution,
        attribution_holder: &asset.attribution_holder,
        attribution_credit: &asset.attribution_credit,
        license_url: &asset.license_url,
        tags: &asset.tags,
        note: &asset.note,
    }
}

fn credit_row(asset: &ExportAssetRow) -> CreditRow<'_> {
    CreditRow {
        id: asset.id.to_string(),
        name: &asset.name,
        license_id: &asset.license_id,
        license_status: asset.license_status.as_str().to_string(),
        holder: &asset.attribution_holder,
        credit: &asset.attribution_credit,
        url: &asset.license_url,
    }
}

fn needs_attribution(asset: &ExportAssetRow) -> bool {
    asset.attribution == Some(true)
        || asset.license_status == LicenseStatus::Attribution
        || asset.attribution_holder.is_some()
        || asset.attribution_credit.is_some()
}

/// Preserve the original collision behavior without a catalog-sized `HashMap`: the staged
/// directory itself is the bounded-memory collision index. Query order makes suffixes stable.
fn available_sidecar_path(dir: &Path, id: &str, name: &str) -> PathBuf {
    let base = sanitize(name);
    let base = if base.is_empty() { id } else { &base };
    let first = dir.join(format!("{base}.json"));
    if !first.exists() {
        return first;
    }
    for suffix in 1u64.. {
        let candidate = dir.join(format!("{base}-{suffix}.json"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn atomic_file(out: &Path) -> Result<tempfile::NamedTempFile, LibError> {
    let parent = output_parent(out);
    std::fs::create_dir_all(parent).map_err(io_err)?;
    tempfile::Builder::new()
        .prefix(".3dam-export-")
        .tempfile_in(parent)
        .map_err(io_err)
}

fn persist_file(temp: tempfile::NamedTempFile, out: &Path) -> Result<(), LibError> {
    temp.as_file().sync_all().map_err(io_err)?;
    temp.persist(out).map(|_| ()).map_err(|e| io_err(e.error))
}

fn replace_directory(stage: tempfile::TempDir, out: &Path) -> Result<(), LibError> {
    if out.exists() && !out.is_dir() {
        return Err(LibError::Internal(format!(
            "export io: destination is not a directory: {}",
            out.display()
        )));
    }
    let stage_path = stage.keep();
    if !out.exists() {
        return std::fs::rename(&stage_path, out).map_err(|error| {
            let _ = std::fs::remove_dir_all(&stage_path);
            io_err(error)
        });
    }

    let parent = output_parent(out);
    let backup = tempfile::Builder::new()
        .prefix(".3dam-export-old-")
        .tempdir_in(parent)
        .map_err(io_err)?;
    let backup_path = backup.keep();
    std::fs::remove_dir(&backup_path).map_err(io_err)?;
    std::fs::rename(out, &backup_path).map_err(io_err)?;
    if let Err(error) = std::fs::rename(&stage_path, out) {
        let _ = std::fs::rename(&backup_path, out);
        let _ = std::fs::remove_dir_all(&stage_path);
        return Err(io_err(error));
    }
    std::fs::remove_dir_all(backup_path).map_err(io_err)
}

fn output_parent(out: &Path) -> &Path {
    out.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn io_err(error: std::io::Error) -> LibError {
    LibError::Internal(format!("export io: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::id::ContentHash;
    use dam_sources::SourceConnection;

    fn large_store(count: usize) -> Store {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "large fixture",
                false,
            )
            .unwrap();
        for n in 0..count {
            store
                .upsert_asset(&dam_store::NewAsset {
                    source_id: source,
                    path: format!("folder/asset-{n:05}.png"),
                    filename: format!("asset-{n:05}.png"),
                    content_hash: Some(ContentHash([(n % 251) as u8; 32])),
                    size_bytes: Some(n as i64),
                    source_modified_at: None,
                    scanned_at: dam_store::now_ms(),
                    media_type: MediaType::Image,
                    format: "png".into(),
                })
                .unwrap();
        }
        store
    }

    #[test]
    fn whole_catalog_stream_is_batch_bounded_and_not_n_plus_one() {
        let store = large_store(1_003);
        let mut seen = 0usize;
        let stats = store
            .stream_export_rows(
                &ExportSelection::Query(QueryRequest::default()),
                &Visibility::Full,
                37,
                |batch| {
                    assert!(batch.len() <= 37);
                    seen += batch.len();
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(seen, 1_003);
        assert_eq!(stats.rows, 1_003);
        assert_eq!(stats.max_batch_rows, 37);
        assert!(stats.queries <= 29, "queries were batch-bounded: {stats:?}");
    }

    #[test]
    fn large_outputs_are_valid_and_staging_is_atomic() {
        let store = large_store(EXPORT_BATCH_SIZE * 2 + 9);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("manifest.json");
        let request = || ExportRequest {
            format: ExportFormat::Json,
            output: output.to_string_lossy().into_owned(),
            ..ExportRequest::default()
        };

        let report = run_export(&store, request(), &Visibility::Full).unwrap();
        assert_eq!(report.assets, (EXPORT_BATCH_SIZE * 2 + 9) as u64);
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(
            document["assets"].as_array().unwrap().len(),
            report.assets as usize
        );

        std::fs::write(&output, b"previous successful export").unwrap();
        let error = run_export_with_checkpoint(&store, request(), &Visibility::Full, |done| {
            if done >= EXPORT_BATCH_SIZE as u64 {
                Err(LibError::Cancelled)
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(matches!(error, LibError::Cancelled));
        assert_eq!(
            std::fs::read(&output).unwrap(),
            b"previous successful export"
        );

        let csv_output = dir.path().join("manifest.csv");
        let csv_report = run_export(
            &store,
            ExportRequest {
                format: ExportFormat::Csv,
                output: csv_output.to_string_lossy().into_owned(),
                ..ExportRequest::default()
            },
            &Visibility::Full,
        )
        .unwrap();
        let mut csv = csv::Reader::from_path(csv_output).unwrap();
        assert_eq!(
            csv.headers().unwrap().iter().collect::<Vec<_>>(),
            vec![
                "id",
                "name",
                "path",
                "media",
                "format",
                "size_bytes",
                "hash",
                "license_id",
                "license_status",
                "commercial",
                "modify",
                "redistribute",
                "attribution",
                "attribution_holder",
                "attribution_credit",
                "license_url",
                "tags",
                "note",
            ]
        );
        assert_eq!(csv.records().count() as u64, csv_report.assets);

        let sidecars = dir.path().join("sidecars");
        std::fs::create_dir(&sidecars).unwrap();
        std::fs::write(sidecars.join("stale.json"), b"stale").unwrap();
        let sidecar_report = run_export(
            &store,
            ExportRequest {
                format: ExportFormat::Sidecar,
                output: sidecars.to_string_lossy().into_owned(),
                ..ExportRequest::default()
            },
            &Visibility::Full,
        )
        .unwrap();
        assert_eq!(sidecar_report.files_written, sidecar_report.assets);
        assert_eq!(
            std::fs::read_dir(&sidecars).unwrap().count() as u64,
            sidecar_report.assets
        );
        assert!(!sidecars.join("stale.json").exists());

        // A sidecar destination that cannot be installed must retain the old target and clean the
        // fully populated staging directory too (the late-failure case).
        let invalid_sidecars = dir.path().join("not-a-directory");
        std::fs::write(&invalid_sidecars, b"keep me").unwrap();
        assert!(run_export(
            &store,
            ExportRequest {
                format: ExportFormat::Sidecar,
                output: invalid_sidecars.to_string_lossy().into_owned(),
                ..ExportRequest::default()
            },
            &Visibility::Full,
        )
        .is_err());
        assert_eq!(std::fs::read(&invalid_sidecars).unwrap(), b"keep me");
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".3dam-export-")
        }));
    }

    #[test]
    fn sidecar_collision_suffixes_remain_compatible_without_a_stem_vector() {
        let dir = tempfile::tempdir().unwrap();
        let first = available_sidecar_path(dir.path(), "unused", "same name.png");
        assert_eq!(first.file_name().unwrap(), "same_name.png.json");
        std::fs::write(&first, b"{}").unwrap();
        let second = available_sidecar_path(dir.path(), "unused", "same name.png");
        assert_eq!(second.file_name().unwrap(), "same_name.png-1.json");
        std::fs::write(&second, b"{}").unwrap();
        // Distinct source names that sanitize to the same base use the same deterministic sequence.
        let third = available_sidecar_path(dir.path(), "unused", "same_name.png");
        assert_eq!(third.file_name().unwrap(), "same_name.png-2.json");
    }
}
