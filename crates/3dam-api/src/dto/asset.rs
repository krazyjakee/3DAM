//! Asset shapes: the cheap grid/table summary row, the full inspector record, and the per-media
//! attribute payloads hanging off them.

use super::analysis::Note;
use super::common::SmallMap;
use super::media::{License, LicenseBadge, MediaType, Origin};
use crate::id::{AssetId, CollectionId, ContentHash, SourceId};
use serde::{Deserialize, Serialize};

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
