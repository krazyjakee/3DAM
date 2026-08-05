//! Export and manifests (PRODUCT_SPEC §6.4): copying a selection out alongside a machine-readable
//! manifest of what was copied.

use super::query::QueryRequest;
use crate::id::{AssetId, CollectionId};
use serde::{Deserialize, Serialize};

/// What to export. Exactly one selector; defaults to the whole library when all are empty.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExportRequest {
    /// Explicit asset ids (takes precedence).
    #[serde(default)]
    pub assets: Vec<AssetId>,
    /// A collection / smart folder to export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<CollectionId>,
    /// A search to export (the same faceted query as browse).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
    pub format: ExportFormat,
    /// Destination: a file path for `json`/`csv`, a directory for `sidecar`.
    pub output: String,
    /// Restrict a manifest to license/attribution fields (the "credits list" use case).
    #[serde(default)]
    pub attribution_only: bool,
}

/// Manifest export whose destination is owned by the server and retrievable through the
/// authenticated job-artifact route. Keeping `output` out of this wire shape makes locality
/// explicit and prevents a download request from ever naming an arbitrary filesystem path.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ManagedExportRequest {
    #[serde(default)]
    pub assets: Vec<AssetId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<CollectionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
    pub format: ExportFormat,
    #[serde(default)]
    pub attribution_only: bool,
}

impl ManagedExportRequest {
    pub fn with_output(self, output: String) -> ExportRequest {
        ExportRequest {
            assets: self.assets,
            collection: self.collection,
            query: self.query,
            format: self.format,
            output,
            attribution_only: self.attribution_only,
        }
    }
}

/// Manifest shape (tech-spec: JSON/CSV/sidecar).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// A single JSON document: `{ "assets": [ … ] }`.
    #[default]
    Json,
    /// A single CSV file, one row per asset.
    Csv,
    /// One `<name>.json` sidecar per asset, written under the output directory.
    Sidecar,
}

impl ExportFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(ExportFormat::Json),
            "csv" => Some(ExportFormat::Csv),
            "sidecar" => Some(ExportFormat::Sidecar),
            _ => None,
        }
    }
}

/// Result of an export run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExportReport {
    pub format: ExportFormat,
    /// The file (json/csv) or directory (sidecar) written.
    pub output: String,
    pub assets: u64,
    /// Number of files written (1 for json/csv, N for sidecar).
    pub files_written: u64,
}
