use crate::{cache, content::fetch_asset, credentials, paths, reliability};
use dam_api::dto::{Asset, AssetContent, ConvertReport, JobState, MediaType};
use dam_api::event::LibraryEvent;
use dam_api::id::{AssetId, ContentHash, JobId};
use dam_api::LibError;
use dam_store::Store;
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;

/// Clamp for a thumbnail's long edge (tech-spec 04 §6.4). Small enough that generation stays cheap
/// and the cache stays compact; large enough for a crisp inspector preview.
pub(super) const THUMB_MIN_EDGE: u32 = 16;
pub(super) const THUMB_MAX_EDGE: u32 = 1024;
/// One hint can enqueue at most this many distinct derivatives. The larger scan cap prevents a
/// duplicate-only request from consuming unbounded CPU while still allowing useful deduplication.
pub(super) const PREFETCH_INPUT_CAP: usize = 512;
pub(super) const PREFETCH_INPUT_SCAN_CAP: usize = PREFETCH_INPUT_CAP * 4;
/// Pending work across overlapping hints is bounded independently of the number of callers.
pub(super) const PREFETCH_QUEUE_CAP: usize = 1024;
pub(super) const PREFETCH_PEER_CAP: usize = 16;

/// Content-keyed cache path for an asset's thumbnail derivative. The cache lives under
/// `<data_dir>/cache/thumbnails/<key>-<edge>[<variant>].png`, keyed by content hash (falling back to
/// the asset id) so identical bytes share one derivative. Model thumbnails carry a renderer-version
/// suffix so a shader/framing bump invalidates only that slice; images keep the bare `{key}-{edge}`.
fn thumb_cache_path(data_dir: &Path, asset: &Asset, max_edge: u32) -> PathBuf {
    thumbnail_cache_path(
        data_dir,
        &asset.summary.id,
        asset.hash,
        asset.summary.media,
        max_edge,
    )
}

/// The content-keyed cache path for an asset's thumbnail at `max_edge`, built from its parts. Model
/// thumbnails carry a renderer-version suffix so a shader/framing bump invalidates only that slice;
/// images keep the bare `{key}-{edge}` name. Shared by [`thumb_cache_path`] and the background
/// pipeline's "is this already warm?" check (issue #71), so the two never drift.
pub(crate) fn thumbnail_cache_path(
    data_dir: &Path,
    id: &AssetId,
    hash: Option<ContentHash>,
    media: MediaType,
    max_edge: u32,
) -> PathBuf {
    let key = hash.map(|h| h.to_hex()).unwrap_or_else(|| id.to_string());
    let variant = thumbnail_variant(media);
    data_dir
        .join("cache")
        .join("thumbnails")
        .join(format!("{key}-{max_edge}{variant}.png"))
}

/// A cheap thumbnail cache probe — a plain file read, no source access. `Some` is the fast path
/// that lets an already-rendered thumbnail skip the bounded background pool entirely.
pub(super) fn thumb_cache_lookup(
    cache: &cache::Controller,
    data_dir: &Path,
    asset: &Asset,
    max_edge: u32,
) -> Option<AssetContent> {
    cache
        .read(
            &thumb_cache_path(data_dir, asset, max_edge),
            cache::Tier::Thumbnail,
        )
        .map(png_content)
}

/// Render (or read from cache) a downscaled PNG thumbnail for an asset — a raster downscale for
/// images, a wgpu turntable render for 3D models (when the `render` feature is on). Pure/blocking —
/// runs inside a blocking closure (the bounded `bg_pool` on a cache miss).
pub(super) fn gen_thumbnail(
    cache: &cache::Controller,
    data_dir: &Path,
    store: &Store,
    secrets: &credentials::SecretVault,
    asset: &Asset,
    max_edge: u32,
) -> Result<AssetContent, LibError> {
    if let Some(hit) = thumb_cache_lookup(cache, data_dir, asset, max_edge) {
        return Ok(hit); // cache hit → no source access at all
    }
    let cache_path = thumb_cache_path(data_dir, asset, max_edge);

    // Cache miss: resolve the source file (in place for local, downloaded for remote). `fetch`
    // guards `..` traversal out of the source root.
    let fetched = fetch_asset(store, secrets, asset, &paths::scratch_dir(data_dir))?;
    let det = dam_media::Detected {
        media: asset.summary.media,
        format: asset.summary.format.clone(),
    };
    let bytes = render_thumbnail_bytes(fetched.path(), &det, max_edge)?;

    cache.publish(&cache_path, &bytes, cache::Tier::Thumbnail);
    Ok(png_content(bytes))
}

/// Produce PNG thumbnail bytes for an asset. Images (and any raster derivative) go through the
/// `dam-media` handler; 3D models go through the wgpu renderer when the `render` feature is on,
/// and otherwise return `Unsupported` so the UI falls back to the honest typed tile.
fn render_thumbnail_bytes(
    path: &Path,
    det: &dam_media::Detected,
    max_edge: u32,
) -> Result<Vec<u8>, LibError> {
    match det.media {
        MediaType::Model => render_model_thumbnail(path, det, max_edge),
        _ => dam_media::render_thumbnail(path, det, max_edge)
            .map(|t| t.bytes)
            .map_err(map_handler_err),
    }
}

/// Cache-key suffix distinguishing thumbnail variants that can change independently of the source
/// bytes. Only 3D renders carry one (keyed to the renderer version); images return an empty suffix.
#[cfg(feature = "render")]
fn thumbnail_variant(media: MediaType) -> String {
    if media == MediaType::Model {
        return format!("-r{}", dam_render::RENDER_VERSION);
    }
    String::new()
}

#[cfg(not(feature = "render"))]
fn thumbnail_variant(_media: MediaType) -> String {
    String::new()
}

/// Render a 3D model to a PNG turntable thumbnail. Fail-soft: every failure — no GPU/software
/// adapter, an unsupported model format, empty geometry, a decode fault — maps to `Unsupported`
/// (a 415), which the web/GUI grid renders as the honest typed tile rather than an error.
#[cfg(feature = "render")]
fn render_model_thumbnail(
    path: &Path,
    det: &dam_media::Detected,
    max_edge: u32,
) -> Result<Vec<u8>, LibError> {
    dam_render::render_model_thumbnail_png(path, &det.format, max_edge).map_err(|e| {
        // Environmental faults (no adapter, readback) are worth a log line; per-asset faults aren't.
        if matches!(
            e,
            dam_render::RenderError::NoAdapter
                | dam_render::RenderError::Device(_)
                | dam_render::RenderError::Readback(_)
        ) {
            tracing::warn!("3D thumbnail render unavailable: {e}");
        }
        LibError::Unsupported(e.to_string())
    })
}

#[cfg(not(feature = "render"))]
fn render_model_thumbnail(
    _path: &Path,
    det: &dam_media::Detected,
    _max_edge: u32,
) -> Result<Vec<u8>, LibError> {
    Err(LibError::Unsupported(format!(
        "{} previews render client-side (WASM island); server 3D thumbnails need the `render` feature",
        det.media.as_str()
    )))
}

pub(super) fn preview_content(bytes: Vec<u8>) -> AssetContent {
    AssetContent {
        bytes,
        content_type: "model/x-dam-preview".to_string(),
        format: "dmsh".to_string(),
        media: MediaType::Model,
    }
}

pub(super) struct ModelDerivativeContent {
    pub(super) thumbnail: Option<Vec<u8>>,
    pub(super) preview: Vec<u8>,
}

#[cfg(feature = "render")]
fn model_preview_cache_path(data_dir: &Path, asset: &Asset) -> PathBuf {
    let key = asset
        .hash
        .map(|h| h.to_hex())
        .unwrap_or_else(|| asset.summary.id.to_string());
    data_dir
        .join("cache")
        .join("previews")
        .join(format!("{key}-p{}.dmsh", dam_render::PREVIEW_VERSION))
}

#[cfg(feature = "render")]
pub(super) fn model_preview_cache_lookup(
    cache: &cache::Controller,
    data_dir: &Path,
    asset: &Asset,
) -> Option<AssetContent> {
    cache
        .read(
            &model_preview_cache_path(data_dir, asset),
            cache::Tier::Preview,
        )
        .map(preview_content)
}

#[cfg(not(feature = "render"))]
pub(super) fn model_preview_cache_lookup(
    _cache: &cache::Controller,
    _data_dir: &Path,
    _asset: &Asset,
) -> Option<AssetContent> {
    None
}

/// Generate both model derivatives from one fetch + Assimp decode. Interactive requests,
/// prefetch, and hosted warming share the same keyed flight around this function.
#[cfg(feature = "render")]
pub(super) fn gen_model_derivatives(
    cache: &cache::Controller,
    data_dir: &Path,
    store: &Store,
    secrets: &credentials::SecretVault,
    asset: &Asset,
    edge: u32,
) -> Result<ModelDerivativeContent, LibError> {
    let thumbnail_path = thumb_cache_path(data_dir, asset, edge);
    let preview_path = model_preview_cache_path(data_dir, asset);
    let thumbnail_hit = cache.read(&thumbnail_path, cache::Tier::Thumbnail);
    let preview_hit = cache.read(&preview_path, cache::Tier::Preview);
    if let (Some(thumbnail), Some(preview)) = (thumbnail_hit.as_ref(), preview_hit.as_ref()) {
        return Ok(ModelDerivativeContent {
            thumbnail: Some(thumbnail.clone()),
            preview: preview.clone(),
        });
    }
    cache.record_model_derivative_generation();
    let fetched = fetch_asset(store, secrets, asset, &paths::scratch_dir(data_dir))?;
    let derivatives = dam_render::model_derivatives(fetched.path(), &asset.summary.format, edge)
        .map_err(|error| LibError::Unsupported(error.to_string()))?;
    if preview_hit.is_none() {
        cache.publish(&preview_path, &derivatives.preview, cache::Tier::Preview);
    }
    let generated_thumbnail = match derivatives.thumbnail {
        Ok(bytes) => {
            if thumbnail_hit.is_none() {
                cache.publish(&thumbnail_path, &bytes, cache::Tier::Thumbnail);
            }
            Some(bytes)
        }
        Err(error) => {
            tracing::warn!(%error, "3D thumbnail warm failed; preview remains available");
            None
        }
    };
    Ok(ModelDerivativeContent {
        thumbnail: thumbnail_hit.or(generated_thumbnail),
        preview: preview_hit.unwrap_or(derivatives.preview),
    })
}

#[cfg(not(feature = "render"))]
pub(super) fn gen_model_derivatives(
    _cache: &cache::Controller,
    _data_dir: &Path,
    _store: &Store,
    _secrets: &credentials::SecretVault,
    _asset: &Asset,
    _edge: u32,
) -> Result<ModelDerivativeContent, LibError> {
    Err(LibError::Unsupported(
        "model derivatives need the server `render` feature".into(),
    ))
}

pub(super) fn png_content(bytes: Vec<u8>) -> AssetContent {
    AssetContent {
        bytes,
        content_type: "image/png".to_string(),
        format: "png".to_string(),
        media: MediaType::Image,
    }
}

/// Map a media-handler fault onto the service error model (tech-spec 03 §5). `Unsupported` becomes
/// a 415 so the web thumbnail falls back to the honest typed tile.
fn map_handler_err(e: dam_media::HandlerError) -> LibError {
    match e {
        dam_media::HandlerError::Unsupported(s) => LibError::Unsupported(s),
        other => LibError::Internal(other.to_string()),
    }
}

/// Delete every cached derivative keyed to one asset — its thumbnail PNGs (across edges + renderer
/// variants) and its 3D preview blob — returning how many files were removed. Both cache tiers are
/// flat and every entry is named `{key}-…`, so a prefix match cleanly scopes deletion to this asset's
/// slice without disturbing others. Best-effort per file (fail-soft, DESIGN_GUIDELINES §2).
pub(super) fn purge_asset_cache(cache: &cache::Controller, key: &str) -> u64 {
    let prefix = format!("{key}-");
    cache.remove_prefix(cache::Tier::Thumbnail, &prefix)
        + cache.remove_prefix(cache::Tier::Preview, &prefix)
}

/// Emit a job's current progress as a `JobProgress` event (best-effort; a dropped read is skipped).
/// Shared by the scan and analyse job loops.
pub(crate) fn emit_progress(store: &Store, events: &broadcast::Sender<LibraryEvent>, job: &JobId) {
    match store.get_job_summary(job) {
        Ok(summary) => {
            reliability::publish_event(
                events,
                LibraryEvent::JobProgress(summary),
                "publish job progress",
            );
        }
        Err(error) => reliability::retryable_store_write(
            Err(error),
            "read job progress for broadcast",
            Some(job),
            None,
        ),
    }
}

pub(super) fn convert_warnings(report: &ConvertReport) -> Vec<String> {
    let mut warnings = Vec::new();
    if report.failed > 0 {
        warnings.push(format!(
            "{} item(s) failed; inspect the itemized report",
            report.failed
        ));
    }
    if report.collisions > 0 {
        warnings.push(format!("{} output collision(s)", report.collisions));
    }
    if report.unsupported > 0 {
        warnings.push(format!("{} unsupported item(s)", report.unsupported));
    }
    warnings
}

pub(super) fn convert_summary(report: &ConvertReport, state: JobState) -> String {
    if state == JobState::Cancelled {
        format!(
            "Convert cancelled after processing {} item(s)",
            report.items.len()
        )
    } else if report.dry_run {
        format!("Planned {} item(s)", report.items.len())
    } else {
        format!(
            "Converted {} of {} item(s)",
            report.done,
            report.items.len()
        )
    }
}
