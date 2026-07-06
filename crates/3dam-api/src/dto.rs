//! Request/response DTOs (tech-spec 03 §3–§4). These are the same structs used as Rust args
//! and JSON bodies. Only the phase-1 slice is modelled here; the rest slot in as their areas land.

use crate::id::{AssetId, CollectionId, ContentHash, JobId, SourceId};
use crate::page::PageParams;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A few media-specific display attributes carried on a summary row (bpm, dims, tris…).
pub type SmallMap = BTreeMap<String, String>;
/// Named counts (by media type, by source…).
pub type CountMap = BTreeMap<String, u64>;

// ── media / license shared value types ─────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Audio,
    Image,
    Model,
}

impl MediaType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaType::Audio => "audio",
            MediaType::Image => "image",
            MediaType::Model => "model",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "audio" => Some(MediaType::Audio),
            "image" => Some(MediaType::Image),
            "model" => Some(MediaType::Model),
            _ => None,
        }
    }
}

/// Where a result row lives — local or a named peer (attributable, PRODUCT_SPEC §4.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Local,
    Peer(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LicenseStatus {
    Permissive,
    Attribution,
    Restricted,
    #[default]
    Unknown,
}

impl LicenseStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LicenseStatus::Permissive => "permissive",
            LicenseStatus::Attribution => "attribution",
            LicenseStatus::Restricted => "restricted",
            LicenseStatus::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> LicenseStatus {
        match s {
            "permissive" => LicenseStatus::Permissive,
            "attribution" => LicenseStatus::Attribution,
            "restricted" => LicenseStatus::Restricted,
            _ => LicenseStatus::Unknown,
        }
    }
}

/// Compact license marker for a grid row (id + badge colour bucket).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LicenseBadge {
    pub id: Option<String>,
    pub status: LicenseStatus,
}

impl Default for LicenseBadge {
    fn default() -> Self {
        Self {
            id: None,
            status: LicenseStatus::Unknown,
        }
    }
}

/// Full first-class rights block (tech-spec 02 §5).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct License {
    pub id: Option<String>,
    pub status: LicenseStatus,
    pub commercial: Option<bool>,
    pub modify: Option<bool>,
    pub redistribute: Option<bool>,
    pub attribution: Option<bool>,
    pub holder: Option<String>,
    pub credit: Option<String>,
    pub url: Option<String>,
    pub provenance: String,
}

// ── asset shapes ───────────────────────────────────────────────────────────

/// Grid/table row — cheap, no heavy blobs (tech-spec 03 §4).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetSummary {
    pub id: AssetId,
    pub name: String,
    pub media: MediaType,
    pub format: String,
    pub size: u64,
    pub license: LicenseBadge,
    #[serde(default)]
    pub top_tags: Vec<String>,
    pub origin: Origin,
    #[serde(default)]
    pub key_attrs: SmallMap,
}

/// Full inspector record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Asset {
    pub summary: AssetSummary,
    pub hash: Option<ContentHash>,
    pub source_id: SourceId,
    pub path: String,
    pub timestamps: AssetTimes,
    pub attributes: MediaAttributes,
    pub license: License,
    #[serde(default)]
    pub tags: Vec<TagRef>,
    #[serde(default)]
    pub collections: Vec<CollectionId>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AssetTimes {
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub scanned: i64,
    pub analyzed: Option<i64>,
}

/// Per-media attribute payload. Cheap, human-readable stats only (tech-spec 02 §3.2);
/// embeddings live in the vector index, not here.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "media", rename_all = "lowercase")]
pub enum MediaAttributes {
    Audio(AudioAttributes),
    Image(ImageAttributes),
    Model(ModelAttributes),
    /// Cheap metadata not yet extracted.
    None,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AudioAttributes {
    pub duration_ms: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageAttributes {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub has_alpha: Option<bool>,
    pub color_space: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelAttributes {
    pub vertex_count: Option<i64>,
    pub triangle_count: Option<i64>,
    pub mesh_count: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagRef {
    pub name: String,
    pub state: String,  // suggested | confirmed | rejected
    pub source: String, // auto | user
    pub confidence: Option<f32>,
}

// ── query ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryRequest {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    pub scope: QueryScope,
    #[serde(default)]
    pub page: PageParams,
    #[serde(default)]
    pub include_facets: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Filter {
    pub field: FacetField,
    pub op: FilterOp,
    pub value: FilterValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacetField {
    MediaType,
    Format,
    Source,
    Tag,
    SizeBytes,
    License,
    UsageRight,
    // media-specific (subset wired in phase 1; the rest are accepted but may be Unsupported)
    Width,
    Height,
    Bpm,
    TriCount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    Range,
    Contains,
    Exists,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterValue {
    Str(String),
    Num(f64),
    Bool(bool),
    Range(f64, f64),
    List(Vec<FilterValue>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sort {
    pub field: SortField,
    pub dir: SortDir,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            field: SortField::Name,
            dir: SortDir::Asc,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    Name,
    Size,
    Scanned,
    Relevance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryScope {
    #[default]
    Local,
    Federated,
    Sources(Vec<SourceId>),
}

// ── sources ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    LocalFs,
    Sftp,
    Smb,
    Federated,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::LocalFs => "local_fs",
            SourceKind::Sftp => "sftp",
            SourceKind::Smb => "smb",
            SourceKind::Federated => "federated",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local_fs" => Some(SourceKind::LocalFs),
            "sftp" => Some(SourceKind::Sftp),
            "smb" => Some(SourceKind::Smb),
            "federated" => Some(SourceKind::Federated),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Online,
    Offline,
    Scanning,
    Error(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceStats {
    pub asset_count: u64,
    pub last_scanned_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: SourceId,
    pub kind: SourceKind,
    pub name: String,
    pub uri: String,
    pub state: SourceState,
    pub stats: SourceStats,
    pub watch: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddSource {
    pub kind: SourceKind,
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub options: SourceOptions,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceOptions {
    #[serde(default)]
    pub watch: bool,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoveSource {
    /// Fail-soft: offline sources keep cached rows (PRODUCT_SPEC §6.1).
    #[serde(default)]
    pub keep_metadata: bool,
}

// ── jobs ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanRequest {
    #[serde(default)]
    pub sources: Vec<SourceId>,
    #[serde(default)]
    pub mode: ScanMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    #[default]
    Full,
    Delta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Scan,
    Analyze,
    Convert,
    Export,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
    pub current: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    pub progress: Progress,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JobListRequest {
    #[serde(default)]
    pub kinds: Vec<JobKind>,
    #[serde(default)]
    pub state: Option<JobState>,
    #[serde(default)]
    pub page: PageParams,
}

// ── stats ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LibraryStats {
    pub total: u64,
    pub by_media: CountMap,
    pub by_source: CountMap,
    pub unanalyzed: u64,
    pub sources: u64,
}
