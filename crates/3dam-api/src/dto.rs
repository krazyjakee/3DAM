//! Request/response DTOs (tech-spec 03 §3–§4). These are the same structs used as Rust args
//! and JSON bodies. Only the phase-1 slice is modelled here; the rest slot in as their areas land.

use crate::id::{AssetId, CollectionId, CommentId, ContentHash, JobId, SourceId};
use crate::page::PageParams;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::pin::Pin;

/// Materialised preview consumers are deliberately capped. The HTTP content transport uses
/// [`AssetContentStream`] instead, so a video may be much larger without being held in memory.
pub const MAX_MATERIALIZED_CONTENT_BYTES: u64 = 256 * 1024 * 1024;

/// A bounded, cancellation-aware byte stream. Producers should yield modest chunks and stop when
/// the consumer drops the stream; transports can then apply their normal backpressure.
pub type ContentByteStream =
    Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>, crate::LibError>> + Send>>;

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

/// Metadata needed to decide HTTP range and validator semantics before any bytes are opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetContentMetadata {
    pub len: u64,
    pub content_type: String,
    pub format: String,
    pub media: MediaType,
    /// A quoted strong entity tag when the catalog has a content hash. `None` means an `If-Range`
    /// validator cannot be proved current and the caller must send the complete representation.
    pub etag: Option<String>,
}

/// Inclusive byte bounds within one representation. Keeping resolution of HTTP's suffix/open
/// forms in the server leaves this transport-neutral and makes an invalid range unrepresentable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRange {
    first: u64,
    last: u64,
}

impl ContentRange {
    /// Construct inclusive bounds, rejecting reversed bounds and the singular `0..=u64::MAX`
    /// interval whose mathematical length cannot be represented by `u64`.
    pub fn new(first: u64, last: u64) -> Option<Self> {
        (first <= last && (last - first).checked_add(1).is_some()).then_some(Self { first, last })
    }

    pub fn first(self) -> u64 {
        self.first
    }

    pub fn last(self) -> u64 {
        self.last
    }

    pub fn len(self) -> u64 {
        self.last - self.first + 1
    }

    /// A constructed inclusive range always contains at least one byte.
    pub const fn is_empty(self) -> bool {
        false
    }
}

/// A content representation opened for bounded streaming. `metadata.len` is the complete
/// representation length; `range` names the exact bytes this stream will produce.
pub struct AssetContentStream {
    pub metadata: AssetContentMetadata,
    pub range: ContentRange,
    pub bytes: ContentByteStream,
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

/// Three-state field for a license patch: absent leaves the column alone, `null` clears it back to
/// unknown, and a value sets it.
///
/// Two states are not enough. A bulk edit like "stamp this holder across 500 assets without
/// touching their license ids" is inexpressible if absence means clear, and "I was wrong, this
/// isn't actually CC-BY" is inexpressible if absence means keep.
pub type Patch<T> = Option<Option<T>>;

/// Deserialize a present-but-possibly-null field into `Some(_)`; `#[serde(default)]` supplies the
/// `None` that means "key absent".
fn patch<'de, D, T>(de: D) -> Result<Patch<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

/// A patch over one asset's rights block (issue #106).
///
/// `status` is deliberately not a field. Tech-spec 02 §5 derives `license_status` from the id and
/// rights at write time, and ADR 0009 §1 sets it **only** when a license is actually known. If a
/// client could send it, "permissive" would be assertable without naming a license — precisely the
/// silent-permissive failure the rights model exists to prevent.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LicenseInput {
    /// SPDX id, or the `Proprietary` / `Custom` sentinels (ADR 0009 §1).
    #[serde(
        default,
        deserialize_with = "patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Patch<String>,
    #[serde(
        default,
        deserialize_with = "patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub commercial: Patch<bool>,
    #[serde(
        default,
        deserialize_with = "patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub modify: Patch<bool>,
    #[serde(
        default,
        deserialize_with = "patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub redistribute: Patch<bool>,
    /// `true` means attribution is *required*.
    #[serde(
        default,
        deserialize_with = "patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub attribution: Patch<bool>,
    #[serde(
        default,
        deserialize_with = "patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub holder: Patch<String>,
    #[serde(
        default,
        deserialize_with = "patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub credit: Patch<String>,
    #[serde(
        default,
        deserialize_with = "patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub url: Patch<String>,
}

impl LicenseInput {
    /// Whether the patch would touch any column at all — an all-absent patch is a no-op that
    /// should still report its selection rather than being rejected.
    pub fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.commercial.is_none()
            && self.modify.is_none()
            && self.redistribute.is_none()
            && self.attribution.is_none()
            && self.holder.is_none()
            && self.credit.is_none()
            && self.url.is_none()
    }
}

/// Apply a rights patch over one explicit or server-resolved selection (issue #106).
/// Explicit ids take precedence, followed by collection, then query — matching `TagEditRequest`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SetLicenseRequest {
    #[serde(default)]
    pub assets: Vec<AssetId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<CollectionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
    pub license: LicenseInput,
    /// Calculate the same authorized effect without changing the catalog.
    #[serde(default)]
    pub dry_run: bool,
}

/// Summary-shaped bulk result: bounded warnings rather than one response row per target.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LicenseEditResult {
    pub matched: u64,
    pub changed: u64,
    /// Post-edit status mix over the changed assets, so a caller can show "42 now permissive,
    /// 3 still unknown" without a follow-up query.
    #[serde(default)]
    pub status: Vec<LicenseStatusCount>,
    #[serde(default)]
    pub warnings: Vec<crate::ItemWarning>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LicenseStatusCount {
    pub status: LicenseStatus,
    pub count: u64,
}

pub const LICENSE_EDIT_EXPLICIT_MAX: usize = 1_000;

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
    /// Which source this row came from — the attribution a [`Visibility`](crate::service::Visibility)
    /// ceiling is evaluated against. Carried on the *summary* (not just the full [`Asset`]) because
    /// `LibraryEvent::AssetAdded` ships a summary, and a restricted subscriber can only be told
    /// about an asset whose source it may reach (issue #42).
    ///
    /// `None` means "unattributed": a payload from a server older than this field, or a peer row
    /// federation has not re-attributed yet. Unattributed is treated as *out* of every restricted
    /// ceiling — the fail-safe direction, since withholding an event costs a stale grid while
    /// leaking one costs a disclosure.
    #[serde(default)]
    pub source_id: Option<SourceId>,
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
    /// The user's free-text note, if one has been written (issue #81). `None` and an empty body are
    /// the same state by construction — clearing a note deletes the row.
    #[serde(default)]
    pub note: Option<Note>,
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
    // ── GPU texture containers (issue #49); None for ordinary rasters ──
    /// The container's own pixel format — `BC7_UNORM`, `R8G8B8A8_SRGB`, … For a DDS or KTX2 this
    /// is the field that actually distinguishes one texture from another: dimensions alone say
    /// nothing about whether a 4 MB file is a BC5 normal map or a BC7 albedo.
    #[serde(default)]
    pub texture_format: Option<String>,
    /// Mip levels stored in the file (1 = just the base image).
    #[serde(default)]
    pub mip_levels: Option<i64>,
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
/// ([ADR 0015](../../../docs/adr/0015-video-decode-backend.md)). Every field is `Option` because
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
    pub state: SuggestionState,
    pub source: String, // auto | user
    pub confidence: Option<f32>,
    /// Concise, persisted reason the analyser proposed this value. User-authored tags have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// Review state presented to users. The store's legacy spelling is `suggested`; the wire and UI
/// call that state `pending` so it cannot be mistaken for an accepted catalog fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionState {
    Pending,
    Confirmed,
    Rejected,
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
    /// Request an exact filtered total even on a continuation page. First pages include the exact
    /// total automatically when omitted; `false` lets internal fan-out/refetch callers suppress
    /// even that count, and `true` explicitly requests it on any page.
    #[serde(default)]
    pub include_total: Option<bool>,
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
    /// Source-relative path prefix (issue #66) — scopes the browse to a folder subtree, **including
    /// everything below it**. The value is the prefix (e.g. `Environment/Rock/`); an empty prefix
    /// matches everything.
    Path,
    /// Source-relative folder, matched **exactly** (issue #66): assets whose immediate parent is
    /// this folder, excluding anything in a deeper subfolder. The value is the same prefix form
    /// [`Path`](Self::Path) takes (`Environment/Rock/`); an empty value means the source root, so
    /// `folder = ""` is the loose files at the top level rather than "everything".
    ///
    /// A separate field rather than an op on `Path` because both readings are legitimate scopes a
    /// user saves into a smart folder, and overloading `Eq` would silently re-scope every folder
    /// filter already stored.
    Folder,
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
    /// Can this source accept an upload *right now* (issue #80)?
    ///
    /// Probed, not inferred from `kind`: a local source can sit on a read-only mount, and a peer is
    /// never writable at all. The destination picker offers only sources where this is true, so the
    /// user learns a share is read-only before choosing files rather than after dropping two
    /// hundred of them. Defaults to `false` so an older server (or any surface that cannot answer)
    /// reads as read-only rather than advertising a write that would fail.
    #[serde(default)]
    pub writable: bool,
    /// Why an otherwise visible source is not an upload destination. In particular, this names a
    /// missing per-source write grant separately from backend/filesystem writability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writable_reason: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AddSource {
    pub kind: SourceKind,
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub options: SourceOptions,
}

impl std::fmt::Debug for AddSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddSource")
            .field("kind", &self.kind)
            // Network URI userinfo may itself contain a password. The parsed SourceConnection has
            // a safe display URI, but this transport DTO has not crossed that boundary yet.
            .field("uri", &"[REDACTED]")
            .field("name", &self.name)
            .field("options", &self.options)
            .finish()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
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
    /// Password (SFTP or SMB). Write-only input; moved into the host secret store before the source
    /// row is created and never returned to clients or stored in the portable catalog.
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

impl std::fmt::Debug for SourceOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceOptions")
            .field("watch", &self.watch)
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "passphrase",
                &self.passphrase.as_ref().map(|_| "[REDACTED]"),
            )
            .field("domain", &self.domain)
            .field("port", &self.port)
            .finish()
    }
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

/// A durable, safe-to-render result of a background job. `route` is an application-relative report
/// or asset route, never a filesystem path; convert/export can populate these when their async job
/// manifests land without changing the history contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobArtifact {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    pub progress: Progress,
    #[serde(default)]
    pub error: Option<String>,
    /// Human-readable terminal summary. Unlike `error`, this describes completed work and remains
    /// available in job history after the live progress event has passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Recoverable, per-item degradation. A done job with warnings is a partial success, not an
    /// unqualified success and not a hard failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub result_artifacts: Vec<JobArtifact>,
    /// Structured terminal output, persisted so history can reopen the complete report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<JobResult>>,
    /// Millisecond Unix timestamps persisted with the job row.
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// The account/token/automation identity which started the job, when it can be attributed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<String>,
    /// Every source this job touches, recorded when the job is created (issue #42). `Progress.current`
    /// names a live file path, so a restricted identity may only observe a job whose sources are
    /// *all* within its ceiling — see
    /// [`Visibility::allows_job`](crate::service::Visibility::allows_job).
    ///
    /// Empty means "unattributed" (a pre-attribution job row, or a server older than this field) and
    /// is therefore reachable only at `Visibility::Full`.
    #[serde(default)]
    pub sources: Vec<SourceId>,
    /// Collection grants used by a restricted job.
    #[serde(default)]
    pub collections: Vec<CollectionId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "report", rename_all = "snake_case")]
pub enum JobResult {
    Convert(ConvertReport),
    Export(ExportReport),
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

/// Convert request whose destination is allocated by the serving host. This transport-facing shape
/// deliberately has no path: a browser cannot accidentally nominate a path on its own machine (or
/// use an artifact endpoint as an arbitrary server-file reader). The server turns it into an
/// ordinary [`ConvertRequest`] under its private artifact root.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagedConvertRequest {
    pub inputs: Vec<AssetId>,
    pub target: ConvertTarget,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub on_collision: CollisionRule,
}

impl ManagedConvertRequest {
    pub fn with_output_dir(self, output_dir: String) -> ConvertRequest {
        ConvertRequest {
            inputs: self.inputs,
            target: self.target,
            output_dir,
            dry_run: self.dry_run,
            on_collision: self.on_collision,
        }
    }
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
    /// 3D container transcode (issue #49, tech-spec 08 §3.3).
    Model {
        /// `glb`, `gltf`, or `obj` in v1. Multi-file targets publish their `.bin`/`.mtl`
        /// companions with the primary; FBX/USD encode are post-v1.
        format: String,
        /// Optimise the mesh while transcoding: merge redundant materials and meshes, drop
        /// degenerate faces, and re-join the vertices a merge duplicates. glTF-family targets
        /// additionally encode geometry with `KHR_draco_mesh_compression` (issue #49, §3.3).
        ///
        /// Off by default, and it stays a separate knob from `format` because the two are
        /// independently useful — a container transcode is expected to preserve what it was given,
        /// while optimisation is deliberately lossy in *structure*: the node graph is collapsed, so
        /// names and hierarchy a downstream tool keyed on may not survive. Nothing about it is
        /// lossy for the original, which convert never touches (§5.1).
        #[serde(default)]
        optimize: bool,
    },
}

impl ConvertTarget {
    pub fn media(&self) -> MediaType {
        match self {
            ConvertTarget::Image { .. } => MediaType::Image,
            ConvertTarget::Audio { .. } => MediaType::Audio,
            ConvertTarget::Model { .. } => MediaType::Model,
        }
    }
    /// The concrete output format token (drives the output extension).
    pub fn format(&self) -> &str {
        match self {
            ConvertTarget::Image { format, .. } => format,
            ConvertTarget::Audio { format } => format,
            ConvertTarget::Model { format, .. } => format,
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

// ── upload (issue #80, tech-spec 08 §5.1) ───────────────────────────────────

/// How an upload resolves a name that is already taken.
///
/// Deliberately **not** [`CollisionRule`]: this enum has no `Overwrite` arm, and that absence is
/// the feature. Upload is the one sanctioned path that writes inside a source tree, and it stays
/// non-destructive because clobbering an existing file is not expressible in the API at all — no
/// flag, no admin toggle, no request field can produce it (tech-spec 08 §5.1). Sharing
/// `CollisionRule` would have put an `Overwrite` variant one typo away from a destroyed original.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadCollision {
    /// The name is taken → the upload fails and writes nothing. The safe default.
    #[default]
    Fail,
    /// Disambiguate: `brick.png` → `brick-1.png`, `brick-2.png`, …
    Suffix,
    /// Leave the existing file alone and report the upload as skipped.
    Skip,
}

/// Write one file into a registered source at an explicit user request.
///
/// The bytes travel out-of-band (an HTTP body, or a staged file for an in-process caller) rather
/// than in this struct, so a multi-gigabyte asset is never held in memory or serialised through a
/// DTO.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadRequest {
    /// Destination source. Must be writable; a federated peer is always rejected.
    pub source: SourceId,
    /// Source-relative destination directory. Empty means the source root. Created if missing.
    #[serde(default)]
    pub folder: String,
    /// The bare filename to create. Validated against the path-safety battery; rejected, never
    /// silently sanitised, so the user always gets the name they asked for or a clear error.
    pub name: String,
    #[serde(default)]
    pub collision: UploadCollision,
}

/// What became of one uploaded file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadOutcome {
    /// The source-relative path actually written — differs from the requested name under
    /// [`UploadCollision::Suffix`], which is why the client is told rather than left to guess.
    pub path: String,
    /// True when [`UploadCollision::Skip`] left an existing file in place. Nothing was written.
    pub skipped: bool,
    pub size: u64,
    /// The catalogued asset, when the file was one 3DAM understands.
    #[serde(default)]
    pub asset: Option<AssetId>,
    /// Why the file was stored but not catalogued (an unsupported format). The file is on disk
    /// either way: the user asked to put it somewhere, so refusing the write would be the wrong
    /// answer — but silently omitting it from the catalog would be a worse one.
    #[serde(default)]
    pub uncatalogued_reason: Option<String>,
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
    /// Internal federation hop marker. A receiving peer warms its local matches but does not relay
    /// the hint again, preventing cycles between mutually registered libraries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub relay: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    /// Opaque continuation returned by the previous duplicate-review page.
    #[serde(default)]
    pub after: Option<crate::Cursor>,
    /// Which durable review queue to show. Pending is the default working queue; reviewed groups
    /// remain reachable so a decision can be inspected or reopened after refresh.
    #[serde(default)]
    pub review: DupReviewFilter,
}

fn default_dup_limit() -> u32 {
    24
}

/// Hard response and work bounds for duplicate reads. Callers may request less, never more.
pub const DUP_GROUP_PAGE_MAX: u32 = 100;
pub const DUP_GROUP_MEMBER_MAX: usize = 100;
pub const DUP_MEMBERSHIP_ASSET_MAX: usize = 600;

/// Lightweight exact-duplicate lookup for a bounded set of browse rows.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DupMembershipRequest {
    pub assets: Vec<AssetId>,
}

/// Exact-duplicate membership without any member summaries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupMembership {
    pub asset: AssetId,
    /// Stable within this library: the lower-case content hash identifying the group.
    pub group: String,
    /// Visible members in the complete group, including `asset`.
    pub count: u32,
}

/// Continue through the members of one exact duplicate group.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupGroupMembersRequest {
    /// Group key returned on [`DupGroup`].
    pub group: String,
    #[serde(default)]
    pub after: Option<crate::Cursor>,
    #[serde(default = "default_dup_limit")]
    pub limit: u32,
}

/// One duplicate-review member with the comparison context deliberately omitted from ordinary
/// browse summaries. This is still metadata-only: no source bytes are read.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupMember {
    pub asset: AssetSummary,
    pub path: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyzed_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DupReviewState {
    #[default]
    Pending,
    Resolved,
    Dismissed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DupReviewFilter {
    #[default]
    Pending,
    Resolved,
    Dismissed,
    All,
}

/// Persist one review decision. Choosing a keep is non-destructive and leaves the group pending;
/// resolve/dismiss move it out of the default queue, while Pending reopens it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupReviewRequest {
    pub review: String,
    pub state: DupReviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<AssetId>,
    /// Catalog-only removals committed in the same transaction as the review state. `block` is
    /// content-addressed and can therefore remove every exact copy; source files remain untouched.
    #[serde(default)]
    pub removals: Vec<DupReviewRemoval>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupReviewRemoval {
    pub asset: AssetId,
    #[serde(default)]
    pub block: bool,
}

/// A cluster of duplicates for the review view (§4.3). Never auto-deleted — 3DAM only groups.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupGroup {
    pub kind: DupKind,
    pub media: MediaType,
    /// Exact-group key used to continue member pagination. `None` for computed near groups.
    #[serde(default)]
    pub group: Option<String>,
    /// Stable identity for the current group, used by the durable review record.
    pub review: String,
    #[serde(default)]
    pub review_state: DupReviewState,
    pub members: Vec<DupMember>,
    /// Visible members in the complete group. `members` is capped for bounded responses.
    pub total_members: u32,
    /// Continue this group's member list without reloading its first summaries.
    #[serde(default)]
    pub members_cursor: Option<crate::Cursor>,
    /// The pairwise signal that linked the group — the explanation (§4.3).
    pub signal: String,
    /// A suggested "keep" (highest resolution / most-permissive / largest); the user disposes.
    pub suggested_keep: AssetId,
    /// Why the automated choice won. Separate from `signal`, which explains why the members were
    /// grouped rather than why one is preferable to keep.
    pub suggested_keep_reason: String,
    /// A user override, persisted independently of the suggestion. `None` means use the suggestion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen_keep: Option<AssetId>,
}

/// Accept, reject, or undo one auto-suggested tag decision (the one-action lifecycle, §1.4).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SuggestionReview {
    pub asset: AssetId,
    pub tag: String,
    pub action: ReviewAction,
}

/// Add/remove user-authored tags over one explicit or server-resolved selection (issue #121).
/// Explicit ids take precedence, followed by collection, then query (matching export selection).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagEditRequest {
    #[serde(default)]
    pub assets: Vec<AssetId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<CollectionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
    /// Calculate the same authorized delta without changing the catalog.
    #[serde(default)]
    pub dry_run: bool,
}

/// Summary-shaped bulk result: bounded warnings rather than one response row per target.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagEditResult {
    pub matched: u64,
    pub changed: u64,
    pub additions: u64,
    pub removals: u64,
    #[serde(default)]
    pub warnings: Vec<crate::ItemWarning>,
}

pub const TAG_EDIT_EXPLICIT_MAX: usize = 1_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagListRequest {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default = "default_tag_list_limit")]
    pub limit: u32,
}

fn default_tag_list_limit() -> u32 {
    20
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub count: u64,
    /// At least one visible assignment was explicitly user-authored.
    pub manual: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    Accept,
    Reject,
    Undo,
}

/// Flag or unflag one asset as a favourite (issue #63). Reversible; the state lives in the asset
/// `flags` bitset, so it survives re-scans and analysis.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct FavoriteRequest {
    pub asset: AssetId,
    pub favorite: bool,
}

/// A user's free-text annotation on an asset (issue #81) — the "why" that filenames, tags, and
/// extracted metadata cannot carry ("client rejected this variant"; "needs a high-pass before use").
///
/// One editable note per asset, not a thread: it answers *"what should I know about this asset?"*,
/// which is a single durable statement. Time-ordered, authored discussion is a different feature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    /// Exactly what the user typed. Stored verbatim — a client may *render* markdown, but nothing
    /// on the way in or out is allowed to rewrite it.
    pub body: String,
    /// Unix ms of the last edit.
    pub updated_at: i64,
    /// Who last edited it, as a loose identity string (account username, else the token identity).
    /// `None` for a single-user local library, which is the common case today.
    #[serde(default)]
    pub updated_by: Option<String>,
}

/// Set or clear an asset's note. An empty (or whitespace-only) `body` **clears** it: there is no
/// separate delete verb, because "select all, delete, blur" is how a user expects to remove text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteRequest {
    pub body: String,
}

// ── per-asset discussion (issue #82) ──────────────────────────────────────────────────────────
//
// The deliberate counterpart to [`Note`]. A note is **one durable editable annotation** answering
// "what should I know about this asset?"; a discussion is **append-only authored history** answering
// "what did we decide about it?". Both can exist on one asset, but only ever in the multi-user
// posture: discussion is gated on user accounts and is simply absent otherwise, so the single-user
// local library — the common case — never sees two text boxes and has to guess which is which.

/// Who wrote a message.
///
/// `id` is the account id as stored in `library.db`; `display` is resolved at *read* time against
/// `server.db`, across the database boundary. Deleting an account therefore leaves its messages
/// intact and merely unresolved (`display: None` → "deleted user") rather than cascade-deleting
/// them, which would silently rewrite a project's history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommentAuthor {
    pub id: String,
    /// `None` when the account no longer exists, or when nothing resolved it (the embedded engine
    /// has no account store to ask).
    #[serde(default)]
    pub display: Option<String>,
}

/// One message in an asset's discussion thread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub asset: AssetId,
    pub author: CommentAuthor,
    /// Empty once deleted — the row survives as a tombstone so replies keep their parent.
    pub body: String,
    pub created_at: i64,
    /// Non-`None` ⇒ the client shows an "edited" marker.
    #[serde(default)]
    pub edited_at: Option<i64>,
    /// Non-`None` ⇒ a tombstone: the message is gone but the thread stays coherent.
    #[serde(default)]
    pub deleted_at: Option<i64>,
    /// The message this replies to, if any. One level of quoting, not arbitrary nesting — deep
    /// trees are a lot of UI for little value on what is usually a three-message exchange. The
    /// column costs nothing to carry and keeps the option open.
    #[serde(default)]
    pub reply_to: Option<CommentId>,
}

/// Post a message to an asset's thread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewComment {
    pub body: String,
    #[serde(default)]
    pub reply_to: Option<CommentId>,
}

/// Replace a message's text. Author-only; sets `edited_at`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditComment {
    pub body: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_ranges_are_ordered_and_have_representable_lengths() {
        let range = ContentRange::new(10, 15).unwrap();
        assert_eq!(range.first(), 10);
        assert_eq!(range.last(), 15);
        assert_eq!(range.len(), 6);
        assert!(ContentRange::new(15, 10).is_none());
        assert!(ContentRange::new(0, u64::MAX).is_none());
    }

    /// The convert target is the one DTO whose wire shape three independent clients hand-write
    /// (the CLI, `web/src/api/types.ts`, and MCP's untyped tool arguments), so its tag and its
    /// defaults are a contract rather than an implementation detail.
    ///
    /// `optimize` in particular has to be *absent-tolerant*: every request written before it
    /// existed omits it, and those must keep meaning "plain transcode" rather than failing to
    /// deserialise or, worse, silently opting into a structurally lossy encode.
    #[test]
    fn a_model_convert_target_defaults_to_no_optimisation_and_round_trips() {
        let legacy: ConvertTarget =
            serde_json::from_str(r#"{"media":"model","format":"glb"}"#).expect("older wire form");
        assert!(
            matches!(
                legacy,
                ConvertTarget::Model {
                    optimize: false,
                    ..
                }
            ),
            "a request without the field must not opt in: {legacy:?}"
        );

        let opted: ConvertTarget =
            serde_json::from_str(r#"{"media":"model","format":"glb","optimize":true}"#).unwrap();
        assert!(matches!(opted, ConvertTarget::Model { optimize: true, .. }));
        assert_eq!(opted.media(), MediaType::Model);
        assert_eq!(opted.format(), "glb");

        let wire = serde_json::to_value(&opted).unwrap();
        assert_eq!(
            wire["media"], "model",
            "the tag names the media, lowercased"
        );
        assert_eq!(wire["optimize"], true);
        let back: ConvertTarget = serde_json::from_value(wire).unwrap();
        assert!(matches!(back, ConvertTarget::Model { optimize: true, .. }));
    }
}
