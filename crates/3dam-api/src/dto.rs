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

/// The media classes the catalog spans. `Audio`/`Image`/`Model` are the **deep** types — full
/// decode, analysis, embeddings, conversion. `Video`/`Document` (PRODUCT_SPEC §9 phase 2b) are
/// deliberately shallower: they exist so a source can be catalogued *completely*, and each is
/// honest about what it can't do rather than faking parity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Audio,
    Image,
    Model,
    Video,
    Document,
}

/// Raw asset bytes for a preview, plus just enough descriptor to serve/label them. This is the
/// out-of-band data-handoff the WASM viewer islands consume (tech-spec 09 §B.3): the DOM fetches it
/// and feeds `load_model` / a WebAudio decode. Not a JSON DTO — the server streams `bytes` with
/// `content_type`, and `ApiClient` reconstructs it from the raw HTTP response.
#[derive(Clone, Debug)]
pub struct AssetContent {
    pub bytes: Vec<u8>,
    /// MIME type for the `Content-Type` header (`model/gltf-binary`, `audio/wav`, …).
    pub content_type: String,
    /// The asset's format token (e.g. `glb`, `wav`), for logging/labels.
    pub format: String,
    pub media: MediaType,
}

/// Best-effort MIME for a `(media, format)` pair — covers the v1 decode matrix (ADR 0009 §8) and
/// falls back to `application/octet-stream`. Lives here so both the engine and any client agree.
pub fn content_type_for(media: MediaType, format: &str) -> &'static str {
    let fmt = format.to_ascii_lowercase();
    // Container extensions shared by more than one media class must consult `media` first —
    // `.mp4`/`.mov` are audio-only or full video depending on their track table, and serving a
    // video as `audio/mp4` makes the browser refuse to render a picture (PRODUCT_SPEC §9 2b).
    match (media, fmt.as_str()) {
        (MediaType::Video, "mp4" | "m4v") => return "video/mp4",
        (MediaType::Video, "mov") => return "video/quicktime",
        (MediaType::Video, "webm") => return "video/webm",
        (MediaType::Video, "mkv") => return "video/x-matroska",
        (MediaType::Video, "avi") => return "video/x-msvideo",
        (MediaType::Video, "ogv") => return "video/ogg",
        _ => {}
    }
    match fmt.as_str() {
        // 3D
        "glb" => "model/gltf-binary",
        "gltf" => "model/gltf+json",
        "obj" => "model/obj",
        "ply" => "model/ply",
        "stl" => "model/stl",
        "fbx" => "application/octet-stream",
        // audio
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        // `mov`/`m4v` land here only when the content probe has already reclassified the file as
        // audio-only (it keeps the original format token) — the video arm above caught every other
        // case. Serving those as `application/octet-stream` would make the one file the probe
        // exists to detect the one file that won't play.
        "aac" | "m4a" | "mp4" | "mov" | "m4v" => "audio/mp4",
        // image
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        // documents — text/* types carry an explicit charset so the browser doesn't guess
        "pdf" => "application/pdf",
        "md" => "text/markdown; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "rtf" => "application/rtf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "odt" => "application/vnd.oasis.opendocument.text",
        _ => match media {
            MediaType::Image
            | MediaType::Audio
            | MediaType::Model
            | MediaType::Video
            | MediaType::Document => "application/octet-stream",
        },
    }
}

impl MediaType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaType::Audio => "audio",
            MediaType::Image => "image",
            MediaType::Model => "model",
            MediaType::Video => "video",
            MediaType::Document => "document",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "audio" => Some(MediaType::Audio),
            "image" => Some(MediaType::Image),
            "model" => Some(MediaType::Model),
            "video" => Some(MediaType::Video),
            "document" => Some(MediaType::Document),
            _ => None,
        }
    }
    /// Every variant, in display order. Lets callers enumerate media types without re-listing
    /// them (and silently missing one) each time a variant is added.
    pub const ALL: &'static [MediaType] = &[
        MediaType::Audio,
        MediaType::Image,
        MediaType::Model,
        MediaType::Video,
        MediaType::Document,
    ];
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
    /// User-flagged favourite (issue #63) — persisted in the asset `flags` bitset. `#[serde(default)]`
    /// so pre-favourite payloads deserialize to `false`.
    #[serde(default)]
    pub favorite: bool,
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
    Video(VideoAttributes),
    Document(DocumentAttributes),
    /// Cheap metadata not yet extracted.
    None,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AudioAttributes {
    pub duration_ms: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    /// Auto-category guess from analysis (`one_shot` | `loop` | `music` | `sfx`); tech-spec 05 §4.
    #[serde(default)]
    pub class: Option<String>,
    // Continuous acoustic features from the analyze pass (issue #61); `None` until analysed.
    /// Integrated loudness in dBFS (an RMS approximation of LUFS for v1). Typically negative.
    #[serde(default)]
    pub loudness_lufs: Option<f32>,
    /// Spectral-centroid brightness, normalised 0–1 (0 = dark/low, 1 = bright/high).
    #[serde(default)]
    pub brightness: Option<f32>,
    /// Harmonic-vs-noise ratio, normalised 0–1 (0 = noisy, 1 = tonal/harmonic).
    #[serde(default)]
    pub harmonicity: Option<f32>,
    /// Normalised (0–1) waveform peak buckets, computed server-side by the analysis pass (issue #73)
    /// so clients draw the inspector waveform without decoding the audio themselves. `None` until
    /// analysed; the bar count is whatever the server produced (clients render `peaks.len()` bars).
    #[serde(default)]
    pub peaks: Option<Vec<f32>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageAttributes {
    pub width: Option<i64>,
    pub height: Option<i64>,
    #[serde(default)]
    pub color_depth: Option<i64>,
    pub has_alpha: Option<bool>,
    pub color_space: Option<String>,
    // ── derived by analysis (tech-spec 05 §5, §6); None until the analyze pass runs ──
    /// Perceptual (dHash) hash, hex-encoded — the near-dup signal (§4.2).
    #[serde(default)]
    pub phash: Option<String>,
    /// Edge-continuity tileability score in [0,1] (§6.2).
    #[serde(default)]
    pub tileability: Option<f32>,
    /// Detected internal repeat period in source pixels, if the image already tiles (§6.3).
    #[serde(default)]
    pub repeat_period: Option<i64>,
    /// `seamless` | `tiled` | `non_tiling` (§6.4).
    #[serde(default)]
    pub tile_class: Option<String>,
    /// Dominant colours as `#rrggbb` hex, most-prominent first.
    #[serde(default)]
    pub dominant_colors: Vec<String>,
    /// Auto-category guess (`texture` | `photo` | `sprite` | …).
    #[serde(default)]
    pub class: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelAttributes {
    pub vertex_count: Option<i64>,
    pub triangle_count: Option<i64>,
    pub mesh_count: Option<i64>,
    #[serde(default)]
    pub material_count: Option<i64>,
    #[serde(default)]
    pub texture_count: Option<i64>,
    /// On-disk bytes of the model's external companion files — textures, glTF `.bin` buffers, an
    /// OBJ's `.mtl` and its maps. The asset's reported `size` adds this to the mesh container so it
    /// reflects the whole asset; `None`/absent for self-contained (embedded) models.
    #[serde(default)]
    pub dependency_bytes: Option<i64>,
    #[serde(default)]
    pub has_rig: Option<bool>,
    #[serde(default)]
    pub has_animation: Option<bool>,
    #[serde(default)]
    pub has_uvs: Option<bool>,
    /// Auto-category guess (`prop` | `character` | `environment` | …); tech-spec 05 §5.
    #[serde(default)]
    pub class: Option<String>,
}

/// Cheap-tier video facts, read from the container by a discovered `ffprobe`
/// ([ADR 0014](../../../docs/adr/0014-video-decode-backend.md)). Every field is `Option` because
/// the whole struct is empty-but-valid when no prober is installed — a video with no metadata is
/// still a catalogued, browsable, playable asset.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VideoAttributes {
    pub duration_ms: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Average frame rate, frames per second.
    #[serde(default)]
    pub fps: Option<f32>,
    /// Video codec of the first video stream (`h264`, `vp9`, `av1`, …).
    #[serde(default)]
    pub codec: Option<String>,
    /// Container/format name (`mov,mp4,m4a,3gp,3g2,mj2`, `matroska,webm`, …).
    #[serde(default)]
    pub container: Option<String>,
    /// Overall container bitrate in bits per second.
    #[serde(default)]
    pub bitrate: Option<i64>,
    /// Whether the container carries at least one audio stream.
    #[serde(default)]
    pub has_audio: Option<bool>,
    /// Auto-category guess (`cutscene` | `clip` | `loop` | …); `None` until analysed.
    #[serde(default)]
    pub class: Option<String>,
}

/// Cheap-tier document facts. Documents are the first media type whose *content is language*
/// (PRODUCT_SPEC §9 phase 2b), so this carries a short excerpt for display; the full extracted
/// text goes to the FTS index rather than travelling on every summary row.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DocumentAttributes {
    pub page_count: Option<i64>,
    pub word_count: Option<i64>,
    /// Document title from the container's metadata (PDF info dict, DOCX core properties, or a
    /// leading Markdown `#` heading) — *not* the filename.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    /// Detected text encoding for plaintext formats (`utf-8`, `utf-16le`, `latin-1`).
    #[serde(default)]
    pub encoding: Option<String>,
    /// A short leading excerpt for the inspector/grid card. Truncated on a character boundary.
    #[serde(default)]
    pub excerpt: Option<String>,
    /// Auto-category guess (`license` | `readme` | `design_doc` | `receipt` | …).
    #[serde(default)]
    pub class: Option<String>,
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
    pub page: PageParams,
    #[serde(default)]
    pub include_facets: bool,
    /// How the `text` query is matched (semantic-search M5). Defaults to `Lexical` so existing
    /// callers and stored queries are unchanged; `Hybrid`/`Semantic` widen results with embedding
    /// neighbours of the lexical hits.
    #[serde(default)]
    pub mode: SearchMode,
    /// Answer from the local catalog only — no federated fan-out. Set on every peer-bound call so
    /// a peer never re-fans-out to *its* peers (federation is one hop, never transitive — ADR 0009
    /// §5). Old peers that predate the field simply ignore it, which is the same thing.
    #[serde(default)]
    pub local_only: bool,
}

/// Text-search strategy (semantic-search M5). `Lexical` is the FTS/synonym path (M1–M3). `Hybrid`
/// keeps every lexical hit and *adds* embedding-nearest neighbours of those hits, reciprocal-rank
/// fused — so "ak47" also surfaces visually/geometrically similar props. `Semantic` ranks purely by
/// that embedding neighbourhood (lexical hits seed it). With no embeddings present both degrade to
/// `Lexical`. When the model-backed spaces land (M4) the same fusion simply gets better vectors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Lexical,
    Hybrid,
    Semantic,
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
    // ── media-specific structured attributes ────────────────────────────────
    // Each maps to a typed column in a per-media attr table (`audio_attr`/`image_attr`/`model_attr`).
    // Numeric fields take Eq/Lt/Lte/Gt/Gte/Range; string fields Eq/In; boolean fields Eq(bool). These
    // back the Advanced Search dropdown/range controls — the tags-vs-attributes split (bounded,
    // extracted metadata belongs here, not in the open-vocabulary tag set).
    // Image (image_attr):
    Width,
    Height,
    ColorDepth,
    HasAlpha,
    ColorSpace,
    ImageClass,
    Tileability,
    TileClass,
    // Audio (audio_attr):
    Bpm,
    Duration,
    SampleRate,
    BitDepth,
    Channels,
    MusicalKey,
    Loudness,
    Brightness,
    Harmonicity,
    AudioClass,
    Codec,
    Container,
    // Model (model_attr):
    TriCount,
    VertexCount,
    MeshCount,
    MaterialCount,
    TextureCount,
    DependencyBytes,
    HasRig,
    HasAnimation,
    HasUv,
    ModelClass,
    // Video (video_attr). `Width`/`Height`/`Duration` above also match video — the columns mean
    // the same thing there, so those facets span both tables rather than being duplicated.
    Fps,
    Bitrate,
    HasAudio,
    VideoClass,
    // Document (document_attr):
    PageCount,
    WordCount,
    Author,
    DocumentClass,
    /// User-flagged favourite (issue #63). Presence of the filter means "favourites only".
    Favorite,
    /// Source-relative path prefix (issue #66) — scopes the browse to a folder subtree. The value is
    /// the prefix (e.g. `Environment/Rock/`); an empty prefix matches everything.
    Path,
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

// ── folder navigation (issue #66) ────────────────────────────────────────────

/// Enumerate the immediate subfolders directly under `prefix` within one source — the lazy unit the
/// folder tree expands. `prefix` is source-relative with a trailing slash (or empty for the source
/// root); paths use `/` separators.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderListing {
    pub source: SourceId,
    #[serde(default)]
    pub prefix: String,
}

/// One immediate subfolder (issue #66): its segment `name` and how many assets live anywhere beneath
/// it (the whole subtree, so a collapsed folder still shows its weight).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderEntry {
    pub name: String,
    pub asset_count: u64,
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
    // ── connection auth for network sources (SFTP/SMB); ignored for local (tech-spec 07 §3.2) ──
    /// Login user (overrides any `user@` in the URI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Password (SFTP or SMB). Stored in the source's connection blob, never returned to clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Path to a private key file for SFTP key auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// Passphrase for an encrypted private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// SMB domain/workgroup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Override the default port (22 SFTP / 445 SMB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoveSource {
    /// Fail-soft: offline sources keep cached rows (PRODUCT_SPEC §6.1).
    #[serde(default)]
    pub keep_metadata: bool,
}

// ── remove / blocklist (issue #21) ──────────────────────────────────────────

/// Remove one asset from the catalog. Optionally record its content hash on the blocklist so a
/// later scan/watch/auto-rescan never re-imports the same bytes (§2.2). Non-destructive to the
/// source: only the catalog row is deleted, the file on disk is untouched.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemoveAsset {
    /// Also block the asset's content hash from being re-imported by any future scan.
    #[serde(default)]
    pub block: bool,
}

/// One blocked content hash — the review surface for the "removed + blocked" set. Carries the
/// last-known filename (captured at block time) so the entry is human-recognisable after the row
/// it came from is gone.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockEntry {
    pub hash: ContentHash,
    /// The filename the asset had when it was blocked (best-effort, for display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub blocked_at: i64,
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

// ── convert (tech-spec 08) ───────────────────────────────────────────────────

/// A submitted convert plan: an input set, one target spec, and where outputs land. One request →
/// one report (one item per input). CLI-first in v1 (tech-spec 08 §1; the job/progress model layers
/// on later). Non-destructive by construction — outputs always go under `output_dir`, never over a
/// source (§5.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConvertRequest {
    pub inputs: Vec<AssetId>,
    pub target: ConvertTarget,
    /// User-chosen destination directory (required; never a source tree — §5.1).
    pub output_dir: String,
    /// Plan only: resolve outputs + estimate, write nothing (§4.2).
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub on_collision: CollisionRule,
}

/// The media-typed encode spec (tech-spec 08 §3). One target per request; a batch that mixes media
/// types against a single-media target fails those items as `unsupported` (fail-soft).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "media", rename_all = "lowercase")]
pub enum ConvertTarget {
    Image {
        /// `png` | `jpg` | `webp` | `bmp` | `tga` | `tiff` | `gif`.
        format: String,
        /// Fit within this box on the long edge (aspect preserved); None keeps source size.
        #[serde(default)]
        max_edge: Option<u32>,
        /// Lossy-encoder quality 1..=100 (JPEG); ignored for lossless formats.
        #[serde(default)]
        quality: Option<u8>,
    },
    Audio {
        /// `wav` in v1 (lossless PCM); other codecs stage later (tech-spec 08 §3.1).
        format: String,
    },
}

impl ConvertTarget {
    pub fn media(&self) -> MediaType {
        match self {
            ConvertTarget::Image { .. } => MediaType::Image,
            ConvertTarget::Audio { .. } => MediaType::Audio,
        }
    }
    /// The concrete output format token (drives the output extension).
    pub fn format(&self) -> &str {
        match self {
            ConvertTarget::Image { format, .. } => format,
            ConvertTarget::Audio { format } => format,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionRule {
    /// Planned path exists → the item fails (§5.3). The safe default.
    #[default]
    Fail,
    /// Disambiguate: `foo.png` → `foo-1.png`, `foo-2.png`, …
    Suffix,
    /// Leave the existing file; the item is done-but-skipped.
    Skip,
    /// Replace a non-source file (never a source — §5.1).
    Overwrite,
}

/// How one input resolved during planning/commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Would be / was written.
    Write,
    /// Planned path already exists and the rule forbids replacing it.
    Collision,
    /// Existing output left in place (Skip rule).
    Skipped,
    /// (from → to) not encodable in this build.
    Unsupported,
    /// Successfully written (commit).
    Done,
    /// Encode/IO/source-safety failure.
    Failed,
}

/// Per-input row of a convert report (dry-run or commit).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConvertItemReport {
    pub input: AssetId,
    pub input_path: String,
    /// The exact path that would be / was written.
    pub planned_output: String,
    pub disposition: Disposition,
    pub input_bytes: u64,
    #[serde(default)]
    pub output_bytes: Option<u64>,
    /// output_bytes / input_bytes, filled on a real (committed) encode.
    #[serde(default)]
    pub ratio: Option<f32>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The whole-batch result. Fail-soft: the request succeeds as long as it ran, even if some items
/// failed; counts summarise the outcome (tech-spec 08 §1.1).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConvertReport {
    pub dry_run: bool,
    pub output_dir: String,
    pub items: Vec<ConvertItemReport>,
    pub total_input_bytes: u64,
    pub total_output_bytes: u64,
    pub done: usize,
    pub failed: usize,
    pub collisions: usize,
    pub unsupported: usize,
}

// ── stats ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LibraryStats {
    pub total: u64,
    pub by_media: CountMap,
    pub by_source: CountMap,
    /// The most-used confirmed tags (asset count per tag) — the vocabulary that powers the Tags
    /// filter facet. Capped to the top handful so the sidebar stays a browsable summary, not the
    /// full long tail. Empty until assets carry confirmed tags.
    pub tags: CountMap,
    pub unanalyzed: u64,
    pub sources: u64,
}

// ── analysis / automation (tech-spec 05, phase 3) ────────────────────────────

/// Submit an analysis pass. With no `assets`, the runner plans every asset that is *due* — behind
/// the current extractor versions (§1.2, §7.2) — so a re-run is incremental, not a full re-sweep.
/// `force` re-analyses even up-to-date assets (e.g. after tuning thresholds). Background job.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AnalyzeRequest {
    #[serde(default)]
    pub assets: Vec<AssetId>,
    #[serde(default)]
    pub force: bool,
}

/// Force the derived preview cache to be rebuilt for specific assets: drop each asset's cached
/// thumbnail PNG(s) (all edges + renderer variants) and its 3D preview blob, so the next read
/// re-renders from source. Content-keyed and non-destructive — only regenerable derivatives are
/// removed; the source bytes are never touched (PRODUCT_SPEC §8). Unlike the whole-tier admin
/// clear-cache, this is a per-asset front-end action (e.g. after a source file was edited in place).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThumbnailRegenRequest {
    pub assets: Vec<AssetId>,
}

/// What a thumbnail-regen pass dropped: how many assets were visited and cache files removed.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ThumbnailRegenReport {
    pub assets: u64,
    pub files_deleted: u64,
}

/// A prefetch hint (hosted mode, issue #72): the assets a client is about to render, so the server
/// warms their thumbnails (at `edge`) and — for models — preview meshes ahead of the HTTP fetch.
/// Fire-and-forget; the bytes still travel on HTTP/2 (ADR 0012), this only moves generation earlier.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PrefetchRequest {
    pub assets: Vec<AssetId>,
    /// Thumbnail long-edge to warm. `None` → the server's default grid edge.
    #[serde(default)]
    pub edge: Option<u32>,
}

/// A "find similar" query. By-asset-id in v1 ("more like this"); the upload-a-reference entry point
/// (tech-spec 05 §3.2) lands with the MCP/web upload path. Scoped to the query asset's media space.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimilarRequest {
    pub asset: AssetId,
    /// How many neighbours to return (post-filter, post-self-drop).
    #[serde(default = "default_k")]
    pub k: u32,
    /// Compose with the same faceted filters as text search (§3.3).
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Answer from the local index only — no federated fan-out or peer forwarding (phase 6). Set
    /// on every peer-bound call so federation stays one hop, never transitive (ADR 0009 §5).
    #[serde(default)]
    pub local_only: bool,
}

fn default_k() -> u32 {
    24
}

/// One similarity hit: the neighbour plus its cosine score and the space it was ranked in
/// (the explanation, DESIGN_GUIDELINES §1.2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimilarHit {
    pub asset: AssetSummary,
    /// Cosine similarity in [0,1] (1 = identical direction).
    pub score: f32,
    /// The `EmbeddingSpace` id the ranking happened in.
    pub space: String,
}

/// Which duplicate tier to surface for the review view (tech-spec 05 §4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DupKind {
    /// Byte-identical (content-hash groups) — free, exact.
    #[default]
    Exact,
    /// Perceptually close but not identical (pHash / embedding cosine).
    Near,
}

/// Request the duplicate groups for review. Optionally scoped to one media type.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DupRequest {
    #[serde(default)]
    pub kind: DupKind,
    #[serde(default)]
    pub media: Option<MediaType>,
    #[serde(default = "default_dup_limit")]
    pub limit: u32,
}

fn default_dup_limit() -> u32 {
    100
}

/// A cluster of duplicates for the review view (§4.3). Never auto-deleted — 3DAM only groups.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupGroup {
    pub kind: DupKind,
    pub media: MediaType,
    pub members: Vec<AssetSummary>,
    /// The pairwise signal that linked the group — the explanation (§4.3).
    pub signal: String,
    /// A suggested "keep" (highest resolution / most-permissive / largest); the user disposes.
    pub suggested_keep: AssetId,
}

/// Accept or reject one auto-suggested tag (the one-action lifecycle, §1.4). Accept promotes the
/// suggestion to a confirmed tag; reject records a negative so re-analysis won't re-suggest it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SuggestionReview {
    pub asset: AssetId,
    pub tag: String,
    pub action: ReviewAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    Accept,
    Reject,
}

/// Flag or unflag one asset as a favourite (issue #63). Reversible; the state lives in the asset
/// `flags` bitset, so it survives re-scans and analysis.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct FavoriteRequest {
    pub asset: AssetId,
    pub favorite: bool,
}

// ── collections / smart folders (phase 4 Reach; PRODUCT_SPEC §3, §6.4) ────────────────────────

/// Two kinds of set. A **manual** collection holds an explicit, hand-curated member list. A
/// **smart** folder holds a saved query and resolves *live* — its members are whatever currently
/// matches, so a smart folder like *safe-to-ship* stays correct as the library changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionKind {
    #[default]
    Manual,
    Smart,
}

impl CollectionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CollectionKind::Manual => "manual",
            CollectionKind::Smart => "smart",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(CollectionKind::Manual),
            "smart" => Some(CollectionKind::Smart),
            _ => None,
        }
    }
}

/// A collection or smart folder record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collection {
    pub id: CollectionId,
    pub name: String,
    pub kind: CollectionKind,
    /// The saved query backing a smart folder (live set); `None` for a manual collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
    /// Member count: exact for a manual collection; the current match count for a smart folder when
    /// it was computed, else `None` (list views may skip the per-folder query for cheapness).
    #[serde(default)]
    pub count: Option<u64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Create a collection. A smart folder must carry a `query`; a manual collection ignores it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewCollection {
    pub name: String,
    #[serde(default)]
    pub kind: CollectionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
}

/// Patch a collection: rename and/or (smart folders) replace the saved query. Absent fields are
/// left unchanged.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateCollection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
}

/// Add/remove members of a **manual** collection (a smart folder's membership is query-driven and
/// cannot be edited directly).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CollectionMembers {
    #[serde(default)]
    pub add: Vec<AssetId>,
    #[serde(default)]
    pub remove: Vec<AssetId>,
}

// ── export / manifests (phase 4 Reach; PRODUCT_SPEC §6.4 — "export a manifest") ───────────────

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
