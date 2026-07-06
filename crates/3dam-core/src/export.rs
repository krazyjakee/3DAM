//! Export / manifests (phase 4 Reach; PRODUCT_SPEC §6.4 — "export metadata and manifests for use in
//! engines and pipelines"). A selector (explicit ids / a collection / a saved query / the whole
//! library) resolves to a set of assets; each is flattened to a manifest row carrying identity,
//! key attributes, license + attribution, and confirmed tags. Three shapes: a single JSON document,
//! a single CSV, or one JSON **sidecar** per asset under an output directory.
//!
//! `attribution_only` narrows to the assets that actually need crediting and emits a focused
//! credits list — the "tell me which ones need attribution before I ship" use case (§1 personas).
//!
//! Read-only and non-destructive: export never touches source files or catalog state.

use dam_api::dto::*;
use dam_api::id::AssetId;
use dam_api::LibError;
use dam_store::Store;
use serde::Serialize;
use std::path::Path;

/// A full manifest row — everything a pipeline needs to place and attribute an asset.
#[derive(Serialize)]
struct ManifestRow {
    id: String,
    name: String,
    path: String,
    media: String,
    format: String,
    size_bytes: u64,
    hash: Option<String>,
    license_id: Option<String>,
    license_status: String,
    commercial: Option<bool>,
    modify: Option<bool>,
    redistribute: Option<bool>,
    attribution: Option<bool>,
    attribution_holder: Option<String>,
    attribution_credit: Option<String>,
    license_url: Option<String>,
    /// Confirmed tags, `;`-joined (CSV-friendly).
    tags: String,
}

/// A credits row — the attribution-only subset.
#[derive(Serialize)]
struct CreditRow {
    id: String,
    name: String,
    license_id: Option<String>,
    license_status: String,
    holder: Option<String>,
    credit: Option<String>,
    url: Option<String>,
}

pub(crate) fn run_export(store: &Store, req: ExportRequest) -> Result<ExportReport, LibError> {
    let ids = resolve_ids(store, &req)?;

    // Gather full records (license/attribution/tags need the inspector record, not the grid row).
    let mut assets = Vec::with_capacity(ids.len());
    for id in &ids {
        // An id that vanished between resolve and read is skipped, not fatal (fail-soft).
        if let Ok(a) = store.get_asset(id) {
            assets.push(a);
        }
    }

    let files_written = if req.attribution_only {
        let rows: Vec<CreditRow> = assets.iter().filter(|a| needs_attribution(a)).map(credit_row).collect();
        let stems = sidecar_stems(rows.iter().map(|r| (&r.id, &r.name)));
        emit(&rows, &stems, &req)?
    } else {
        let rows: Vec<ManifestRow> = assets.iter().map(manifest_row).collect();
        let stems = sidecar_stems(rows.iter().map(|r| (&r.id, &r.name)));
        emit(&rows, &stems, &req)?
    };

    // For the report, count what actually went into the manifest.
    let asset_count = if req.attribution_only {
        assets.iter().filter(|a| needs_attribution(a)).count() as u64
    } else {
        assets.len() as u64
    };

    Ok(ExportReport {
        format: req.format,
        output: req.output,
        assets: asset_count,
        files_written,
    })
}

/// Resolve the selector to an ordered id set (ids → collection → query → whole library).
fn resolve_ids(store: &Store, req: &ExportRequest) -> Result<Vec<AssetId>, LibError> {
    if !req.assets.is_empty() {
        return Ok(req.assets.clone());
    }
    if let Some(cid) = req.collection {
        let coll = store.get_collection(&cid)?;
        return match coll.kind {
            CollectionKind::Manual => store.collection_member_ids(&cid),
            CollectionKind::Smart => store.query_asset_ids(&coll.query.unwrap_or_default()),
        };
    }
    store.query_asset_ids(&req.query.clone().unwrap_or_default())
}

fn needs_attribution(a: &Asset) -> bool {
    a.license.attribution == Some(true)
        || a.license.status == LicenseStatus::Attribution
        || a.license.holder.is_some()
        || a.license.credit.is_some()
}

fn manifest_row(a: &Asset) -> ManifestRow {
    let tags = a
        .tags
        .iter()
        .filter(|t| t.state == "confirmed")
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    ManifestRow {
        id: a.summary.id.to_string(),
        name: a.summary.name.clone(),
        path: a.path.clone(),
        media: a.summary.media.as_str().to_string(),
        format: a.summary.format.clone(),
        size_bytes: a.summary.size,
        hash: a.hash.map(|h| h.to_hex()),
        license_id: a.license.id.clone(),
        license_status: a.license.status.as_str().to_string(),
        commercial: a.license.commercial,
        modify: a.license.modify,
        redistribute: a.license.redistribute,
        attribution: a.license.attribution,
        attribution_holder: a.license.holder.clone(),
        attribution_credit: a.license.credit.clone(),
        license_url: a.license.url.clone(),
        tags,
    }
}

fn credit_row(a: &Asset) -> CreditRow {
    CreditRow {
        id: a.summary.id.to_string(),
        name: a.summary.name.clone(),
        license_id: a.license.id.clone(),
        license_status: a.license.status.as_str().to_string(),
        holder: a.license.holder.clone(),
        credit: a.license.credit.clone(),
        url: a.license.url.clone(),
    }
}

/// Sidecar filename stems, disambiguated so two assets named the same don't collide.
fn sidecar_stems<'a>(rows: impl Iterator<Item = (&'a String, &'a String)>) -> Vec<String> {
    let mut seen = std::collections::HashMap::<String, u32>::new();
    let mut stems = Vec::new();
    for (id, name) in rows {
        let base = sanitize(name);
        let base = if base.is_empty() { id.clone() } else { base };
        let n = seen.entry(base.clone()).or_insert(0);
        let stem = if *n == 0 { base.clone() } else { format!("{base}-{n}") };
        *n += 1;
        stems.push(stem);
    }
    stems
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

/// A JSON wrapper so the document is `{ "assets": [ … ] }`.
#[derive(Serialize)]
struct Manifest<'a, T> {
    assets: &'a [T],
}

fn emit<T: Serialize>(rows: &[T], stems: &[String], req: &ExportRequest) -> Result<u64, LibError> {
    let out = Path::new(&req.output);
    match req.format {
        ExportFormat::Json => {
            if let Some(dir) = out.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir).map_err(io_err)?;
                }
            }
            let file = std::fs::File::create(out).map_err(io_err)?;
            serde_json::to_writer_pretty(file, &Manifest { assets: rows })
                .map_err(|e| LibError::Internal(format!("write json: {e}")))?;
            Ok(1)
        }
        ExportFormat::Csv => {
            if let Some(dir) = out.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir).map_err(io_err)?;
                }
            }
            let mut wtr = csv::Writer::from_path(out).map_err(|e| LibError::Internal(format!("open csv: {e}")))?;
            for row in rows {
                wtr.serialize(row)
                    .map_err(|e| LibError::Internal(format!("write csv: {e}")))?;
            }
            wtr.flush().map_err(io_err)?;
            Ok(1)
        }
        ExportFormat::Sidecar => {
            std::fs::create_dir_all(out).map_err(io_err)?;
            let mut written = 0u64;
            for (row, stem) in rows.iter().zip(stems) {
                let path = out.join(format!("{stem}.json"));
                let json = serde_json::to_string_pretty(row)
                    .map_err(|e| LibError::Internal(format!("encode sidecar: {e}")))?;
                std::fs::write(&path, json).map_err(io_err)?;
                written += 1;
            }
            Ok(written)
        }
    }
}

fn io_err(e: std::io::Error) -> LibError {
    LibError::Internal(format!("export io: {e}"))
}
