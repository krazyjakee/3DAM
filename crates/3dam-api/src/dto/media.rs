//! Media and licensing value types — the shared vocabulary (`MediaType`, the content handoff,
//! the rights block) that every other section is written in.

use super::common::ContentByteStream;
use super::query::QueryRequest;
use crate::id::{AssetId, CollectionId};
use serde::{Deserialize, Serialize};

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
}
