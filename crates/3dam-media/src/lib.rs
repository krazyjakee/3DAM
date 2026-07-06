//! `dam-media` — format detection and the `MediaHandler` trait (tech-spec 04).
//!
//! Phase 1 implements only the cheapest tier: **detection by file extension**. The richer
//! `extract_metadata` (cheap header scan) / `thumbnail` / `extract_features` (expensive) split
//! lands with the analysis pipeline (tech-spec 05); the trait is sketched here so handlers slot in.

use dam_api::dto::MediaType;
use std::path::Path;

/// What detection yields for a file: its media class and a concrete format tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detected {
    pub media: MediaType,
    /// Concrete format, lowercase (`wav`, `png`, `gltf`, …).
    pub format: String,
}

/// A handler for one media class. Phase 1 uses only `detect`; the rest are the cost-tiered
/// extraction points (tech-spec 04) filled in later.
pub trait MediaHandler: Send + Sync {
    fn media(&self) -> MediaType;
    /// Cheap: can this handler claim the file (by extension / magic)?
    fn detect(&self, path: &Path) -> Option<Detected>;
}

/// Lowercased final extension of a path, if any.
fn ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// The extension → (media, format) table for phase-1 detection. Kept in one place so the
/// format-coverage matrix (tech-spec 04 open questions) has a single source of truth.
pub fn detect(path: &Path) -> Option<Detected> {
    let e = ext(path)?;
    let media = match e.as_str() {
        // audio
        "wav" | "flac" | "mp3" | "ogg" | "oga" | "opus" | "aiff" | "aif" | "m4a" | "aac"
        | "wma" | "it" | "xm" | "mod" | "s3m" => MediaType::Audio,
        // image
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tga" | "tiff" | "tif" | "webp" | "psd"
        | "exr" | "hdr" | "dds" | "ktx" | "ktx2" | "svg" => MediaType::Image,
        // 3d models
        "gltf" | "glb" | "fbx" | "obj" | "stl" | "ply" | "dae" | "3ds" | "blend" | "usd"
        | "usdz" | "usda" | "usdc" => MediaType::Model,
        _ => return None,
    };
    // Normalise a few aliases to a canonical format tag.
    let format = match e.as_str() {
        "jpeg" => "jpg",
        "tif" => "tiff",
        "aif" => "aiff",
        "oga" => "ogg",
        other => other,
    }
    .to_string();
    Some(Detected { media, format })
}
