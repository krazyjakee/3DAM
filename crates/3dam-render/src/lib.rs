//! `dam-render` — the wgpu renderer (tech-spec 06, [ADR 0002](../../docs/adr/0002-3d-render-crate-boundary.md)).
//!
//! Owns GPU work (headless render-to-PNG thumbnails and, later, the multi-view render feeding shape
//! embeddings) but **never** windowing — the desktop shell is a webview (`3dam-desktop`, ADR 0013)
//! whose 3D viewer is the `dam-viewer` WASM island. Renders headless with no surface and falls back
//! to a software rasteriser
//! (Mesa lavapipe/llvmpipe) on GPU-less hosts, as validated by `spikes/headless-render/` (ADR 0001).
//!
//! Models are imported via **Assimp** (`russimp-ng`, statically linked — [ADR 0011](../../docs/adr/0011-assimp-import-backend.md))
//! for the full professional format range (FBX, OBJ, DAE, 3DS, glTF/GLB, PLY, STL, blend, …) and
//! rendered with a textured metallic-roughness PBR pipeline.

mod blend;
mod camera;
mod model;
mod preview;
mod renderer;

pub use camera::{
    canonical_direction as framing_direction, clip_planes as framing_clip_planes,
    fit_distance as framing_fit_distance, framing_convention, FramingConvention,
};
pub use renderer::Renderer;

use std::path::Path;
use std::sync::OnceLock;

/// A render fault. Always fail-soft at the call site: the caller degrades to the honest typed tile
/// (never aborts a scan or convert batch). `Clone` so the lazily-cached global init error can be
/// handed to every caller.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RenderError {
    #[error("no GPU or software adapter available (headless fallback ladder exhausted)")]
    NoAdapter,
    #[error("gpu device request failed: {0}")]
    Device(String),
    #[error("model decode failed: {0}")]
    Decode(String),
    #[error("model has no renderable triangle geometry")]
    EmptyMesh,
    #[error("unsupported model format for rendering: {0}")]
    UnsupportedFormat(String),
    #[error("gpu readback failed: {0}")]
    Readback(String),
    #[error("png encode failed: {0}")]
    Encode(String),
}

/// Whether this format can be rendered to a thumbnail. Assimp covers the professional interchange
/// range; only the USD family (`usd`/`usda`/`usdc`/`usdz`) is out — there is no Assimp USD importer,
/// so those degrade to the typed tile until a dedicated USD path lands (tech-spec 06 follow-up).
pub fn supports_format(format: &str) -> bool {
    matches!(
        format,
        "gltf"
            | "glb"
            | "fbx"
            | "obj"
            | "stl"
            | "ply"
            | "dae"
            | "3ds"
            | "blend"
            | "x"
            | "lwo"
            | "lws"
            | "ase"
            | "ms3d"
            | "off"
            | "dxf"
    )
}

/// The process-wide renderer. Device creation (adapter ladder + pipelines) is the dominant cost and
/// wgpu handles are `Send + Sync`, so we build it once and share it across all thumbnail renders.
/// A failed init is cached too — a GPU-less-and-no-software host shouldn't retry the ladder per call.
fn shared() -> Result<&'static Renderer, RenderError> {
    static RENDERER: OnceLock<Result<Renderer, RenderError>> = OnceLock::new();
    match RENDERER.get_or_init(|| pollster::block_on(Renderer::new())) {
        Ok(r) => Ok(r),
        Err(e) => Err(e.clone()),
    }
}

/// Render a model file to a square PNG thumbnail of `size`×`size`. Blocking/CPU+GPU work — call
/// from `spawn_blocking`, never on an async executor. Fail-soft: any error means the UI should fall
/// back to the typed tile.
pub fn render_model_thumbnail_png(
    path: &Path,
    format: &str,
    size: u32,
) -> Result<Vec<u8>, RenderError> {
    if !supports_format(format) {
        return Err(RenderError::UnsupportedFormat(format.to_string()));
    }
    // A modern `.blend` can't be GPU-rendered (Assimp can't decode its geometry), but artist-saved
    // files usually embed Blender's own preview image. Use that as the thumbnail when present — it's
    // instant and avoids an expensive (and, on a large `.blend`, unreasonable) full-scene render.
    if format.eq_ignore_ascii_case("blend") {
        if let Some(png) = blend::embedded_thumbnail_png(path) {
            return Ok(png);
        }
    }
    let model = model::load(path)
        .map_err(RenderError::Decode)?
        .ok_or(RenderError::EmptyMesh)?;
    shared()?.render_png(&model, size)
}

/// Bump when the renderer output changes (shaders, framing, material handling, import backend), so
/// cached thumbnails from an older renderer are invalidated (the engine folds this into the cache
/// key). v2: Assimp import backend + textured metallic-roughness PBR. v3: FlipUVs — corrects the
/// vertically-flipped textures (Assimp emits lower-left UVs; the pipeline samples top-left). v4:
/// game-asset companion-map discovery + sibling texture folders, dropped uniform-constant vertex
/// colours (export junk that tinted albedo), shininess→roughness, and SSAA thumbnails. v5: material
/// transparency — glTF `alphaMode`/opacity/transmission render as alpha-blended glass, not opaque.
/// v6: canonical framing v1 shared with the interactive viewer (aspect-aware sphere fit).
pub const RENDER_VERSION: u32 = 6;

/// Decode a model to the compact self-contained `DMSH` blob the browser 3D island uploads
/// directly (see [`preview`]). Reuses the **same Assimp decode** as the turntable thumbnail, so the
/// interactive viewer covers the full professional format range with textures — but this path is
/// **CPU-only** (it never touches the GPU), so previews work even on hosts with no GPU/software
/// adapter, unlike [`render_model_thumbnail_png`]. Blocking/CPU work — call from `spawn_blocking`.
/// Fail-soft: an unsupported format or empty geometry maps to an error the caller degrades on.
pub fn model_preview_blob(path: &Path, format: &str) -> Result<Vec<u8>, RenderError> {
    if !supports_format(format) {
        return Err(RenderError::UnsupportedFormat(format.to_string()));
    }
    let model = model::load(path)
        .map_err(RenderError::Decode)?
        .ok_or(RenderError::EmptyMesh)?;
    Ok(preview::serialize(&model))
}

/// Both model derivatives from one Assimp import. Background warming asks for the thumbnail and
/// interactive preview together; decoding independently would double the dominant CPU/I/O work.
/// A GPU thumbnail failure is isolated so the CPU-only preview can still be published.
pub struct ModelDerivatives {
    pub thumbnail: Result<Vec<u8>, RenderError>,
    pub preview: Vec<u8>,
}

pub fn model_derivatives(
    path: &Path,
    format: &str,
    size: u32,
) -> Result<ModelDerivatives, RenderError> {
    if !supports_format(format) {
        return Err(RenderError::UnsupportedFormat(format.to_string()));
    }
    let model = model::load(path)
        .map_err(RenderError::Decode)?
        .ok_or(RenderError::EmptyMesh)?;
    let preview = preview::serialize(&model);
    let thumbnail = if format.eq_ignore_ascii_case("blend") {
        blend::embedded_thumbnail_png(path)
            .map(Ok)
            .unwrap_or_else(|| shared().and_then(|renderer| renderer.render_png(&model, size)))
    } else {
        shared().and_then(|renderer| renderer.render_png(&model, size))
    };
    Ok(ModelDerivatives { thumbnail, preview })
}

/// Bump when the `DMSH` blob layout or its texture handling changes, so cached previews from an
/// older serializer are invalidated (the engine folds this into the preview cache key). v2: FlipUVs
/// in the shared decode — corrects the vertically-flipped textures in the interactive viewer. v3:
/// companion-map discovery + sibling texture folders + dropped uniform-constant vertex colours (the
/// shared decode now feeds the viewer the same faithful materials as the thumbnail), and the raised
/// preview texture cap. v4: `DMSH` v2 — per-material `alpha_mode`/`alpha_cutoff` so the viewer
/// renders transparency (glass) instead of forcing every surface opaque.
pub const PREVIEW_VERSION: u32 = 4;
