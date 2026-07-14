//! `dam-core` — the pure engine. `EmbeddedLibrary` implements the `LibraryService` seam over the
//! store, media handlers, and sources (tech-spec 01 §3). No UI, transport, or GPU (ADR 0002).
//! This is the standalone, no-network path; `3dam-server` serves the same object over HTTP/WS.

mod analysis;
mod background;
mod convert;
mod export;
mod federation;
mod paths;
mod scan;
pub mod semantic;
mod watch;

pub use background::PipelinePolicy;
pub use paths::default_data_dir;

use async_trait::async_trait;
use dam_api::admin::{
    CacheTarget, CacheUsage, ClearAnalysisReport, ClearCacheReport, StorageUsage, VacuumReport,
    WipeReport,
};
use dam_api::dto::*;
use dam_api::event::{ChangeKind, LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, CollectionId, ContentHash, JobId, SourceId};
use dam_api::page::{Page, PageParams};
use dam_api::service::{AuthContext, EventStream, LibraryService};
use dam_api::LibError;
use dam_store::Store;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Upper bound on a single preview read (tech-spec 09 §B.3). Preview islands want interactive assets,
/// not arbitrary blobs; a larger file returns a typed error and the UI degrades to metadata. Config-
/// tunable later (ADR 0009 §11 storage knobs); a constant for now.
const MAX_CONTENT_BYTES: u64 = 256 * 1024 * 1024;

/// Clamp for a thumbnail's long edge (tech-spec 04 §6.4). Small enough that generation stays cheap
/// and the cache stays compact; large enough for a crisp inspector preview.
const THUMB_MIN_EDGE: u32 = 16;
const THUMB_MAX_EDGE: u32 = 1024;

/// Thread count for the bounded background-CPU pool (`bg_pool`): every core bar two, never zero.
/// Heavy *background* work — grid-thumbnail generation and the analysis pass — shares this budget,
/// so a burst can't pin every core. Interactive inspector reads (`read_model_preview`, audio
/// content, `get_asset`) stay off it and thus always have headroom to preempt when the user selects
/// an asset. This is the engine-side inspector-priority lane (golden rule 5, tech-spec 14).
fn background_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(1)
}

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
fn thumb_cache_lookup(data_dir: &Path, asset: &Asset, max_edge: u32) -> Option<AssetContent> {
    std::fs::read(thumb_cache_path(data_dir, asset, max_edge))
        .ok()
        .map(png_content)
}

/// Render (or read from cache) a downscaled PNG thumbnail for an asset — a raster downscale for
/// images, a wgpu turntable render for 3D models (when the `render` feature is on). Pure/blocking —
/// runs inside a blocking closure (the bounded `bg_pool` on a cache miss).
fn gen_thumbnail(
    data_dir: &Path,
    store: &Store,
    asset: &Asset,
    max_edge: u32,
) -> Result<AssetContent, LibError> {
    if let Some(hit) = thumb_cache_lookup(data_dir, asset, max_edge) {
        return Ok(hit); // cache hit → no source access at all
    }
    let cache_path = thumb_cache_path(data_dir, asset, max_edge);

    // Cache miss: resolve the source file (in place for local, downloaded for remote). `fetch`
    // guards `..` traversal out of the source root.
    let fetched = fetch_asset(store, asset)?;
    let det = dam_media::Detected {
        media: asset.summary.media,
        format: asset.summary.format.clone(),
    };
    let bytes = render_thumbnail_bytes(fetched.path(), &det, max_edge)?;

    cache_write_atomic(&cache_path, &bytes);
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
#[cfg_attr(not(feature = "render"), allow(unused_variables))]
fn thumbnail_variant(media: MediaType) -> String {
    #[cfg(feature = "render")]
    if media == MediaType::Model {
        return format!("-r{}", dam_render::RENDER_VERSION);
    }
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

/// Read (generating + caching on miss) the interactive 3D preview blob for a **model** asset — the
/// self-contained `DMSH` mesh (geometry + PBR materials + downscaled textures) the browser island
/// uploads directly (tech-spec 09 §B.3). Mirrors [`gen_thumbnail`]'s content-keyed cache, but the
/// blob is CPU-decoded (no GPU), so it works on GPU-less hosts. Fail-soft: non-models and (in a
/// build without the `render` feature) every model map to `Unsupported`, a 415 the UI degrades on.
fn gen_model_preview(
    data_dir: &Path,
    store: &Store,
    asset: &Asset,
) -> Result<AssetContent, LibError> {
    if asset.summary.media != MediaType::Model {
        return Err(LibError::Unsupported(
            "3D preview is only available for model assets".to_string(),
        ));
    }
    gen_model_preview_impl(data_dir, store, asset)
}

#[cfg(feature = "render")]
fn preview_content(bytes: Vec<u8>) -> AssetContent {
    AssetContent {
        bytes,
        content_type: "model/x-dam-preview".to_string(),
        format: "dmsh".to_string(),
        media: MediaType::Model,
    }
}

/// Decode + cache the `DMSH` blob. Split out so the `render`-off build can short-circuit *before*
/// any (possibly remote) fetch instead of downloading only to fail the decode.
#[cfg(feature = "render")]
fn gen_model_preview_impl(
    data_dir: &Path,
    store: &Store,
    asset: &Asset,
) -> Result<AssetContent, LibError> {
    let key = asset
        .hash
        .map(|h| h.to_hex())
        .unwrap_or_else(|| asset.summary.id.to_string());
    let cache_dir = data_dir.join("cache").join("previews");
    // Suffix carries the blob-format version so a serializer bump invalidates only this slice.
    let cache_path = cache_dir.join(format!("{key}-p{}.dmsh", dam_render::PREVIEW_VERSION));
    if let Ok(bytes) = std::fs::read(&cache_path) {
        return Ok(preview_content(bytes)); // cache hit → no source access at all
    }

    let fetched = fetch_asset(store, asset)?;
    let bytes =
        dam_render::model_preview_blob(fetched.path(), &asset.summary.format).map_err(|e| {
            if matches!(e, dam_render::RenderError::Decode(_)) {
                tracing::warn!("3D preview decode failed: {e}");
            }
            LibError::Unsupported(e.to_string())
        })?;

    cache_write_atomic(&cache_path, &bytes);
    Ok(preview_content(bytes))
}

#[cfg(not(feature = "render"))]
fn gen_model_preview_impl(
    _data_dir: &Path,
    _store: &Store,
    _asset: &Asset,
) -> Result<AssetContent, LibError> {
    Err(LibError::Unsupported(
        "interactive 3D preview needs the server `render` feature".to_string(),
    ))
}

/// Serialise an optional saved query to JSON for the `collection.query` column.
fn serialize_opt_query(q: &Option<QueryRequest>) -> Result<Option<String>, LibError> {
    q.as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| LibError::Internal(e.to_string()))
}

/// Best-effort atomic cache write: create the parent dir, write to a sibling temp file, then rename
/// into place. A cold cache is a slow path, not an error, so every failure is silently ignored. The
/// temp name is derived from the final filename (`.{name}.tmp`), keeping it on the same filesystem.
fn cache_write_atomic(cache_path: &Path, bytes: &[u8]) {
    let (Some(dir), Some(name)) = (
        cache_path.parent(),
        cache_path.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_ok() {
        let tmp = dir.join(format!(".{name}.tmp"));
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, cache_path);
        }
    }
}

/// Emit a job's current progress as a `JobProgress` event (best-effort; a dropped read is skipped).
/// Shared by the scan and analyse job loops.
pub(crate) fn emit_progress(store: &Store, events: &broadcast::Sender<LibraryEvent>, job: &JobId) {
    if let Ok(js) = store.get_job(job) {
        let _ = events.send(LibraryEvent::JobProgress(js));
    }
}

fn png_content(bytes: Vec<u8>) -> AssetContent {
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

/// Rebuild the asset's source backend and resolve its bytes to a local path (in place for local,
/// downloaded to a temp file for SFTP/SMB). Traversal-guarded inside `fetch`. Pure/blocking.
fn fetch_asset(store: &Store, asset: &Asset) -> Result<dam_sources::Fetched, LibError> {
    let conn = store.get_source_connection(&asset.source_id)?;
    let fs = dam_sources::open_source(&conn)?;
    fs.fetch(&asset.path)
}

/// Read an asset's bytes for a preview, bounded by the content cap. The size gate is checked
/// against the stored size *before* any (possibly remote) fetch, so an oversized asset never
/// triggers a download. Pure/blocking — called inside a `spawn_blocking` closure.
fn read_asset_content(store: &Store, asset: &Asset) -> Result<AssetContent, LibError> {
    let size = asset.summary.size;
    if size > MAX_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "asset is {size} bytes; preview content is capped at {MAX_CONTENT_BYTES} bytes"
        )));
    }
    let fetched = fetch_asset(store, asset)?;
    let abs = fetched.path();
    let bytes = std::fs::read(abs)
        .map_err(|e| LibError::Internal(format!("read {}: {e}", abs.display())))?;
    let media = asset.summary.media;
    let format = asset.summary.format.clone();
    let content_type = content_type_for(media, &format).to_string();
    Ok(AssetContent {
        bytes,
        content_type,
        format,
        media,
    })
}

/// Resolve `rel` against the *directory* of `base` (a source-relative path), normalising `.`/`..`
/// and rejecting anything absolute or that escapes the source root. Returns a clean source-relative
/// path. This is the loose-glTF sibling resolver (#56); the source's own `fetch` is separately
/// traversal-guarded as defence-in-depth.
fn resolve_sibling(base: &str, rel: &str) -> Result<String, LibError> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err(LibError::BadRequest("empty related path".into()));
    }
    // Absolute paths (POSIX or Windows-drive) and URLs are never source-relative siblings.
    if rel.starts_with('/') || rel.starts_with('\\') || rel.contains("://") {
        return Err(LibError::BadRequest(
            "related path must be source-relative".into(),
        ));
    }
    // Start from the base file's directory (drop its final component).
    let mut parts: Vec<&str> = base.split('/').collect();
    parts.pop();
    for seg in rel.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(LibError::BadRequest(
                        "related path escapes the source".into(),
                    ));
                }
            }
            s => parts.push(s),
        }
    }
    Ok(parts.join("/"))
}

/// Read a file relative to `asset`'s directory within the same source, bounded by the content cap.
/// Pure/blocking — called inside a `spawn_blocking` closure. Powers loose-glTF external buffers (#56).
fn read_related_content(store: &Store, asset: &Asset, rel: &str) -> Result<AssetContent, LibError> {
    let target = resolve_sibling(&asset.path, rel)?;
    let conn = store.get_source_connection(&asset.source_id)?;
    let fs = dam_sources::open_source(&conn)?;
    let fetched = fs.fetch(&target)?;
    let abs = fetched.path();
    let meta = std::fs::metadata(abs)
        .map_err(|e| LibError::NotFound(format!("related file {target}: {e}")))?;
    if meta.len() > MAX_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "related file is {} bytes; preview content is capped at {MAX_CONTENT_BYTES} bytes",
            meta.len()
        )));
    }
    let bytes = std::fs::read(abs)
        .map_err(|e| LibError::Internal(format!("read {}: {e}", abs.display())))?;
    Ok(AssetContent {
        bytes,
        content_type: "application/octet-stream".to_string(),
        format: String::new(),
        media: asset.summary.media,
    })
}

/// The in-process engine. Cheap to clone the handle by wrapping in `Arc`.
pub struct EmbeddedLibrary {
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    data_dir: PathBuf,
    cancels: Mutex<HashMap<JobId, Arc<AtomicBool>>>,
    /// Auto-rescan watchers for `watch`-enabled sources (tech-spec 07 §3.1).
    watchers: watch::WatchManager,
    /// Bounded pool for heavy *background* CPU work (thumbnail generation + the analysis pass),
    /// sized to leave cores free so interactive inspector reads preempt it (see [`background_threads`]).
    bg_pool: Arc<rayon::ThreadPool>,
    /// Model-backed semantic embedder (semantic-search M4), or `None` when no weights ship — the
    /// default. When present, the analysis pass also writes its space and text search can encode a
    /// query into it. Held behind the [`semantic::SemanticModel`] seam.
    semantic: Option<Arc<dyn semantic::SemanticModel>>,
    /// Federated peers (phase 6, issue #39): the TTL-cached registry the query fan-out reads,
    /// rebuilt from the source table and invalidated on source add/remove.
    fed: federation::PeerRegistry,
}

impl EmbeddedLibrary {
    /// Open (creating if needed) the library rooted at `data_dir`.
    pub async fn open(data_dir: &Path) -> Result<EmbeddedLibrary, LibError> {
        let dir = data_dir.to_path_buf();
        let store = tokio::task::spawn_blocking(move || Store::open(&dir))
            .await
            .map_err(|e| LibError::Internal(e.to_string()))??;
        let store = Arc::new(store);
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let watchers = watch::WatchManager::new(
            store.clone(),
            events.clone(),
            tokio::runtime::Handle::current(),
        );
        // NB: watchers are *not* started here. Auto-rescan only makes sense for long-running roles
        // (serve/mcp), which call `start_watchers()` explicitly. A run-and-exit CLI command must not
        // register OS watches — they add nothing to a one-shot and their setup would outlive the
        // command (keeping the runtime from shutting down). See tech-spec 07 §3.1.
        // Load the semantic model if this build ships one (M4). `None` by default — the model-free
        // embeddings stand in — so this is a cheap, always-safe call.
        let semantic = semantic::load(data_dir).map(Arc::from);
        let bg_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(background_threads())
            .thread_name(|i| format!("dam-bg-{i}"))
            .build()
            .map_err(|e| LibError::Internal(e.to_string()))?;
        Ok(EmbeddedLibrary {
            store,
            events,
            data_dir: data_dir.to_path_buf(),
            cancels: Mutex::new(HashMap::new()),
            watchers,
            bg_pool: Arc::new(bg_pool),
            semantic,
            fed: federation::PeerRegistry::new(),
        })
    }

    /// Open at the platform default location.
    pub async fn open_default() -> Result<EmbeddedLibrary, LibError> {
        EmbeddedLibrary::open(&default_data_dir()).await
    }

    /// Resume auto-rescan for every `watch`-enabled source (tech-spec 07 §3.1). Long-running roles
    /// (`serve`, `mcp`) call this once after open; one-shot CLI commands never do. Non-blocking:
    /// each OS watch is registered off the async runtime, so a huge or network-backed root can't
    /// stall startup.
    pub fn start_watchers(&self) {
        self.watchers.start_all();
    }

    /// Run a synchronous store operation on the blocking pool (tech-spec 14).
    async fn db<T, F>(&self, f: F) -> Result<T, LibError>
    where
        F: FnOnce(&Store) -> Result<T, LibError> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .map_err(|e| LibError::Internal(e.to_string()))?
    }

    /// The local-index query path — the pre-federation body of [`LibraryService::query`], shared
    /// by the plain path and the fan-out engine (which merges this page with the peers').
    pub(crate) async fn local_query(
        &self,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        let model = self.semantic.clone();
        self.db(move |s| {
            // Model-backed text→asset search (semantic-search M4): when a semantic model is loaded and
            // this is a Hybrid/Semantic text query, encode the query string into the model's shared
            // space so assets that match the *meaning* (not the filename) rank in. Encoding is
            // CPU-bound and runs here on the blocking DB thread. No model ⇒ `None` ⇒ model-free path.
            let text_vec = match (&model, req.mode, req.text.as_deref()) {
                (Some(m), SearchMode::Hybrid | SearchMode::Semantic, Some(t)) if !t.is_empty() => m
                    .encode_text(MediaType::Image, t)
                    .map(|v| (m.space_id(MediaType::Image), v)),
                _ => None,
            };
            s.query_assets_semantic(&req, text_vec)
        })
        .await
    }

    /// The `media type → EmbeddingSpace id` map this instance ranks similarity in — what
    /// `advertise()` publishes so peers can gate cross-peer similarity on an exact space match
    /// (phase 6, issue #40). Model-free v1 spaces by default; a loaded semantic model overrides
    /// its media with the model-backed space id.
    pub fn embedding_spaces(&self) -> std::collections::BTreeMap<String, String> {
        let mut spaces = std::collections::BTreeMap::new();
        for media in [MediaType::Audio, MediaType::Image, MediaType::Model] {
            let id = match &self.semantic {
                Some(m) => m.space_id(media),
                None => format!("{}-stats-v1", media.as_str()),
            };
            spaces.insert(media.as_str().to_string(), id);
        }
        spaces
    }

    /// Like [`Self::db`], but runs the closure on the bounded background pool (`bg_pool`) instead of
    /// the unbounded blocking pool. Use for heavy *background* generation (thumbnails) so a burst
    /// can't saturate every core — interactive inspector reads stay on `db()` and preempt it. The
    /// hand-off is a one-shot async→CPU hop (golden rule 5); the caller awaits the result.
    async fn run_bg<T, F>(&self, f: F) -> Result<T, LibError>
    where
        F: FnOnce(&Store) -> Result<T, LibError> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.bg_pool.spawn(move || {
            let _ = tx.send(f(&store));
        });
        rx.await.map_err(|e| LibError::Internal(e.to_string()))?
    }

    // ── storage & maintenance (Settings §Storage, tech-spec 10 §5) ────────────
    //
    // Operator-plane methods the server's `/admin/api/maintenance/*` routes delegate to. Inherent
    // (not on the `LibraryService` seam) because maintenance is admin-plane — the CLI reaches these
    // through the audited admin surface, not the generic frontend trait.

    /// Report on-disk usage: `library.db` + `server.db` sizes, both cache tiers, and catalog counts.
    /// Read-only. File sizing runs off the async runtime.
    pub async fn storage_usage(&self) -> Result<StorageUsage, LibError> {
        let stats = self.db(|s| s.stats()).await?;
        let data_dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let file_len = |p: PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            Ok(StorageUsage {
                data_dir: data_dir.display().to_string(),
                library_db_bytes: file_len(data_dir.join("library.db")),
                server_db_bytes: file_len(data_dir.join("server.db")),
                thumbnails: dir_usage(&data_dir.join("cache").join("thumbnails")),
                previews: dir_usage(&data_dir.join("cache").join("previews")),
                asset_count: stats.total,
                source_count: stats.sources,
            })
        })
        .await
        .map_err(|e| LibError::Internal(e.to_string()))?
    }

    /// Delete the selected regenerable cache tier(s) under `<data_dir>/cache/`. Non-destructive:
    /// every file is content-keyed and re-generated on the next thumbnail/preview read, so this
    /// emits no event.
    pub async fn clear_caches(&self, target: CacheTarget) -> Result<ClearCacheReport, LibError> {
        let data_dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let cache = data_dir.join("cache");
            let mut freed = CacheUsage { bytes: 0, files: 0 };
            if matches!(target, CacheTarget::Thumbnails | CacheTarget::All) {
                let u = clear_dir(&cache.join("thumbnails"));
                freed.bytes += u.bytes;
                freed.files += u.files;
            }
            if matches!(target, CacheTarget::Previews | CacheTarget::All) {
                let u = clear_dir(&cache.join("previews"));
                freed.bytes += u.bytes;
                freed.files += u.files;
            }
            Ok(ClearCacheReport {
                bytes_freed: freed.bytes,
                files_deleted: freed.files,
            })
        })
        .await
        .map_err(|e| LibError::Internal(e.to_string()))?
    }

    /// Drop the analysis layer (suggestions + embeddings + derived attrs) and mark every asset due
    /// for re-analysis, keeping user-confirmed tags. Emits `CatalogReset` so open grids refresh.
    pub async fn clear_analysis(&self) -> Result<ClearAnalysisReport, LibError> {
        let report = self.db(|s| s.clear_analysis()).await?;
        let _ = self.events.send(LibraryEvent::CatalogReset);
        Ok(report)
    }

    /// Compact `library.db` (`VACUUM`), returning the before/after size and bytes reclaimed.
    pub async fn vacuum(&self) -> Result<VacuumReport, LibError> {
        let db_path = self.data_dir.join("library.db");
        let before = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        self.db(|s| s.vacuum()).await?;
        let after = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        Ok(VacuumReport {
            before_bytes: before,
            after_bytes: after,
            reclaimed_bytes: before.saturating_sub(after),
        })
    }

    /// Reset the catalog to empty — assets/sources/collections/tags/embeddings/suggestions/jobs/
    /// blocklist — without touching files in sources or `server.db`. Emits `CatalogReset` so live
    /// clients empty their grids.
    pub async fn wipe_catalog(&self) -> Result<WipeReport, LibError> {
        let report = self.db(|s| s.wipe_catalog()).await?;
        let _ = self.events.send(LibraryEvent::CatalogReset);
        Ok(report)
    }
}

/// Sum the size + count of the regular files directly under `dir` (the cache tiers are flat). A
/// missing/unreadable dir reads as empty — a cold cache is zero usage, not an error.
fn dir_usage(dir: &Path) -> CacheUsage {
    let mut usage = CacheUsage { bytes: 0, files: 0 };
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if let Ok(m) = entry.metadata() {
                if m.is_file() {
                    usage.bytes += m.len();
                    usage.files += 1;
                }
            }
        }
    }
    usage
}

/// Best-effort delete of every regular file directly under `dir`, returning what was freed. Leaves
/// the directory itself (it is recreated lazily on the next cache write). A file that fails to
/// delete is skipped, not counted — fail-soft (DESIGN_GUIDELINES §2).
fn clear_dir(dir: &Path) -> CacheUsage {
    let mut freed = CacheUsage { bytes: 0, files: 0 };
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let Ok(m) = entry.metadata() else { continue };
            if m.is_file() && std::fs::remove_file(entry.path()).is_ok() {
                freed.bytes += m.len();
                freed.files += 1;
            }
        }
    }
    freed
}

/// Delete every cached derivative keyed to one asset — its thumbnail PNGs (across edges + renderer
/// variants) and its 3D preview blob — returning how many files were removed. Both cache tiers are
/// flat and every entry is named `{key}-…`, so a prefix match cleanly scopes deletion to this asset's
/// slice without disturbing others. Best-effort per file (fail-soft, DESIGN_GUIDELINES §2).
fn purge_asset_cache(data_dir: &Path, key: &str) -> u64 {
    let prefix = format!("{key}-");
    let mut removed = 0u64;
    for tier in ["thumbnails", "previews"] {
        let dir = data_dir.join("cache").join(tier);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix)
                && entry.metadata().map(|m| m.is_file()).unwrap_or(false)
                && std::fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}

#[async_trait]
impl LibraryService for EmbeddedLibrary {
    async fn query(
        &self,
        _ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        // Federated fan-out (phase 6, issue #39): merge local + peer pages when federated sources
        // are registered. `local_only` marks a peer-bound call — one hop, never transitive.
        if !req.local_only {
            if let Some(page) = federation::federated_query(self, &req).await? {
                return Ok(page);
            }
        }
        self.local_query(req).await
    }

    async fn get_asset(&self, _ctx: &AuthContext, id: &AssetId) -> Result<Asset, LibError> {
        let id = *id;
        match self.db(move |s| s.get_asset(&id)).await {
            // A merged result can name a peer-owned asset: proxy the detail read (phase 6).
            Err(LibError::NotFound(_)) => federation::proxy_get_asset(self, &id)
                .await
                .ok_or_else(|| LibError::NotFound(format!("asset {id}"))),
            r => r,
        }
    }

    async fn read_content(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        let local = self
            .db(move |s| {
                let asset = s.get_asset(&id)?;
                read_asset_content(s, &asset)
            })
            .await;
        match local {
            Err(LibError::NotFound(_)) => federation::proxy_read_content(self, &id)
                .await
                .ok_or_else(|| LibError::NotFound(format!("asset {id}"))),
            r => r,
        }
    }

    async fn read_related_content(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        let rel = rel.to_string();
        let rel2 = rel.clone();
        let local = self
            .db(move |s| {
                let asset = s.get_asset(&id)?;
                read_related_content(s, &asset, &rel2)
            })
            .await;
        match local {
            Err(LibError::NotFound(_)) => federation::proxy_read_related(self, &id, &rel)
                .await
                .ok_or_else(|| LibError::NotFound(format!("asset {id}"))),
            r => r,
        }
    }

    async fn read_thumbnail(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        let edge = max_edge.clamp(THUMB_MIN_EDGE, THUMB_MAX_EDGE);
        let data_dir = self.data_dir.clone();
        // Fast path: a cheap cache probe on the unbounded pool, so an already-rendered thumbnail is
        // never stuck behind background generation.
        let probe_dir = data_dir.clone();
        let probe = self
            .db(move |s| {
                let asset = s.get_asset(&id)?;
                Ok(thumb_cache_lookup(&probe_dir, &asset, edge))
            })
            .await;
        match probe {
            Ok(Some(hit)) => return Ok(hit),
            Ok(None) => {}
            // Peer-owned asset: fetch its remote-owned preview — the one sanctioned federated byte
            // transfer (tech-spec 07 §4) — through the 7-day local peer cache.
            Err(LibError::NotFound(_)) => {
                return federation::proxy_thumbnail(self, &id, edge)
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            Err(e) => return Err(e),
        }
        // Cache miss: the expensive render/decode runs on the bounded background pool so a grid
        // burst can't starve an interactive inspector read (preview / waveform / detail).
        self.run_bg(move |s| {
            let asset = s.get_asset(&id)?;
            gen_thumbnail(&data_dir, s, &asset, edge)
        })
        .await
    }

    async fn read_model_preview(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        let data_dir = self.data_dir.clone();
        let local = self
            .db(move |s| {
                let asset = s.get_asset(&id)?;
                gen_model_preview(&data_dir, s, &asset)
            })
            .await;
        match local {
            Err(LibError::NotFound(_)) => federation::proxy_model_preview(self, &id)
                .await
                .ok_or_else(|| LibError::NotFound(format!("asset {id}"))),
            r => r,
        }
    }

    async fn prefetch(&self, _ctx: &AuthContext, req: PrefetchRequest) -> Result<(), LibError> {
        if req.assets.is_empty() {
            return Ok(());
        }
        // Warm the exact thumbnail edge the client will request (the grid uses a variable edge the
        // background pipeline can't all pre-render), plus model preview meshes. Runs off the request
        // path on the blocking pool; fire-and-forget so the caller returns immediately (issue #72).
        let edge = req
            .edge
            .unwrap_or(background::PREGEN_THUMB_EDGE)
            .clamp(THUMB_MIN_EDGE, THUMB_MAX_EDGE);
        let store = self.store.clone();
        let data_dir = self.data_dir.clone();
        let assets = req.assets.clone();
        tokio::task::spawn_blocking(move || {
            for id in assets {
                let Ok(asset) = store.get_asset(&id) else {
                    continue; // vanished (or peer-owned) — fail-soft
                };
                let _ = gen_thumbnail(&data_dir, &store, &asset, edge);
                if asset.summary.media == MediaType::Model {
                    let _ = gen_model_preview(&data_dir, &store, &asset);
                }
            }
        });
        // Merged grids can hold peer assets: forward the same hint so each peer warms its own
        // derivatives (phase 6). Fire-and-forget, same as the local pass — ids a peer doesn't own
        // are its no-ops.
        for peer in self.fed_peers().await.iter() {
            let peer = peer.clone();
            let req = req.clone();
            tokio::spawn(async move {
                let _ = peer
                    .client
                    .prefetch(&dam_api::service::AuthContext::embedded(), req)
                    .await;
            });
        }
        Ok(())
    }

    async fn library_stats(&self, _ctx: &AuthContext) -> Result<LibraryStats, LibError> {
        self.db(|s| s.stats()).await
    }

    async fn convert(
        &self,
        _ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<ConvertReport, LibError> {
        self.db(move |s| convert::run_convert(s, req)).await
    }

    async fn list_sources(&self, _ctx: &AuthContext) -> Result<Vec<SourceInfo>, LibError> {
        self.db(|s| s.list_sources()).await
    }

    async fn get_source(&self, _ctx: &AuthContext, id: &SourceId) -> Result<SourceInfo, LibError> {
        let id = *id;
        self.db(move |s| {
            s.get_source(&id)?
                .ok_or_else(|| LibError::NotFound(format!("source {id}")))
        })
        .await
    }

    async fn list_folders(
        &self,
        _ctx: &AuthContext,
        req: FolderListing,
    ) -> Result<Vec<FolderEntry>, LibError> {
        // Normalise the prefix so the derived-tree SQL is well-defined: empty (root) or ending in `/`.
        let prefix = if req.prefix.is_empty() || req.prefix.ends_with('/') {
            req.prefix
        } else {
            format!("{}/", req.prefix)
        };
        let source = req.source;
        self.db(move |s| s.list_folders(&source, &prefix)).await
    }

    async fn add_source(&self, _ctx: &AuthContext, req: AddSource) -> Result<SourceId, LibError> {
        let opts = dam_sources::ConnOptions {
            username: req.options.username.clone(),
            password: req.options.password.clone(),
            private_key: req.options.private_key.clone(),
            passphrase: req.options.passphrase.clone(),
            domain: req.options.domain.clone(),
            port: req.options.port,
        };
        let mut conn = dam_sources::SourceConnection::parse(req.kind.as_str(), &req.uri, &opts)?;

        // Local FS: normalise + existence-check up front (fail early on a typo). Remote sources are
        // allowed to be offline at add-time — the scan surfaces reachability, fail-soft (§3).
        let default_name: String;
        if let dam_sources::SourceConnection::LocalFs { root } = &mut conn {
            let path = PathBuf::from(&*root);
            if !path.exists() {
                return Err(LibError::BadRequest(format!("path does not exist: {root}")));
            }
            let canon = path
                .canonicalize()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| root.clone());
            default_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| canon.clone());
            *root = canon;
        } else {
            default_name = conn.display_uri();
        }

        // Federated peer: handshake up front (phase 6, issue #39) — verify the peer serves
        // federation and speaks a compatible protocol version, so a typo'd endpoint or a
        // flag-off peer fails here with a clear message, not silently at first query.
        if let dam_sources::SourceConnection::Federated(cfg) = &conn {
            let client = federation::connect_peer(&cfg.endpoint, cfg.token.clone()).await?;
            let ad = tokio::time::timeout(federation::ADD_HANDSHAKE_TIMEOUT, client.advertise())
                .await
                .map_err(|_| {
                    LibError::BadRequest(format!(
                        "peer {} did not answer advertise in time",
                        cfg.endpoint
                    ))
                })?
                .map_err(|e| match e {
                    LibError::NotFound(_) => LibError::BadRequest(format!(
                        "peer {} is not serving federation — enable its 'federation' flag",
                        cfg.endpoint
                    )),
                    e => LibError::BadRequest(format!("peer {} unreachable: {e}", cfg.endpoint)),
                })?;
            if !dam_api::protocol_compatible(
                &ad.protocol_version,
                dam_api::FEDERATION_PROTOCOL_VERSION,
            ) {
                return Err(LibError::BadRequest(format!(
                    "peer {} speaks federation protocol {} but this build speaks {}",
                    cfg.endpoint,
                    ad.protocol_version,
                    dam_api::FEDERATION_PROTOCOL_VERSION
                )));
            }
        }

        let name = req.name.clone().unwrap_or(default_name);
        let watch = req.options.watch;
        let is_federated = matches!(conn, dam_sources::SourceConnection::Federated(_));
        let id = self.db(move |s| s.add_source(&conn, &name, watch)).await?;
        // Start watching immediately if requested (tech-spec 07 §3.1).
        if watch {
            self.watchers.ensure(id);
        }
        if is_federated {
            self.fed.invalidate().await; // participate in the very next query
        }
        Ok(id)
    }

    async fn remove_source(
        &self,
        _ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError> {
        let id = *id;
        self.db(move |s| s.remove_source(&id, req.keep_metadata))
            .await?;
        self.fed.invalidate().await;
        Ok(())
    }

    // ── remove / blocklist (issue #21) ───────────────────────────────────────
    async fn remove_asset(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        req: RemoveAsset,
    ) -> Result<(), LibError> {
        let id = *id;
        self.db(move |s| s.remove_asset(&id, req.block)).await?;
        // Live update: drop the row from every open grid/inspector (mirrors AssetAdded on scan).
        let _ = self.events.send(LibraryEvent::AssetRemoved(id));
        Ok(())
    }

    async fn list_blocklist(&self, _ctx: &AuthContext) -> Result<Vec<BlockEntry>, LibError> {
        self.db(|s| s.list_blocklist()).await
    }

    async fn unblock(&self, _ctx: &AuthContext, hash: &ContentHash) -> Result<(), LibError> {
        let hash = *hash;
        self.db(move |s| s.unblock(&hash)).await
    }

    // ── collections / smart folders ──────────────────────────────────────────
    async fn list_collections(&self, _ctx: &AuthContext) -> Result<Vec<Collection>, LibError> {
        self.db(|s| s.list_collections()).await
    }

    async fn get_collection(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<Collection, LibError> {
        let id = *id;
        self.db(move |s| {
            let mut c = s.get_collection(&id)?;
            // A smart folder's count is the live match count — compute it on the single-item read.
            if c.kind == CollectionKind::Smart {
                let ids = s.query_asset_ids(&c.query.clone().unwrap_or_default())?;
                c.count = Some(ids.len() as u64);
            }
            Ok(c)
        })
        .await
    }

    async fn create_collection(
        &self,
        _ctx: &AuthContext,
        req: NewCollection,
    ) -> Result<CollectionId, LibError> {
        if req.kind == CollectionKind::Smart && req.query.is_none() {
            return Err(LibError::BadRequest(
                "a smart folder requires a query".into(),
            ));
        }
        let query_json = serialize_opt_query(&req.query)?;
        let name = req.name.clone();
        let kind = req.kind;
        self.db(move |s| s.create_collection(&name, kind, query_json.as_deref()))
            .await
    }

    async fn update_collection(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
        req: UpdateCollection,
    ) -> Result<(), LibError> {
        let id = *id;
        let query_json = serialize_opt_query(&req.query)?;
        let name = req.name.clone();
        self.db(move |s| s.update_collection(&id, name.as_deref(), query_json.as_deref()))
            .await
    }

    async fn delete_collection(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<(), LibError> {
        let id = *id;
        self.db(move |s| s.delete_collection(&id)).await
    }

    async fn modify_collection_members(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
        req: CollectionMembers,
    ) -> Result<(), LibError> {
        let id = *id;
        let add = req.add.clone();
        let remove = req.remove.clone();
        self.db(move |s| s.modify_collection_members(&id, &add, &remove))
            .await
    }

    async fn collection_assets(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
        page: PageParams,
    ) -> Result<Page<AssetSummary>, LibError> {
        let id = *id;
        self.db(move |s| {
            let coll = s.get_collection(&id)?;
            match coll.kind {
                CollectionKind::Manual => {
                    let items = s.collection_summaries(&id, page.clamped(500))?;
                    Ok(Page::new(items, None))
                }
                CollectionKind::Smart => {
                    // Live resolution: run the saved query with the caller's page window.
                    let mut q = coll.query.unwrap_or_default();
                    q.page = page;
                    s.query_assets(&q)
                }
            }
        })
        .await
    }

    async fn export(
        &self,
        _ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<ExportReport, LibError> {
        self.db(move |s| export::run_export(s, req)).await
    }

    async fn submit_scan(&self, _ctx: &AuthContext, req: ScanRequest) -> Result<JobId, LibError> {
        // Resolve target sources (all file sources when none specified; federated peers excluded).
        let all = self.db(|s| s.list_sources()).await?;
        let sources: Vec<SourceInfo> = if req.sources.is_empty() {
            all.into_iter()
                .filter(|s| s.kind != SourceKind::Federated)
                .collect()
        } else {
            all.into_iter()
                .filter(|s| req.sources.contains(&s.id) && s.kind != SourceKind::Federated)
                .collect()
        };
        if sources.is_empty() {
            return Err(LibError::BadRequest(
                "no scannable file sources selected".into(),
            ));
        }

        let mode = req.mode;
        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let job = self
            .db(move |s| s.create_job(JobKind::Scan, &params, None))
            .await?;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());

        let store = self.store.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            scan::run_scan(store, events, job, sources, mode, cancel);
        });

        Ok(job)
    }

    async fn submit_analyze(
        &self,
        _ctx: &AuthContext,
        req: AnalyzeRequest,
    ) -> Result<JobId, LibError> {
        // Plan: resolve the due (or requested) targets up front so the job total is known (§1.2).
        let assets = req.assets.clone();
        let force = req.force;
        let targets = self
            .db(move |s| s.list_analysis_targets(analysis::PIPELINE_VERSION, force, &assets))
            .await?;
        if targets.is_empty() {
            return Err(LibError::BadRequest(
                "nothing to analyse (all assets are up to date; pass --force to re-run)".into(),
            ));
        }

        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let total = targets.len() as u64;
        let job = self
            .db(move |s| s.create_job(JobKind::Analyze, &params, Some(total)))
            .await?;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());

        let store = self.store.clone();
        let events = self.events.clone();
        let model = self.semantic.clone();
        let pool = self.bg_pool.clone();
        tokio::task::spawn_blocking(move || {
            analysis::run_analyze(store, events, job, targets, cancel, model, &pool);
        });
        Ok(job)
    }

    async fn regenerate_thumbnails(
        &self,
        _ctx: &AuthContext,
        req: ThumbnailRegenRequest,
    ) -> Result<ThumbnailRegenReport, LibError> {
        let data_dir = self.data_dir.clone();
        // Resolve each asset's content key inside the store lock, then purge its cache slice; the
        // next thumbnail read re-renders from source. A missing asset fails the whole request (the
        // caller passed a bad id) — per-item fail-soft applies to the file deletes, not the lookup.
        self.db(move |s| {
            let mut report = ThumbnailRegenReport::default();
            for id in &req.assets {
                let asset = s.get_asset(id)?;
                let key = asset
                    .hash
                    .map(|h| h.to_hex())
                    .unwrap_or_else(|| asset.summary.id.to_string());
                report.files_deleted += purge_asset_cache(&data_dir, &key);
                report.assets += 1;
            }
            Ok(report)
        })
        .await
    }

    async fn find_similar(
        &self,
        ctx: &AuthContext,
        req: SimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        let (asset, k) = (req.asset, req.k);
        let filters = req.filters.clone();
        let hits = self
            .db(move |s| s.similar(&asset, k, &filters))
            .await?
            .into_iter()
            // Tag each hit with the media space it was ranked in (the explanation, §3.2).
            .map(|(asset, score)| SimilarHit {
                space: format!("{}-stats-v1", asset.media.as_str()),
                asset,
                score,
            })
            .collect::<Vec<_>>();
        if req.local_only {
            return Ok(Page::new(hits, None));
        }
        // Cross-peer similarity (phase 6, issue #40): ship the query asset's own vector to every
        // matched-space peer and merge one globally-ranked list. No vector yet (or a source-pinned
        // query) degrades to the local page. A peer-owned query asset is instead forwarded whole —
        // the owning peer ranks it in its index (`local_only` keeps that a single hop).
        if self.fed_peers().await.is_empty()
            || req.filters.iter().any(|f| f.field == FacetField::Source)
        {
            return Ok(Page::new(hits, None));
        }
        let embedding = self.db(move |s| s.embedding_for(&asset)).await?;
        match embedding {
            Some((space, vector)) => {
                let media = self.get_asset(ctx, &req.asset).await?.summary.media;
                Ok(federation::federated_similar(self, &req, media, space, vector, hits).await)
            }
            None => {
                let local_has = !hits.is_empty()
                    || self
                        .db(move |s| s.get_asset(&asset).map(|_| ()))
                        .await
                        .is_ok();
                if local_has {
                    return Ok(Page::new(hits, None));
                }
                for peer in self.fed_peers().await.iter() {
                    let mut fwd = req.clone();
                    fwd.local_only = true;
                    if let Ok(Ok(mut page)) = tokio::time::timeout(
                        federation::QUERY_DEADLINE,
                        peer.client.find_similar(ctx, fwd),
                    )
                    .await
                    {
                        for hit in &mut page.items {
                            hit.asset.origin = Origin::Peer(peer.name.clone());
                        }
                        return Ok(page);
                    }
                }
                Ok(Page::new(hits, None))
            }
        }
    }

    async fn find_similar_by_vector(
        &self,
        _ctx: &AuthContext,
        req: dam_api::VectorSimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        // The serving side of cross-peer similarity (issue #40): rank the shipped vector against
        // this catalog's own index. Strictly local by construction — never re-fans-out.
        let dam_api::VectorSimilarRequest {
            media: _,
            space,
            vector,
            k,
            filters,
        } = req;
        let space_for_hits = space.clone();
        let hits = self
            .db(move |s| s.similar_by_vector(&space, &vector, k, &filters))
            .await?
            .into_iter()
            .map(|(asset, score)| SimilarHit {
                space: space_for_hits.clone(),
                asset,
                score,
            })
            .collect::<Vec<_>>();
        Ok(Page::new(hits, None))
    }

    async fn list_duplicates(
        &self,
        _ctx: &AuthContext,
        req: DupRequest,
    ) -> Result<Vec<DupGroup>, LibError> {
        self.db(move |s| s.duplicates(&req)).await
    }

    async fn review_suggestion(
        &self,
        _ctx: &AuthContext,
        req: SuggestionReview,
    ) -> Result<(), LibError> {
        let state = match req.action {
            ReviewAction::Accept => "confirmed",
            ReviewAction::Reject => "rejected",
        };
        let id = req.asset;
        let tag = req.tag.clone();
        self.db(move |s| s.set_tag_state(&id, &tag, state)).await?;
        let _ = self.events.send(LibraryEvent::AssetChanged {
            id: req.asset,
            kind: ChangeKind::Retagged,
        });
        Ok(())
    }

    async fn set_favorite(&self, _ctx: &AuthContext, req: FavoriteRequest) -> Result<(), LibError> {
        let id = req.asset;
        let on = req.favorite;
        self.db(move |s| s.set_favorite(&id, on)).await?;
        let _ = self.events.send(LibraryEvent::AssetChanged {
            id: req.asset,
            kind: ChangeKind::Metadata,
        });
        Ok(())
    }

    async fn get_job(&self, _ctx: &AuthContext, id: &JobId) -> Result<JobStatus, LibError> {
        let id = *id;
        self.db(move |s| s.get_job(&id)).await
    }

    async fn list_jobs(
        &self,
        _ctx: &AuthContext,
        req: JobListRequest,
    ) -> Result<Page<JobStatus>, LibError> {
        self.db(move |s| s.list_jobs(&req)).await
    }

    async fn cancel_job(&self, _ctx: &AuthContext, id: &JobId) -> Result<(), LibError> {
        if let Some(flag) = self.cancels.lock().unwrap().get(id) {
            flag.store(true, Ordering::Relaxed);
        }
        // Reflect intent immediately; the running task also sets terminal state on exit.
        let id = *id;
        self.db(move |s| {
            let job = s.get_job(&id)?;
            if matches!(job.state, JobState::Queued | JobState::Running) {
                s.set_job_state(&id, JobState::Cancelled, None)?;
            }
            Ok(())
        })
        .await
    }

    async fn subscribe(
        &self,
        _ctx: &AuthContext,
        _req: SubscribeRequest,
    ) -> Result<EventStream<LibraryEvent>, LibError> {
        let rx = self.events.subscribe();
        // Drop lag errors (a slow subscriber missed events) rather than failing the stream.
        let stream =
            tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|r| async move { r.ok() });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_sibling;

    #[test]
    fn resolve_sibling_confines_to_source() {
        // Same-directory sibling (the common loose-glTF `.bin` case).
        assert_eq!(
            resolve_sibling("models/scene.gltf", "scene.bin").unwrap(),
            "models/scene.bin"
        );
        // Sub-directory (e.g. textures/).
        assert_eq!(
            resolve_sibling("models/scene.gltf", "textures/wall.png").unwrap(),
            "models/textures/wall.png"
        );
        // A `..` that stays within the source root.
        assert_eq!(
            resolve_sibling("a/b/scene.gltf", "../shared.bin").unwrap(),
            "a/shared.bin"
        );
        // A glTF at the source root.
        assert_eq!(
            resolve_sibling("scene.gltf", "scene.bin").unwrap(),
            "scene.bin"
        );
        // `.`/`..` segments normalise.
        assert_eq!(
            resolve_sibling("m/s.gltf", "./x/../y.bin").unwrap(),
            "m/y.bin"
        );

        // Traversal that escapes the source root is rejected.
        assert!(resolve_sibling("models/s.gltf", "../../etc/passwd").is_err());
        assert!(resolve_sibling("s.gltf", "../secret").is_err());
        // Absolute paths and URLs are never source-relative siblings.
        assert!(resolve_sibling("m/s.gltf", "/etc/passwd").is_err());
        assert!(resolve_sibling("m/s.gltf", "\\windows\\system32").is_err());
        assert!(resolve_sibling("m/s.gltf", "http://evil/x").is_err());
        // Empty is rejected.
        assert!(resolve_sibling("m/s.gltf", "  ").is_err());
    }
}
