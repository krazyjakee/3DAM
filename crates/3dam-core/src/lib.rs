//! `dam-core` — the pure engine. `EmbeddedLibrary` implements the `LibraryService` seam over the
//! store, media handlers, and sources (tech-spec 01 §3). No UI, transport, or GPU (ADR 0002).
//! This is the standalone, no-network path; `3dam-server` serves the same object over HTTP/WS.

mod analysis;
mod background;
mod cache;
mod convert;
mod credentials;
mod export;
mod federation;
mod paths;
mod resources;
mod scan;
pub mod semantic;
mod upload;
mod watch;

pub use background::PipelinePolicy;
pub use cache::CacheOptions;
/// Re-exported so a transport can stage an upload under a name the engine's scratch sweep knows
/// (issue #80). The server stages the request body itself but must not depend on `dam-sources`
/// directly — frontends reach the engine, not around it.
pub use dam_sources::UPLOAD_SCRATCH_PREFIX;
pub use paths::default_data_dir;
pub use resources::ResourceOptions;

/// Whether this machine has a video decode backend installed (a discovered `ffprobe`, ADR 0015).
///
/// Re-exported from `dam-media` so the server can advertise it without taking a direct dependency
/// on the handler crate — frontends talk to the engine, not around it. It is a property of the
/// *host*, not the build, so a client has no way to infer it and an unexplained empty tile would
/// read as a bug.
pub fn video_probe_available() -> bool {
    dam_media::video_probe_available()
}

use async_trait::async_trait;
use dam_api::admin::{
    CacheTarget, ClearAnalysisReport, ClearCacheReport, StorageUsage, VacuumReport, WipeReport,
};
use dam_api::dto::*;
use dam_api::event::{ChangeKind, LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, CollectionId, CommentId, ContentHash, JobId, SourceId};
use dam_api::page::{Page, PageParams};
use dam_api::service::{AuthContext, EventStream, LibraryService, Scope, Visibility};
use dam_api::{ItemWarning, LibError};
use dam_store::Store;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Clamp for a thumbnail's long edge (tech-spec 04 §6.4). Small enough that generation stays cheap
/// and the cache stays compact; large enough for a crisp inspector preview.
const THUMB_MIN_EDGE: u32 = 16;
const THUMB_MAX_EDGE: u32 = 1024;
/// One hint can enqueue at most this many distinct derivatives. The larger scan cap prevents a
/// duplicate-only request from consuming unbounded CPU while still allowing useful deduplication.
const PREFETCH_INPUT_CAP: usize = 512;
const PREFETCH_INPUT_SCAN_CAP: usize = PREFETCH_INPUT_CAP * 4;
/// Pending work across overlapping hints is bounded independently of the number of callers.
const PREFETCH_QUEUE_CAP: usize = 1024;
const PREFETCH_PEER_CAP: usize = 16;

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
fn thumb_cache_lookup(
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
fn gen_thumbnail(
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

fn preview_content(bytes: Vec<u8>) -> AssetContent {
    AssetContent {
        bytes,
        content_type: "model/x-dam-preview".to_string(),
        format: "dmsh".to_string(),
        media: MediaType::Model,
    }
}

struct ModelDerivativeContent {
    thumbnail: Option<Vec<u8>>,
    preview: Vec<u8>,
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
fn model_preview_cache_lookup(
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
fn model_preview_cache_lookup(
    _cache: &cache::Controller,
    _data_dir: &Path,
    _asset: &Asset,
) -> Option<AssetContent> {
    None
}

/// Generate both model derivatives from one fetch + Assimp decode. Interactive requests,
/// prefetch, and hosted warming share the same keyed flight around this function.
#[cfg(feature = "render")]
fn gen_model_derivatives(
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
fn gen_model_derivatives(
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

/// Serialise an optional saved query to JSON for the `collection.query` column.
fn serialize_opt_query(q: &Option<QueryRequest>) -> Result<Option<String>, LibError> {
    q.as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| LibError::Internal(e.to_string()))
}

/// The distinct sources an analyse run will touch — the job's visibility attribution (issue #42).
/// Sorted+deduped so the recorded set is stable regardless of target order.
pub(crate) fn distinct_sources(targets: &[dam_store::AnalysisTarget]) -> Vec<SourceId> {
    let set: std::collections::BTreeSet<SourceId> = targets.iter().map(|t| t.source_id).collect();
    set.into_iter().collect()
}

/// Emit a job's current progress as a `JobProgress` event (best-effort; a dropped read is skipped).
/// Shared by the scan and analyse job loops.
pub(crate) fn emit_progress(store: &Store, events: &broadcast::Sender<LibraryEvent>, job: &JobId) {
    if let Ok(js) = store.get_job_summary(job) {
        let _ = events.send(LibraryEvent::JobProgress(js));
    }
}

fn convert_warnings(report: &ConvertReport) -> Vec<String> {
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

fn convert_summary(report: &ConvertReport, state: JobState) -> String {
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

fn png_content(bytes: Vec<u8>) -> AssetContent {
    AssetContent {
        bytes,
        content_type: "image/png".to_string(),
        format: "png".to_string(),
        media: MediaType::Image,
    }
}

/// The account id behind a request, or `Forbidden` (issue #82).
///
/// Posting requires a **person**, not merely a credential. A bearer token has an identity string
/// but is typically a shared machine credential — attributing a conversation to one would be a
/// fiction — and an anonymous caller has nothing to attribute at all. The embedded engine likewise
/// has no account, which is consistent: discussion is a multi-user feature, and the single-user
/// local library has notes.
fn require_account(ctx: &AuthContext) -> Result<String, LibError> {
    ctx.account
        .as_ref()
        .map(|a| a.account_id.clone())
        .ok_or_else(|| {
            LibError::Forbidden("posting to a discussion requires a signed-in account".into())
        })
}

/// Map a media-handler fault onto the service error model (tech-spec 03 §5). `Unsupported` becomes
/// a 415 so the web thumbnail falls back to the honest typed tile.
fn map_handler_err(e: dam_media::HandlerError) -> LibError {
    match e {
        dam_media::HandlerError::Unsupported(s) => LibError::Unsupported(s),
        other => LibError::Internal(other.to_string()),
    }
}

/// Rebuild the asset's source backend and resolve its bytes to a private local temp path. Local
/// bytes are copied from an already-open root capability; remote bytes are downloaded. Pure/blocking.
fn fetch_asset(
    store: &Store,
    secrets: &credentials::SecretVault,
    asset: &Asset,
    scratch: &Path,
) -> Result<dam_sources::Fetched, LibError> {
    let conn = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
    let fs = dam_sources::open_source(&conn, scratch)?;
    fs.fetch(&asset.path)
}

/// Read an asset's bytes for a preview, bounded by the content cap. Both the cheap catalog size and
/// a live source stat are checked before any (possibly remote) fetch, so a file which grew since its
/// last scan cannot turn this materialising compatibility path into an unbounded allocation.
/// Pure/blocking — called inside a `spawn_blocking` closure.
fn read_asset_content(
    store: &Store,
    secrets: &credentials::SecretVault,
    asset: &Asset,
    scratch: &Path,
) -> Result<AssetContent, LibError> {
    let size = asset.summary.size;
    if size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "asset is {size} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes"
        )));
    }
    let conn = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
    let source = dam_sources::open_source(&conn, scratch)?;
    let live_size = source.content_stat(&asset.path)?.len;
    if live_size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "asset is {live_size} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes"
        )));
    }
    let fetched = source.fetch(&asset.path)?;
    let abs = fetched.path();
    let fetched_size = std::fs::metadata(abs)
        .map_err(|e| LibError::Internal(format!("stat fetched {}: {e}", abs.display())))?
        .len();
    if fetched_size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "asset is {fetched_size} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes"
        )));
    }
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

fn content_metadata(asset: &Asset, stat: dam_sources::ContentStat) -> AssetContentMetadata {
    AssetContentMetadata {
        len: stat.len,
        content_type: content_type_for(asset.summary.media, &asset.summary.format).to_string(),
        format: asset.summary.format.clone(),
        media: asset.summary.media,
        etag: asset.hash.map(|hash| format!("\"{hash}\"")),
    }
}

/// Open a bounded producer over one source. The queue holds at most two chunks (plus the producer
/// and HTTP consumer's current chunks): `blocking_send` applies backpressure, and receiver drop
/// tells every source backend to stop its range loop promptly.
fn source_content_stream(
    source: Box<dyn dam_sources::FileSource>,
    path: String,
    metadata: AssetContentMetadata,
    range: ContentRange,
) -> AssetContentStream {
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    tokio::task::spawn_blocking(move || {
        let mut send = |chunk| sender.blocking_send(Ok(chunk)).is_ok();
        if let Err(error) = source.read_range(&path, range.first(), range.len(), &mut send) {
            let _ = sender.blocking_send(Err(error));
        }
    });
    AssetContentStream {
        metadata,
        range,
        bytes: Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver)),
    }
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
    let bytes = rel.as_bytes();
    let windows_drive =
        bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic();
    let uri_scheme = rel.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme.chars().enumerate().all(|(i, c)| {
                c.is_ascii_alphabetic() || (i > 0 && (c.is_ascii_digit() || "+-.".contains(c)))
            })
    });
    if rel.starts_with('/') || rel.starts_with('\\') || windows_drive || uri_scheme {
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
fn read_related_content(
    store: &Store,
    secrets: &credentials::SecretVault,
    asset: &Asset,
    rel: &str,
    scratch: &Path,
) -> Result<AssetContent, LibError> {
    let target = resolve_sibling(&asset.path, rel)?;
    let conn = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
    let fs = dam_sources::open_source(&conn, scratch)?;
    let size = fs.content_stat(&target)?.len;
    if size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "related file is {} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes",
            size
        )));
    }
    let fetched = fs.fetch(&target)?;
    let abs = fetched.path();
    let fetched_size = std::fs::metadata(abs)
        .map_err(|e| LibError::Internal(format!("stat fetched {}: {e}", abs.display())))?
        .len();
    if fetched_size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "related file is {fetched_size} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes"
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
    /// Host-local resolver for the opaque `source.auth_ref` values in the portable catalog.
    secrets: credentials::SecretVault,
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
    /// Host-pressure governor (tech-spec 14 §3.4): background loops pace themselves against it so a
    /// whole-library pass yields when the host runs short on memory or CPU, or its disks are
    /// stalled under bulk I/O.
    governor: Arc<resources::Governor>,
    /// Byte-bounded derivative cache, O(1) accounting, LRU eviction, and keyed flights.
    cache: Arc<cache::Controller>,
    /// One bounded/deduplicated queue feeds the background pool for all prefetch calls.
    prefetch_pending: Arc<Mutex<HashSet<(AssetId, u32)>>>,
    prefetch_running: Arc<AtomicBool>,
    prefetch_max_pending: Arc<AtomicUsize>,
    /// At most one sequential federation fan-out task per library; hints are optional and may be
    /// coalesced while it is busy.
    prefetch_peer_running: Arc<AtomicBool>,
    /// Wake handle for the background pipeline's drain worker (issue #71).
    ///
    /// Held on the engine rather than owned privately by `start_background_pipeline` so that an
    /// ingest which is *not* a scan can ask for a drain. Upload (issue #80) is the first: it writes
    /// a catalog row directly, and the pipeline's only other trigger is a `Scan` job reaching
    /// `Done`, so without this an uploaded asset would get its cheap tier and then wait for an
    /// unrelated scan before it ever received an embedding or auto-tags — invisible to `similar`
    /// and `dedup` in the meantime, and on an unwatched source that could be indefinitely.
    pipeline_wake: Arc<tokio::sync::Notify>,
}

impl EmbeddedLibrary {
    /// Open (creating if needed) the library rooted at `data_dir`, with default resource options
    /// (environment overrides honoured — see [`ResourceOptions::from_env`]).
    pub async fn open(data_dir: &Path) -> Result<EmbeddedLibrary, LibError> {
        EmbeddedLibrary::open_with(data_dir, ResourceOptions::default()).await
    }

    /// Open with explicit resource knobs (the server passes its `[resources]` config through
    /// here). Unset knobs fall back to the `3DAM_BG_THREADS` / `3DAM_MIN_FREE_MEMORY_MB` /
    /// `3DAM_MAX_IO_STALL_PCT` environment variables, then to host-derived defaults.
    pub async fn open_with(
        data_dir: &Path,
        resources: ResourceOptions,
    ) -> Result<EmbeddedLibrary, LibError> {
        Self::open_with_cache(data_dir, resources, CacheOptions::default()).await
    }

    /// Open with explicit resource and derivative-cache budgets.
    pub async fn open_with_cache(
        data_dir: &Path,
        resources: ResourceOptions,
        cache_options: CacheOptions,
    ) -> Result<EmbeddedLibrary, LibError> {
        let resources = resources.or_env();
        // Scratch for fetched byte copies (issue #87). Created up front so `temp_sink` never has to,
        // and swept of anything a previous run left behind: `Fetched::Temp` cleans up on drop, but
        // a kill -9 mid-fetch can strand a multi-gigabyte file with nothing to collect it.
        let scratch = paths::scratch_dir(data_dir);
        if let Err(e) = std::fs::create_dir_all(&scratch) {
            tracing::warn!(dir = %scratch.display(), error = %e, "could not create scratch dir; remote fetches will fall back to the OS temp dir");
        } else {
            dam_sources::clean_scratch(&scratch);
        }
        let dir = data_dir.to_path_buf();
        let (store, secrets) = tokio::task::spawn_blocking(move || {
            let store = Store::open(&dir)?;
            let secrets = credentials::SecretVault::for_host(&dir)?;
            credentials::migrate_legacy_credentials(&store, &secrets)?;
            credentials::cleanup_pending_credentials(&store, &secrets)?;
            Ok::<_, LibError>((store, secrets))
        })
        .await
        .map_err(|e| LibError::Internal(e.to_string()))??;
        let store = Arc::new(store);
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        // Built before the watch manager: watch-triggered delta scans are bulk readers too and
        // pace against the same governor as everything else (tech-spec 14 §3.4).
        let governor = Arc::new(resources::Governor::new(
            resources.min_free_memory_mb,
            resources.max_io_stall_pct,
        ));
        let watchers = watch::WatchManager::new(
            store.clone(),
            secrets.clone(),
            events.clone(),
            tokio::runtime::Handle::current(),
            governor.clone(),
            scratch.clone(),
        );
        // NB: watchers are *not* started here. Auto-rescan only makes sense for long-running roles
        // (serve/mcp), which call `start_watchers()` explicitly. A run-and-exit CLI command must not
        // register OS watches — they add nothing to a one-shot and their setup would outlive the
        // command (keeping the runtime from shutting down). See tech-spec 07 §3.1.
        // Load the semantic model if this build ships one (M4). `None` by default — the model-free
        // embeddings stand in — so this is a cheap, always-safe call.
        let semantic = semantic::load(data_dir).map(Arc::from);
        // The background pool is a guest on the host: sized from the *effective* CPU budget
        // (cgroup-aware), hard-capped by default, and its workers run reniced at the idle I/O
        // class so co-tenant workloads and interactive reads preempt them (tech-spec 14 §3.4).
        let bg_threads = resources::background_thread_count(resources.background_threads);
        tracing::info!(
            bg_threads,
            effective_cpus = resources::effective_cpus(),
            containerised = resources::is_resource_limited(),
            "background pool sized"
        );
        let bg_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(bg_threads)
            .thread_name(|i| format!("dam-bg-{i}"))
            .start_handler(|_| resources::deprioritize_current_thread())
            .build()
            .map_err(|e| LibError::Internal(e.to_string()))?;
        let cache = cache::Controller::new(data_dir, cache_options);
        // Build the one-time cache inventory on the bounded worker pool before request handlers can
        // reach it. A cold admin/media request therefore never recursively walks a large tree on a
        // Tokio runtime worker, while all later accounting remains O(1).
        let inventory_cache = cache.clone();
        let (inventory_tx, inventory_rx) = tokio::sync::oneshot::channel();
        bg_pool.spawn(move || {
            inventory_cache.initialize_now();
            let _ = inventory_tx.send(());
        });
        inventory_rx
            .await
            .map_err(|error| LibError::Internal(error.to_string()))?;
        Ok(EmbeddedLibrary {
            store,
            secrets,
            events,
            data_dir: data_dir.to_path_buf(),
            cancels: Mutex::new(HashMap::new()),
            watchers,
            bg_pool: Arc::new(bg_pool),
            semantic,
            fed: federation::PeerRegistry::new(),
            governor,
            cache,
            prefetch_pending: Arc::new(Mutex::new(HashSet::new())),
            prefetch_running: Arc::new(AtomicBool::new(false)),
            prefetch_max_pending: Arc::new(AtomicUsize::new(0)),
            prefetch_peer_running: Arc::new(AtomicBool::new(false)),
            pipeline_wake: Arc::new(tokio::sync::Notify::new()),
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

    /// Where remote fetches materialise their bytes (issue #87) — real disk under the data dir,
    /// never the OS temp dir. Cheap to recompute; handed to every `open_source` call.
    fn scratch(&self) -> PathBuf {
        paths::scratch_dir(&self.data_dir)
    }

    /// [`Self::scratch`], for transports that must stage bytes before handing them to the engine —
    /// the upload route streams a request body here (issue #80). Public for the same reason the
    /// directory exists: staging in `std::env::temp_dir()` would put a multi-gigabyte upload on
    /// tmpfs (issue #87), so callers need somewhere correct to put it rather than a default that
    /// silently reintroduces the bug.
    pub fn scratch_dir(&self) -> PathBuf {
        self.scratch()
    }

    /// Announce that an asset's thread changed (issue #82). Rides `AssetChanged`, which already
    /// carries `source_id` — the attribution `Visibility::allows_event` filters on — so a comment
    /// event cannot reach a subscriber who cannot see the asset.
    async fn emit_commented(&self, asset: AssetId) {
        let source_id = self
            .db(move |s| s.asset_source(&asset))
            .await
            .unwrap_or(None);
        let _ = self.events.send(LibraryEvent::AssetChanged {
            id: asset,
            source_id,
            kind: ChangeKind::Commented,
        });
    }

    /// Fill in [`SourceInfo::writable`] — can each of these accept an upload right now (issue #80)?
    ///
    /// The capability question itself belongs to `dam-sources`, which owns the write seam, so this
    /// only transports the answer: see [`dam_sources::writable_without_handshake`] for why a remote
    /// kind is answered statically rather than by opening it, and for the note that slice 7 must
    /// update *that* function rather than this one.
    ///
    /// Runs on the blocking pool because probing a local root touches the filesystem, and a
    /// `LocalFs` source can perfectly well be a mounted network share underneath.
    async fn mark_writable(&self, sources: &mut [SourceInfo]) {
        let ids: Vec<SourceId> = sources.iter().map(|s| s.id).collect();
        let secrets = self.secrets.clone();
        let probed = self
            .db(move |s| {
                Ok(ids
                    .into_iter()
                    .map(|id| {
                        let result = s
                            .get_source_connection(&id)
                            .and_then(|connection| secrets.resolve(connection))
                            .map(|connection| dam_sources::writable_without_handshake(&connection));
                        (id, result)
                    })
                    .collect::<Vec<_>>())
            })
            .await
            .unwrap_or_default();
        for s in sources.iter_mut() {
            match probed.iter().find(|(id, _)| *id == s.id).map(|(_, r)| r) {
                Some(Ok(writable)) => s.writable = *writable,
                Some(Err(error)) => {
                    s.writable = false;
                    // Credential errors are deliberately generic and contain no ref, secret, host
                    // path, or platform-provider detail. Listing therefore makes a locked/missing
                    // store explicit without turning it into a disclosure surface.
                    s.state = SourceState::Error(error.to_string());
                }
                None => s.writable = false,
            }
        }
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

    /// Attach the authenticated transport actor to a newly-created background job. This remains
    /// inherent (rather than part of `LibraryService`) because attribution is server-boundary
    /// metadata, not a caller-controlled library operation.
    pub async fn set_job_initiator(&self, id: &JobId, initiator: String) -> Result<(), LibError> {
        let id = *id;
        self.db(move |s| s.set_job_initiator(&id, &initiator)).await
    }

    // ── visibility enforcement (tech-spec 10 §4.3, issue #42) ────────────────
    //
    // The ceiling is resolved once at auth time (server) and enforced here, in the engine, as a
    // query predicate — so search, similarity, dedup, stats, folders, collections, previews, and
    // export all filter through the same few store choke points and no transport handler can
    // forget. An unreachable resource answers `NotFound`/absent, never `Forbidden` — existence is
    // part of what the ceiling hides.

    /// Restricted contexts may not run library-wide operations (source management, scans,
    /// analysis, blocklist edits, conversion): the reachable set is per-resource, but these
    /// operations span the whole catalog.
    fn require_full_visibility(ctx: &AuthContext, what: &str) -> Result<(), LibError> {
        if ctx.visibility.is_full() {
            Ok(())
        } else {
            Err(LibError::Forbidden(format!(
                "{what} requires unrestricted visibility"
            )))
        }
    }

    /// Guard a single-asset read: an asset outside the ceiling answers `NotFound`,
    /// indistinguishable from a nonexistent id.
    async fn require_asset_visible(&self, ctx: &AuthContext, id: &AssetId) -> Result<(), LibError> {
        if ctx.visibility.is_full() {
            return Ok(());
        }
        let vis = ctx.visibility.clone();
        let id = *id;
        let ok = self.db(move |s| s.asset_visible(&id, &vis)).await?;
        if ok {
            Ok(())
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        }
    }

    /// Guard a single-asset write: the asset must be *write*-reachable (a `write` share on its
    /// source or a containing shared collection). Unreachable-for-read stays `NotFound`; readable
    /// but not writable is `Forbidden` — the share level, not existence, is what's denied here.
    async fn require_asset_writable(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<(), LibError> {
        if ctx.visibility.is_full() {
            return Ok(());
        }
        self.require_asset_visible(ctx, id).await?;
        let wv = ctx.visibility.write_view();
        let id = *id;
        let ok = self.db(move |s| s.asset_visible(&id, &wv)).await?;
        if ok {
            Ok(())
        } else {
            Err(LibError::Forbidden(
                "no write access to this asset (a write share is required)".into(),
            ))
        }
    }

    /// Guard a collection write: `NotFound` when unreachable, `Forbidden` without a write share.
    async fn require_collection_writable(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<(), LibError> {
        if ctx.visibility.is_full() {
            return Ok(());
        }
        let vis = ctx.visibility.clone();
        let cid = *id;
        let visible = self.db(move |s| s.collection_visible(&cid, &vis)).await?;
        if !visible {
            return Err(LibError::NotFound(format!("collection {id}")));
        }
        if ctx.visibility.allows_collection_write(id) {
            Ok(())
        } else {
            Err(LibError::Forbidden(
                "no write access to this collection (a write share is required)".into(),
            ))
        }
    }

    /// The local-index query path — the pre-federation body of [`LibraryService::query`], shared
    /// by the plain path and the fan-out engine (which merges this page with the peers').
    pub(crate) async fn local_query(
        &self,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        self.local_query_vis(req, Visibility::Full).await
    }

    /// [`Self::local_query`] under a visibility ceiling (issue #42) — the ceiling composes into the
    /// store's WHERE clause, so lexical, hybrid, and semantic paths all filter identically.
    pub(crate) async fn local_query_vis(
        &self,
        req: QueryRequest,
        vis: Visibility,
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
            s.query_assets_semantic(&req, text_vec, &vis)
        })
        .await
    }

    /// The `media type → EmbeddingSpace id` map this instance ranks similarity in — what
    /// `advertise()` publishes so peers can gate cross-peer similarity on an exact space match
    /// (phase 6, issue #40). Model-free v1 spaces by default; a loaded semantic model overrides
    /// its media with the model-backed space id.
    pub fn embedding_spaces(&self) -> std::collections::BTreeMap<String, String> {
        let mut spaces = std::collections::BTreeMap::new();
        for &media in MediaType::ALL {
            // A loaded model that doesn't cover this media reports an empty space id (the composite
            // has no checkpoint for video or prose); fall back to the model-free space the analyse
            // pass actually wrote into, so the advertised name is never one nothing was indexed under.
            let id = match &self.semantic {
                Some(m) => match m.space_id(media) {
                    s if s.is_empty() => analysis::model_free_space(media).to_string(),
                    s => s,
                },
                None => analysis::model_free_space(media).to_string(),
            };
            spaces.insert(media.as_str().to_string(), id);
        }
        spaces
    }

    /// Diagnostic counter used by cache stress tests and operator troubleshooting. One increment
    /// represents one shared source fetch + model decode for a thumbnail/preview pair.
    #[doc(hidden)]
    pub fn model_derivative_generation_count(&self) -> u64 {
        self.cache.model_derivative_generations()
    }

    /// `(maximum pending derivatives, maximum local worker tasks)`. The latter is one by
    /// construction; exposing it keeps the prefetch stress contract observable.
    #[doc(hidden)]
    pub fn prefetch_bound_diagnostics(&self) -> (usize, usize) {
        let pending = self.prefetch_max_pending.load(Ordering::Relaxed);
        (pending, usize::from(pending > 0))
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

    /// Report on-disk usage: DB sizes, local/peer cache tiers, metrics, and catalog counts.
    /// Read-only. Cache footprints/counters come from the controller's live inventory (no directory
    /// walk); DB sizes and catalog counts are always live.
    pub async fn storage_usage(&self) -> Result<StorageUsage, LibError> {
        let stats = self.db(|s| s.stats(None, &Visibility::Full)).await?;
        let thumbnails = self.cache.usage(cache::Tier::Thumbnail);
        let previews = self.cache.usage(cache::Tier::Preview);
        let peer_previews = self.cache.usage(cache::Tier::Peer);
        let data_dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let file_len = |p: PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            Ok(StorageUsage {
                data_dir: data_dir.display().to_string(),
                library_db_bytes: file_len(data_dir.join("library.db")),
                server_db_bytes: file_len(data_dir.join("server.db")),
                thumbnails,
                previews,
                peer_previews,
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
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            let tiers: &[cache::Tier] = match target {
                CacheTarget::Thumbnails => &[cache::Tier::Thumbnail],
                CacheTarget::Previews => &[cache::Tier::Preview],
                CacheTarget::All => &[
                    cache::Tier::Thumbnail,
                    cache::Tier::Preview,
                    cache::Tier::Peer,
                ],
            };
            let (bytes_freed, files_deleted) = cache.clear(tiers);
            ClearCacheReport {
                bytes_freed,
                files_deleted,
            }
        })
        .await
        .map_err(|e| LibError::Internal(e.to_string()))
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
        let secrets = self.secrets.clone();
        let report = self
            .db(move |s| {
                let report = s.wipe_catalog()?;
                credentials::cleanup_pending_credentials(s, &secrets)?;
                Ok(report)
            })
            .await?;
        let _ = self.events.send(LibraryEvent::CatalogReset);
        Ok(report)
    }
}

/// Delete every cached derivative keyed to one asset — its thumbnail PNGs (across edges + renderer
/// variants) and its 3D preview blob — returning how many files were removed. Both cache tiers are
/// flat and every entry is named `{key}-…`, so a prefix match cleanly scopes deletion to this asset's
/// slice without disturbing others. Best-effort per file (fail-soft, DESIGN_GUIDELINES §2).
fn purge_asset_cache(cache: &cache::Controller, key: &str) -> u64 {
    let prefix = format!("{key}-");
    cache.remove_prefix(cache::Tier::Thumbnail, &prefix)
        + cache.remove_prefix(cache::Tier::Preview, &prefix)
}

#[async_trait]
impl LibraryService for EmbeddedLibrary {
    async fn query(
        &self,
        ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        // Federated fan-out (phase 6, issue #39): merge local + peer pages when federated sources
        // are registered. `local_only` marks a peer-bound call — one hop, never transitive.
        // Restricted contexts never fan out (a peer's catalog is outside their reachable set);
        // their query runs locally under the ceiling predicate.
        if !req.local_only && ctx.visibility.is_full() {
            if let Some(page) = federation::federated_query(self, &req).await? {
                return Ok(page);
            }
        }
        self.local_query_vis(req, ctx.visibility.clone()).await
    }

    async fn get_asset(&self, ctx: &AuthContext, id: &AssetId) -> Result<Asset, LibError> {
        self.get_asset_from(ctx, id, None).await
    }

    async fn get_asset_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<Asset, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        // The detail read is the one path that surfaces collection membership, so it carries the
        // ceiling: an asset reached through a collection share must not enumerate the *other*
        // collections holding it (issue #42 — unreachable is absent, not merely denied).
        let vis = ctx.visibility.clone();
        match self.db(move |s| s.get_asset_detail(&id, &vis)).await {
            // A merged result can name a peer-owned asset: proxy the detail read (phase 6).
            // Never for a restricted context — the ceiling can't vouch for peer-owned ids.
            Err(LibError::NotFound(_)) if ctx.visibility.is_full() => {
                federation::proxy_get_asset(self, &id, source)
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            r => r,
        }
    }

    async fn read_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        self.read_content_from(ctx, id, None).await
    }

    async fn read_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        let local = self
            .db(move |s| {
                let asset = s.get_asset(&id)?;
                read_asset_content(s, &secrets, &asset, &scratch)
            })
            .await;
        match local {
            Err(LibError::NotFound(_)) if ctx.visibility.is_full() => {
                federation::proxy_read_content(self, &id, source)
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            r => r,
        }
    }

    async fn content_metadata(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContentMetadata, LibError> {
        self.content_metadata_from(ctx, id, None).await
    }

    async fn content_metadata_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContentMetadata, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        let local = self
            .db(move |store| {
                let asset = store.get_asset(&id)?;
                let connection = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
                let source = dam_sources::open_source(&connection, &scratch)?;
                let stat = source.content_stat(&asset.path)?;
                Ok(content_metadata(&asset, stat))
            })
            .await;
        match local {
            Err(LibError::NotFound(_)) if ctx.visibility.is_full() => {
                federation::proxy_content_metadata(self, &id, source)
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            result => result,
        }
    }

    async fn stream_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
    ) -> Result<AssetContentStream, LibError> {
        self.stream_content_from(ctx, id, range, None).await
    }

    async fn stream_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
        source: Option<SourceId>,
    ) -> Result<AssetContentStream, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        let local = self
            .db(move |store| {
                let asset = store.get_asset(&id)?;
                let connection = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
                let source = dam_sources::open_source(&connection, &scratch)?;
                let stat = source.content_stat(&asset.path)?;
                if range.last() >= stat.len {
                    return Err(LibError::BadRequest(
                        "content stream range is outside the representation".into(),
                    ));
                }
                let metadata = content_metadata(&asset, stat);
                Ok((source, asset.path, metadata))
            })
            .await;
        match local {
            Ok((source, path, metadata)) => {
                Ok(source_content_stream(source, path, metadata, range))
            }
            Err(LibError::NotFound(_)) if ctx.visibility.is_full() => {
                federation::proxy_stream_content(self, &id, range, source)
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            Err(error) => Err(error),
        }
    }

    async fn read_related_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
    ) -> Result<AssetContent, LibError> {
        self.read_related_content_from(ctx, id, rel, None).await
    }

    async fn read_related_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        let rel = rel.to_string();
        let rel2 = rel.clone();
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        let local = self
            .db(move |s| {
                let asset = s.get_asset(&id)?;
                read_related_content(s, &secrets, &asset, &rel2, &scratch)
            })
            .await;
        match local {
            Err(LibError::NotFound(_)) if ctx.visibility.is_full() => {
                federation::proxy_read_related(self, &id, &rel, source)
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            r => r,
        }
    }

    async fn read_thumbnail(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
    ) -> Result<AssetContent, LibError> {
        self.read_thumbnail_from(ctx, id, max_edge, None).await
    }

    async fn read_thumbnail_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        let edge = max_edge.clamp(THUMB_MIN_EDGE, THUMB_MAX_EDGE);
        let data_dir = self.data_dir.clone();
        let cache = self.cache.clone();
        let secrets = self.secrets.clone();
        // Fast path: a cheap cache probe on the unbounded pool, so an already-rendered thumbnail is
        // never stuck behind background generation.
        let probe_dir = data_dir.clone();
        let probe = self
            .db(move |s| {
                let asset = s.get_asset(&id)?;
                Ok((
                    thumb_cache_lookup(&cache, &probe_dir, &asset, edge),
                    asset.summary.media == MediaType::Model,
                ))
            })
            .await;
        let is_model = match probe {
            Ok((Some(hit), _)) => return Ok(hit),
            Ok((None, is_model)) => is_model,
            // Peer-owned asset: fetch its remote-owned preview — the one sanctioned federated byte
            // transfer (tech-spec 07 §4) — through the 7-day local peer cache. Full-visibility only.
            Err(LibError::NotFound(_)) if ctx.visibility.is_full() => {
                return federation::proxy_thumbnail(self, &id, edge, source)
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")))
            }
            Err(e) => return Err(e),
        };
        // Cache miss: the expensive render/decode runs on the bounded background pool so a grid
        // burst can't starve an interactive inspector read (preview / waveform / detail).
        let key = if is_model {
            format!("model-derivatives:{id}:{edge}")
        } else {
            format!("thumbnail:{id}:{edge}")
        };
        let cache = self.cache.clone();
        self.cache
            .singleflight(key, || async move {
                self.run_bg(move |s| {
                    let asset = s.get_asset(&id)?;
                    if is_model {
                        let derivatives =
                            gen_model_derivatives(&cache, &data_dir, s, &secrets, &asset, edge)?;
                        derivatives.thumbnail.map(png_content).ok_or_else(|| {
                            LibError::Unsupported("3D thumbnail generation failed".into())
                        })
                    } else {
                        gen_thumbnail(&cache, &data_dir, s, &secrets, &asset, edge)
                    }
                })
                .await
            })
            .await
    }

    async fn read_model_preview(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        self.read_model_preview_from(ctx, id, None).await
    }

    async fn read_model_preview_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        let data_dir = self.data_dir.clone();
        let cache = self.cache.clone();
        let secrets = self.secrets.clone();
        let probe_dir = data_dir.clone();
        let probe = self
            .db(move |s| {
                let asset = s.get_asset(&id)?;
                if asset.summary.media != MediaType::Model {
                    return Err(LibError::Unsupported(
                        "3D preview is only available for model assets".into(),
                    ));
                }
                Ok(model_preview_cache_lookup(&cache, &probe_dir, &asset))
            })
            .await;
        match probe {
            Ok(Some(hit)) => return Ok(hit),
            Ok(None) => {}
            Err(LibError::NotFound(_)) if ctx.visibility.is_full() => {
                return federation::proxy_model_preview(self, &id, source)
                    .await
                    .ok_or_else(|| LibError::NotFound(format!("asset {id}")));
            }
            Err(error) => return Err(error),
        }
        let edge = background::PREGEN_THUMB_EDGE;
        let key = format!("model-derivatives:{id}:{edge}");
        let cache = self.cache.clone();
        self.cache
            .singleflight(key, || async move {
                self.run_bg(move |store| {
                    let asset = store.get_asset(&id)?;
                    let derivatives =
                        gen_model_derivatives(&cache, &data_dir, store, &secrets, &asset, edge)?;
                    Ok(preview_content(derivatives.preview))
                })
                .await
            })
            .await
    }

    async fn prefetch(&self, ctx: &AuthContext, req: PrefetchRequest) -> Result<(), LibError> {
        // A pure optimisation: for a restricted context it's a safe no-op (the on-demand reads
        // still generate + guard), which keeps hidden ids from steering the warm path.
        if req.assets.is_empty() || !ctx.visibility.is_full() {
            return Ok(());
        }
        // Warm the exact thumbnail edge the client will request (the grid uses a variable edge the
        // hosted pipeline can't all pre-render). Input, queue, task, and CPU concurrency are all
        // bounded: overlapping calls feed one deduplicated queue and at most one bg-pool worker.
        let edge = req
            .edge
            .unwrap_or(background::PREGEN_THUMB_EDGE)
            .clamp(THUMB_MIN_EDGE, THUMB_MAX_EDGE);
        let mut unique = HashSet::new();
        for id in req.assets.into_iter().take(PREFETCH_INPUT_SCAN_CAP) {
            unique.insert(id);
            if unique.len() == PREFETCH_INPUT_CAP {
                break;
            }
        }
        let peer_assets: Vec<_> = unique.iter().copied().collect();
        let relay = req.relay;
        {
            let mut pending = self.prefetch_pending.lock().unwrap();
            for id in unique {
                if pending.len() == PREFETCH_QUEUE_CAP {
                    break;
                }
                pending.insert((id, edge));
            }
            self.prefetch_max_pending
                .fetch_max(pending.len(), Ordering::Relaxed);
        }
        if self
            .prefetch_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let store = self.store.clone();
            let secrets = self.secrets.clone();
            let data_dir = self.data_dir.clone();
            let cache = self.cache.clone();
            let pending = self.prefetch_pending.clone();
            let running = self.prefetch_running.clone();
            self.bg_pool.spawn(move || {
                loop {
                    let work = {
                        let mut pending = pending.lock().unwrap();
                        let Some(item) = pending.iter().next().copied() else {
                            // Publish idle while holding the queue lock. A concurrent producer either
                            // already sees `true`, or inserts after this and starts the next worker.
                            running.store(false, Ordering::Release);
                            break;
                        };
                        pending.remove(&item);
                        item
                    };
                    let (id, edge) = work;
                    let Ok(asset) = store.get_asset(&id) else {
                        continue; // vanished (or peer-owned) — fail-soft
                    };
                    let key = if asset.summary.media == MediaType::Model {
                        format!("model-derivatives:{id}:{edge}")
                    } else {
                        format!("thumbnail:{id}:{edge}")
                    };
                    let _ = cache.singleflight_blocking(key, || {
                        if asset.summary.media == MediaType::Model {
                            let _ = gen_model_derivatives(
                                &cache, &data_dir, &store, &secrets, &asset, edge,
                            );
                        } else {
                            let _ =
                                gen_thumbnail(&cache, &data_dir, &store, &secrets, &asset, edge);
                        }
                    });
                }
            });
        }

        // Preserve merged-grid warming without spawning one task per peer. One sequential fan-out
        // task is admitted per library, peers and ids are capped/deduplicated, and the relay bit
        // prevents mutually registered libraries from bouncing hints forever.
        if !relay
            && self
                .prefetch_peer_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let peers = self.fed_peers().await;
            let running = self.prefetch_peer_running.clone();
            tokio::spawn(async move {
                let request = PrefetchRequest {
                    assets: peer_assets,
                    edge: Some(edge),
                    relay: true,
                };
                for peer in peers.iter().take(PREFETCH_PEER_CAP) {
                    let _ = tokio::time::timeout(
                        federation::QUERY_DEADLINE,
                        peer.client
                            .prefetch(&AuthContext::embedded(), request.clone()),
                    )
                    .await;
                }
                running.store(false, Ordering::Release);
            });
        }
        Ok(())
    }

    async fn library_stats(
        &self,
        ctx: &AuthContext,
        source: Option<SourceId>,
    ) -> Result<LibraryStats, LibError> {
        let vis = ctx.visibility.clone();
        let Some(sid) = source else {
            return self.db(move |s| s.stats(None, &vis)).await;
        };
        // A source-scoped read of an unreachable source is absent, not an aggregate oracle.
        if !ctx.visibility.allows_source(&sid) {
            return Err(LibError::NotFound(format!("source {sid}")));
        }
        // A federated source's counts live on the peer — proxy the read so the sidebar shows the
        // peer's own numbers, as fresh as the call (phase 6). Local kinds scope the local catalog.
        if let Some(peer) = self
            .fed_peers()
            .await
            .iter()
            .find(|p| p.source_id == sid)
            .cloned()
        {
            return tokio::time::timeout(
                federation::QUERY_DEADLINE,
                peer.client.library_stats(&AuthContext::embedded(), None),
            )
            .await
            .map_err(|_| LibError::SourceUnavailable(format!("peer '{}' timed out", peer.name)))?;
        }
        self.db(move |s| s.stats(Some(&sid), &vis)).await
    }

    async fn convert(
        &self,
        ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<ConvertReport, LibError> {
        // Restricted contexts may convert only assets they can reach (the outputs land in a
        // server-side dir either way, which non-destructive §5.1 already confines).
        if !ctx.visibility.is_full() {
            for id in &req.inputs {
                self.require_asset_visible(ctx, id).await?;
            }
        }
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        self.db(move |s| convert::run_convert(s, &secrets, req, &scratch))
            .await
    }

    async fn submit_convert(
        &self,
        ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<JobId, LibError> {
        if !ctx.visibility.is_full() {
            let inputs = req.inputs.clone();
            let visibility = ctx.visibility.clone();
            let visible = self
                .db(move |store| store.assets_visible(&inputs, &visibility))
                .await?;
            if !visible {
                return Err(LibError::NotFound(
                    "one or more convert inputs are unavailable".into(),
                ));
            }
        }
        let total = req.inputs.len() as u64;
        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let (sources, collections): (Vec<SourceId>, Vec<CollectionId>) =
            if let Some(scope) = ctx.visibility.restricted() {
                (
                    scope.sources.iter().copied().collect(),
                    scope.collections.iter().copied().collect(),
                )
            } else {
                let inputs = req.inputs.clone();
                let sources = self.db(move |store| store.asset_sources(&inputs)).await?;
                (sources, Vec::new())
            };
        let job = self
            .db(move |store| {
                store.create_job_scoped(
                    JobKind::Convert,
                    &params,
                    Some(total),
                    &sources,
                    &collections,
                )
            })
            .await?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());
        let store = self.store.clone();
        let events = self.events.clone();
        let secrets = self.secrets.clone();
        let scratch = self.scratch();
        tokio::task::spawn_blocking(move || {
            let run = convert::run_convert_with_checkpoint(
                &store,
                &secrets,
                req,
                &scratch,
                |done, total, current| {
                    if cancel.load(Ordering::Relaxed) {
                        return false;
                    }
                    let _ = store.update_job_progress(
                        &job,
                        JobState::Running,
                        done,
                        Some(total),
                        current,
                    );
                    emit_progress(&store, &events, &job);
                    !cancel.load(Ordering::Relaxed)
                },
            );
            match run {
                Ok(run) => {
                    let result = JobResult::Convert(run.report.clone());
                    let warnings = convert_warnings(&run.report);
                    let state = if run.cancelled || cancel.load(Ordering::Relaxed) {
                        JobState::Cancelled
                    } else {
                        JobState::Done
                    };
                    let summary = convert_summary(&run.report, state);
                    let finished =
                        store.finish_job(&job, state, &summary, &warnings, Some(&result));
                    if state == JobState::Done && matches!(finished, Ok(false)) {
                        // Cancellation won the DB race after the worker sampled the flag. Preserve
                        // the itemized partial/full report without changing the Cancelled state.
                        let summary = convert_summary(&run.report, JobState::Cancelled);
                        let _ = store.finish_job(
                            &job,
                            JobState::Cancelled,
                            &summary,
                            &warnings,
                            Some(&result),
                        );
                    }
                }
                Err(error) if cancel.load(Ordering::Relaxed) => {
                    let _ = store.finish_job(
                        &job,
                        JobState::Cancelled,
                        "Convert cancelled before completion",
                        &[],
                        None,
                    );
                    tracing::debug!(%job, %error, "convert stopped after cancellation");
                }
                Err(error) => {
                    let _ = store.set_job_state(&job, JobState::Failed, Some(&error.to_string()));
                }
            }
            let _ = store.set_job_artifacts(
                &job,
                &[JobArtifact {
                    label: "Open convert report".into(),
                    route: Some(format!("/jobs?job={job}")),
                }],
            );
            emit_progress(&store, &events, &job);
        });
        Ok(job)
    }

    async fn upload(
        &self,
        ctx: &AuthContext,
        req: UploadRequest,
        staged: &std::path::Path,
    ) -> Result<UploadOutcome, LibError> {
        ctx.require(Scope::Write)?;
        if !ctx.visibility.allows_source(&req.source) {
            return Err(LibError::NotFound(format!("source {}", req.source)));
        }
        if !ctx.visibility.allows_source_write(&req.source) {
            return Err(LibError::Forbidden(
                "no write access to this source (a write share is required)".into(),
            ));
        }

        let scratch = self.scratch();
        let staged = staged.to_path_buf();
        let events = self.events.clone();
        let secrets = self.secrets.clone();
        let outcome = self
            .db(move |s| upload::run_upload(s, &secrets, &events, req, &staged, &scratch))
            .await?;

        // Ask the background pipeline for a drain. `ingest_one` writes only the cheap tier, and the
        // pipeline's other trigger is a `Scan` job completing — so without this an uploaded asset
        // would carry no embedding and no auto-tags (invisible to `similar`/`dedup`) until some
        // unrelated scan of that source happened to run. `Notify` holds one permit, so a
        // twenty-file drop coalesces into a single follow-up pass rather than twenty.
        if outcome.asset.is_some() {
            self.pipeline_wake.notify_one();
        }
        Ok(outcome)
    }

    async fn list_sources(&self, ctx: &AuthContext) -> Result<Vec<SourceInfo>, LibError> {
        let all = self.db(|s| s.list_sources()).await?;
        // An unshared source is absent from the listing (issue #42): filter, don't 403.
        let mut visible: Vec<SourceInfo> = all
            .into_iter()
            .filter(|s| ctx.visibility.allows_source(&s.id))
            .collect();
        self.mark_writable(&mut visible).await;
        for source in &mut visible {
            if !ctx.scopes.has(Scope::Write) {
                source.writable = false;
                source.writable_reason = Some("read-only — write scope is required".into());
            } else if !ctx.visibility.allows_source_write(&source.id) {
                source.writable = false;
                source.writable_reason = Some("read-only — a write share is required".into());
            }
        }
        Ok(visible)
    }

    async fn get_source(&self, ctx: &AuthContext, id: &SourceId) -> Result<SourceInfo, LibError> {
        if !ctx.visibility.allows_source(id) {
            return Err(LibError::NotFound(format!("source {id}")));
        }
        let sid = *id;
        let mut info = self
            .db(move |s| {
                s.get_source(&sid)?
                    .ok_or_else(|| LibError::NotFound(format!("source {sid}")))
            })
            .await?;
        self.mark_writable(std::slice::from_mut(&mut info)).await;
        if !ctx.scopes.has(Scope::Write) {
            info.writable = false;
            info.writable_reason = Some("read-only — write scope is required".into());
        } else if !ctx.visibility.allows_source_write(&info.id) {
            info.writable = false;
            info.writable_reason = Some("read-only — a write share is required".into());
        }
        Ok(info)
    }

    async fn list_folders(
        &self,
        ctx: &AuthContext,
        req: FolderListing,
    ) -> Result<Vec<FolderEntry>, LibError> {
        // Folder trees enumerate paths without touching assets (issue #42 leak audit): an
        // unreachable source has no tree. A collection-only grant reaches assets, not the tree.
        if !ctx.visibility.allows_source(&req.source) {
            return Err(LibError::NotFound(format!("source {}", req.source)));
        }
        // Normalise the prefix so the derived-tree SQL is well-defined: empty (root) or ending in `/`.
        let prefix = if req.prefix.is_empty() || req.prefix.ends_with('/') {
            req.prefix
        } else {
            format!("{}/", req.prefix)
        };
        let source = req.source;
        self.db(move |s| s.list_folders(&source, &prefix)).await
    }

    async fn add_source(&self, ctx: &AuthContext, req: AddSource) -> Result<SourceId, LibError> {
        Self::require_full_visibility(ctx, "adding a source")?;
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
        let id = SourceId::new();
        let credential = conn.take_credentials();
        let credential_ref = credential
            .as_ref()
            .map(|_| credentials::SecretVault::reference(&id));
        let secrets = self.secrets.clone();
        let stored_ref = credential_ref.clone();
        self.db(move |s| {
            // Commit the redacted row + deterministic opaque ref first. If the secret write then
            // fails, the row still tracks any provider-side partial success; cleanup deletes the
            // credential before the row, so no failure ordering can create an untracked secret.
            s.add_source_with_auth(id, &conn, &name, watch, stored_ref.as_deref())?;
            if let (Some(reference), Some(material)) = (stored_ref.as_deref(), credential.as_ref())
            {
                if let Err(error) = secrets.put(reference, material) {
                    // If deletion itself cannot be confirmed, retain the row/reference. It will
                    // list as locked/unavailable and remains recoverable; removing the row here
                    // would turn a possibly committed provider entry into an orphan.
                    if secrets.delete(reference).is_ok() {
                        let _ = s.remove_source(&id, false);
                    }
                    return Err(error);
                }
            }
            Ok(())
        })
        .await?;
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
        ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError> {
        Self::require_full_visibility(ctx, "removing a source")?;
        let id = *id;
        let secrets = self.secrets.clone();
        self.db(move |s| {
            s.remove_source(&id, req.keep_metadata)?;
            if !req.keep_metadata {
                credentials::cleanup_pending_credentials(s, &secrets)?;
            }
            Ok(())
        })
        .await?;
        self.fed.invalidate().await;
        // Peer derivatives are owner-scoped. Once the source is gone they must not linger until
        // their TTL; lifecycle cleanup is an explicit bounded-pool maintenance walk.
        let cache = self.cache.clone();
        let peer_dir = self
            .data_dir
            .join("cache")
            .join("peer")
            .join(id.to_string());
        self.run_bg(move |_| {
            cache.remove_tree(&peer_dir);
            Ok(())
        })
        .await?;
        Ok(())
    }

    // ── remove / blocklist (issue #21) ───────────────────────────────────────
    async fn remove_asset(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        req: RemoveAsset,
    ) -> Result<(), LibError> {
        self.require_asset_writable(ctx, id).await?;
        // Blocklisting is a library-wide policy (it affects every future scan), not a per-asset
        // edit — reserved for unrestricted identities.
        if req.block {
            Self::require_full_visibility(ctx, "blocklisting content")?;
        }
        let id = *id;
        // Capture the attribution *before* the delete — afterwards the row is gone and the event
        // could never be matched against a subscriber's ceiling (issue #42).
        let source_id = self.db(move |s| s.asset_source(&id)).await?;
        self.db(move |s| s.remove_asset(&id, req.block)).await?;
        // Live update: drop the row from every open grid/inspector (mirrors AssetAdded on scan).
        let _ = self
            .events
            .send(LibraryEvent::AssetRemoved { id, source_id });
        Ok(())
    }

    async fn list_blocklist(&self, ctx: &AuthContext) -> Result<Vec<BlockEntry>, LibError> {
        // Blocked hashes describe library-wide content a restricted caller may not reach — the
        // list is simply empty for them (absent, not forbidden).
        if !ctx.visibility.is_full() {
            return Ok(Vec::new());
        }
        self.db(|s| s.list_blocklist()).await
    }

    async fn unblock(&self, ctx: &AuthContext, hash: &ContentHash) -> Result<(), LibError> {
        Self::require_full_visibility(ctx, "editing the blocklist")?;
        let hash = *hash;
        self.db(move |s| s.unblock(&hash)).await
    }

    // ── collections / smart folders ──────────────────────────────────────────
    async fn list_collections(&self, ctx: &AuthContext) -> Result<Vec<Collection>, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| s.list_collections_vis(&vis)).await
    }

    async fn get_collection(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<Collection, LibError> {
        let id = *id;
        let vis = ctx.visibility.clone();
        self.db(move |s| {
            if !s.collection_visible(&id, &vis)? {
                return Err(LibError::NotFound(format!("collection {id}")));
            }
            let mut c = s.get_collection(&id, &vis)?;
            // A smart folder's count is the live match count — compute it on the single-item read,
            // under the caller's ceiling so the count is never an oracle for hidden matches.
            if c.kind == CollectionKind::Smart {
                let ids = s.query_asset_ids(&c.query.clone().unwrap_or_default(), &vis)?;
                c.count = Some(ids.len() as u64);
            }
            Ok(c)
        })
        .await
    }

    async fn create_collection(
        &self,
        ctx: &AuthContext,
        req: NewCollection,
    ) -> Result<CollectionId, LibError> {
        // v1: collections are library-level objects with no owner concept — a restricted identity
        // cannot create one (post-v1 ownership may relax this; ADR 0009 freeze).
        Self::require_full_visibility(ctx, "creating a collection")?;
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
        ctx: &AuthContext,
        id: &CollectionId,
        req: UpdateCollection,
    ) -> Result<(), LibError> {
        self.require_collection_writable(ctx, id).await?;
        // Replacing a smart folder's saved query changes what it *matches*, not what the caller
        // can reach — results stay ceiling-filtered — so a write share safely covers it.
        let id = *id;
        let query_json = serialize_opt_query(&req.query)?;
        let name = req.name.clone();
        self.db(move |s| s.update_collection(&id, name.as_deref(), query_json.as_deref()))
            .await
    }

    async fn delete_collection(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<(), LibError> {
        self.require_collection_writable(ctx, id).await?;
        let id = *id;
        self.db(move |s| s.delete_collection(&id)).await
    }

    async fn modify_collection_members(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        req: CollectionMembers,
    ) -> Result<(), LibError> {
        self.require_collection_writable(ctx, id).await?;
        // Every asset being *added* must itself be reachable — otherwise membership in a shared
        // collection would grant visibility of a hidden asset (issue #42 rule 5, inverted).
        if !ctx.visibility.is_full() {
            for a in &req.add {
                self.require_asset_visible(ctx, a).await?;
            }
        }
        let id = *id;
        let add = req.add.clone();
        let remove = req.remove.clone();
        self.db(move |s| s.modify_collection_members(&id, &add, &remove))
            .await
    }

    async fn collection_assets(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        page: PageParams,
    ) -> Result<Page<AssetSummary>, LibError> {
        let id = *id;
        let vis = ctx.visibility.clone();
        self.db(move |s| {
            if !s.collection_visible(&id, &vis)? {
                return Err(LibError::NotFound(format!("collection {id}")));
            }
            let coll = s.get_collection(&id, &vis)?;
            match coll.kind {
                CollectionKind::Manual => {
                    let items = s.collection_summaries(&id, page.clamped(500), &vis)?;
                    Ok(Page::new(items, None))
                }
                CollectionKind::Smart => {
                    // Live resolution: run the saved query with the caller's page window, under
                    // the caller's ceiling (a shared smart folder never widens the reachable set).
                    let mut q = coll.query.unwrap_or_default();
                    q.page = page;
                    s.query_assets_semantic(&q, None, &vis)
                }
            }
        })
        .await
    }

    async fn export(
        &self,
        ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<ExportReport, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| export::run_export(s, req, &vis)).await
    }

    async fn submit_export(
        &self,
        ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<JobId, LibError> {
        let total = (!req.assets.is_empty()).then_some(req.assets.len() as u64);
        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let (sources, collections): (Vec<SourceId>, Vec<CollectionId>) =
            if let Some(scope) = ctx.visibility.restricted() {
                (
                    scope.sources.iter().copied().collect(),
                    scope.collections.iter().copied().collect(),
                )
            } else {
                let sources = self
                    .db(|store| {
                        Ok(store
                            .list_sources()?
                            .into_iter()
                            .map(|s| s.id)
                            .collect::<Vec<_>>())
                    })
                    .await?;
                (sources, Vec::new())
            };
        let job = self
            .db(move |store| {
                store.create_job_scoped(JobKind::Export, &params, total, &sources, &collections)
            })
            .await?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());
        let store = self.store.clone();
        let events = self.events.clone();
        let vis = ctx.visibility.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = export::run_export_with_checkpoint(&store, req, &vis, |done| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(LibError::Cancelled);
                }
                let _ = store.update_job_progress(
                    &job,
                    JobState::Running,
                    done,
                    total,
                    Some("Encoding manifest"),
                );
                emit_progress(&store, &events, &job);
                Ok(())
            });
            match outcome {
                Ok(report) => {
                    let result = JobResult::Export(report.clone());
                    let state = if cancel.load(Ordering::Relaxed) {
                        JobState::Cancelled
                    } else {
                        JobState::Done
                    };
                    let summary = if state == JobState::Cancelled {
                        format!(
                            "Cancellation arrived after {} file(s) were committed; output was retained",
                            report.files_written
                        )
                    } else {
                        format!(
                            "Exported {} asset(s) to {} file(s)",
                            report.assets, report.files_written
                        )
                    };
                    let finished = store.finish_job(&job, state, &summary, &[], Some(&result));
                    if state == JobState::Done && matches!(finished, Ok(false)) {
                        let summary = format!(
                            "Cancellation arrived after {} file(s) were committed; output was retained",
                            report.files_written
                        );
                        let _ = store.finish_job(
                            &job,
                            JobState::Cancelled,
                            &summary,
                            &[],
                            Some(&result),
                        );
                    }
                }
                Err(LibError::Cancelled) => {
                    let _ = store.finish_job(
                        &job,
                        JobState::Cancelled,
                        "Export cancelled; staged output was removed",
                        &[],
                        None,
                    );
                }
                Err(error) if cancel.load(Ordering::Relaxed) => {
                    let _ = store.finish_job(
                        &job,
                        JobState::Cancelled,
                        "Export cancelled; staged output was removed",
                        &[],
                        None,
                    );
                    tracing::debug!(%job, %error, "export stopped after cancellation");
                }
                Err(error) => {
                    let _ = store.set_job_state(&job, JobState::Failed, Some(&error.to_string()));
                }
            }
            let _ = store.set_job_artifacts(
                &job,
                &[JobArtifact {
                    label: "Open export report".into(),
                    route: Some(format!("/jobs?job={job}")),
                }],
            );
            emit_progress(&store, &events, &job);
        });
        Ok(job)
    }

    async fn submit_scan(&self, ctx: &AuthContext, req: ScanRequest) -> Result<JobId, LibError> {
        Self::require_full_visibility(ctx, "scanning")?;
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
        // The resolved source set *is* the job's attribution (issue #42).
        let touched: Vec<SourceId> = sources.iter().map(|s| s.id).collect();
        let job = self
            .db(move |s| s.create_job(JobKind::Scan, &params, None, &touched))
            .await?;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());

        let store = self.store.clone();
        let secrets = self.secrets.clone();
        let events = self.events.clone();
        let governor = self.governor.clone();
        let scratch = self.scratch();
        tokio::task::spawn_blocking(move || {
            scan::run_scan(
                store, secrets, events, job, sources, mode, cancel, &governor, &scratch,
            );
        });

        Ok(job)
    }

    async fn submit_analyze(
        &self,
        ctx: &AuthContext,
        req: AnalyzeRequest,
    ) -> Result<JobId, LibError> {
        Self::require_full_visibility(ctx, "analysis")?;
        // Plan: resolve the due (or requested) targets up front so the job total is known (§1.2).
        let assets = req.assets.clone();
        let force = req.force;
        let secrets = self.secrets.clone();
        let targets = self
            .db(move |s| {
                let mut targets =
                    s.list_analysis_targets(analysis::PIPELINE_VERSION, force, &assets)?;
                for target in &mut targets {
                    target.connection = secrets.resolve(target.connection.clone())?;
                }
                Ok(targets)
            })
            .await?;
        if targets.is_empty() {
            return Err(LibError::BadRequest(
                "nothing to analyse (all assets are up to date; pass --force to re-run)".into(),
            ));
        }

        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let total = targets.len() as u64;
        let touched = distinct_sources(&targets);
        let job = self
            .db(move |s| s.create_job(JobKind::Analyze, &params, Some(total), &touched))
            .await?;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());

        let store = self.store.clone();
        let events = self.events.clone();
        let model = self.semantic.clone();
        let pool = self.bg_pool.clone();
        let governor = self.governor.clone();
        let scratch = self.scratch();
        tokio::task::spawn_blocking(move || {
            analysis::run_analyze(
                store, events, job, targets, cancel, model, &pool, &governor, &scratch,
            );
        });
        Ok(job)
    }

    async fn regenerate_thumbnails(
        &self,
        ctx: &AuthContext,
        req: ThumbnailRegenRequest,
    ) -> Result<ThumbnailRegenReport, LibError> {
        if !ctx.visibility.is_full() {
            for id in &req.assets {
                self.require_asset_writable(ctx, id).await?;
            }
        }
        let cache = self.cache.clone();
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
                report.files_deleted += purge_asset_cache(&cache, &key);
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
        // The seed asset itself must be reachable (a hidden id must not seed a ranking), and the
        // neighbour candidates are ceiling-filtered inside the store's summary fetch.
        self.require_asset_visible(ctx, &req.asset).await?;
        let (asset, k) = (req.asset, req.k);
        let filters = req.filters.clone();
        let vis = ctx.visibility.clone();
        let (space, hits) = self
            .db(move |s| s.similar(&asset, k, &filters, &vis))
            .await?;
        let hits = hits
            .into_iter()
            // Tag each hit with the space it was actually ranked in (the explanation, §3.2). This
            // is the `space_id` the store ranked against, not a string rebuilt from the media type:
            // spaces are not uniformly named (documents rank in `text-hash-v1`) and a model-backed
            // embedding ranks in its own space entirely, so reconstructing the label would state a
            // space the ranking never used.
            .map(|(asset, score)| SimilarHit {
                space: space.clone(),
                asset,
                score,
            })
            .collect::<Vec<_>>();
        if req.local_only || !ctx.visibility.is_full() {
            // Restricted contexts never fan out — peer catalogs sit outside their reachable set.
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
        ctx: &AuthContext,
        req: dam_api::VectorSimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        let vis = ctx.visibility.clone();
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
            .db(move |s| s.similar_by_vector(&space, &vector, k, &filters, &vis))
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
        ctx: &AuthContext,
        req: DupRequest,
    ) -> Result<Page<DupGroup>, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| s.duplicates(&req, &vis)).await
    }

    async fn duplicate_membership(
        &self,
        ctx: &AuthContext,
        req: DupMembershipRequest,
    ) -> Result<Vec<DupMembership>, LibError> {
        if req.assets.len() > DUP_MEMBERSHIP_ASSET_MAX {
            return Err(LibError::BadRequest(format!(
                "duplicate membership accepts at most {DUP_MEMBERSHIP_ASSET_MAX} assets"
            )));
        }
        let vis = ctx.visibility.clone();
        self.db(move |s| s.duplicate_membership(&req.assets, &vis))
            .await
    }

    async fn duplicate_group(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Option<DupGroup>, LibError> {
        let vis = ctx.visibility.clone();
        let asset = *asset;
        self.db(move |s| s.duplicate_group(&asset, &vis)).await
    }

    async fn duplicate_group_members(
        &self,
        ctx: &AuthContext,
        req: DupGroupMembersRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| s.duplicate_group_members(&req, &vis))
            .await
    }

    async fn review_suggestion(
        &self,
        ctx: &AuthContext,
        req: SuggestionReview,
    ) -> Result<(), LibError> {
        self.require_asset_writable(ctx, &req.asset).await?;
        let state = match req.action {
            ReviewAction::Accept => "confirmed",
            ReviewAction::Reject => "rejected",
        };
        let id = req.asset;
        let tag = req.tag.clone();
        self.db(move |s| s.set_tag_state(&id, &tag, state)).await?;
        let source_id = self.db(move |s| s.asset_source(&id)).await?;
        let _ = self.events.send(LibraryEvent::AssetChanged {
            id: req.asset,
            source_id,
            kind: ChangeKind::Retagged,
        });
        Ok(())
    }

    async fn edit_tags(
        &self,
        ctx: &AuthContext,
        mut req: TagEditRequest,
    ) -> Result<TagEditResult, LibError> {
        const TAG_NAME_MAX: usize = 64;
        const TAG_DELTA_MAX: usize = 50;
        const WARNING_MAX: usize = 50;

        ctx.require(Scope::Write)?;
        let selectors = usize::from(!req.assets.is_empty())
            + usize::from(req.collection.is_some())
            + usize::from(req.query.is_some());
        if selectors != 1 {
            return Err(LibError::BadRequest(
                "tag edit requires exactly one assets, collection, or query selector".into(),
            ));
        }
        if req.assets.len() > TAG_EDIT_EXPLICIT_MAX {
            return Err(LibError::BadRequest(format!(
                "tag edit accepts at most {TAG_EDIT_EXPLICIT_MAX} explicit assets"
            )));
        }
        let normalize = |tags: Vec<String>| -> Result<Vec<String>, LibError> {
            let mut normalized = std::collections::BTreeSet::new();
            for tag in tags {
                let tag = tag.trim().to_lowercase();
                if tag.is_empty() || tag.chars().count() > TAG_NAME_MAX {
                    return Err(LibError::BadRequest(format!(
                        "tag names must contain 1–{TAG_NAME_MAX} characters"
                    )));
                }
                normalized.insert(tag);
            }
            if normalized.len() > TAG_DELTA_MAX {
                return Err(LibError::BadRequest(format!(
                    "tag edit accepts at most {TAG_DELTA_MAX} additions or removals"
                )));
            }
            Ok(normalized.into_iter().collect())
        };
        req.add = normalize(std::mem::take(&mut req.add))?;
        req.remove = normalize(std::mem::take(&mut req.remove))?;
        if req.add.is_empty() && req.remove.is_empty() {
            return Err(LibError::BadRequest(
                "tag edit requires at least one addition or removal".into(),
            ));
        }
        if let Some(tag) = req.add.iter().find(|tag| req.remove.contains(tag)) {
            return Err(LibError::BadRequest(format!(
                "tag '{tag}' cannot be added and removed in one edit"
            )));
        }

        let read_vis = ctx.visibility.clone();
        let write_vis = ctx.visibility.write_view();
        let explicit = !req.assets.is_empty();
        let selector = req.clone();
        let (readable, writable) = self
            .db(move |store| {
                let candidates = if !selector.assets.is_empty() {
                    selector.assets.clone()
                } else if let Some(collection) = selector.collection {
                    if !store.collection_visible(&collection, &read_vis)? {
                        return Err(LibError::NotFound(format!("collection {collection}")));
                    }
                    let record = store.get_collection(&collection, &read_vis)?;
                    match record.kind {
                        CollectionKind::Manual => store.collection_member_ids(&collection)?,
                        CollectionKind::Smart => {
                            store.query_asset_ids(&record.query.unwrap_or_default(), &read_vis)?
                        }
                    }
                } else {
                    store.query_asset_ids(
                        selector.query.as_ref().expect("selector validated"),
                        &read_vis,
                    )?
                };
                let filter = |visibility: &Visibility| -> Result<Vec<AssetId>, LibError> {
                    let mut visible = std::collections::HashSet::new();
                    for chunk in candidates.chunks(500) {
                        visible.extend(store.visible_asset_ids(chunk, visibility)?);
                    }
                    let mut seen = std::collections::HashSet::new();
                    Ok(candidates
                        .iter()
                        .copied()
                        .filter(|id| visible.contains(id) && seen.insert(*id))
                        .collect())
                };
                let readable = filter(&read_vis)?;
                let writable_set: std::collections::HashSet<_> =
                    filter(&write_vis)?.into_iter().collect();
                let writable: Vec<AssetId> = readable
                    .iter()
                    .copied()
                    .filter(|id| writable_set.contains(id))
                    .collect();
                Ok((readable, writable))
            })
            .await?;

        let readable_set: std::collections::HashSet<_> = readable.iter().copied().collect();
        let writable_set: std::collections::HashSet<_> = writable.iter().copied().collect();
        let mut warnings = Vec::new();
        if explicit {
            for id in &req.assets {
                let warning = if !readable_set.contains(id) {
                    Some((
                        "target_unavailable",
                        "Asset is unavailable or outside your read scope",
                    ))
                } else if !writable_set.contains(id) {
                    Some((
                        "target_read_only",
                        "Asset is readable but requires a write share",
                    ))
                } else {
                    None
                };
                if let Some((code, message)) = warning {
                    if warnings.len() < WARNING_MAX {
                        warnings.push(ItemWarning {
                            subject: id.to_string(),
                            code: code.into(),
                            message: message.into(),
                        });
                    }
                }
            }
            let excluded = req.assets.len().saturating_sub(writable.len());
            if excluded > warnings.len() {
                warnings.push(ItemWarning {
                    subject: "selection".into(),
                    code: "warnings_truncated".into(),
                    message: format!(
                        "{excluded} targets were excluded; individual warning details are capped at {WARNING_MAX}"
                    ),
                });
            }
        } else if readable.len() > writable.len() {
            warnings.push(ItemWarning {
                subject: "selection".into(),
                code: "targets_excluded".into(),
                message: format!(
                    "{} readable local targets were excluded because they require a write share",
                    readable.len() - writable.len()
                ),
            });
        }

        let add = req.add.clone();
        let remove = req.remove.clone();
        let dry_run = req.dry_run;
        let mut outcome = self
            .db(move |store| store.edit_manual_tags(&writable, &add, &remove, dry_run))
            .await?;
        outcome.result.warnings = warnings;
        if !dry_run {
            for (id, source_id) in outcome.changed_assets {
                let _ = self.events.send(LibraryEvent::AssetChanged {
                    id,
                    source_id,
                    kind: ChangeKind::Retagged,
                });
            }
        }
        Ok(outcome.result)
    }

    async fn list_tags(
        &self,
        ctx: &AuthContext,
        mut req: TagListRequest,
    ) -> Result<Vec<TagInfo>, LibError> {
        ctx.require(Scope::Read)?;
        req.prefix = req
            .prefix
            .map(|prefix| prefix.trim().to_lowercase())
            .filter(|prefix| !prefix.is_empty());
        if req.prefix.as_ref().is_some_and(|prefix| prefix.len() > 64) {
            return Err(LibError::BadRequest(
                "tag prefix must be at most 64 bytes".into(),
            ));
        }
        let vis = ctx.visibility.clone();
        self.db(move |store| store.list_tags(req.prefix.as_deref(), req.limit, &vis))
            .await
    }

    async fn set_favorite(&self, ctx: &AuthContext, req: FavoriteRequest) -> Result<(), LibError> {
        self.require_asset_writable(ctx, &req.asset).await?;
        let id = req.asset;
        let on = req.favorite;
        self.db(move |s| s.set_favorite(&id, on)).await?;
        let source_id = self.db(move |s| s.asset_source(&id)).await?;
        let _ = self.events.send(LibraryEvent::AssetChanged {
            id: req.asset,
            source_id,
            kind: ChangeKind::Metadata,
        });
        Ok(())
    }

    async fn get_note(&self, ctx: &AuthContext, id: &AssetId) -> Result<Option<Note>, LibError> {
        self.require_asset_visible(ctx, id).await?;
        let id = *id;
        self.db(move |s| s.get_note(&id)).await
    }

    async fn set_note(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        req: NoteRequest,
    ) -> Result<Option<Note>, LibError> {
        self.require_asset_writable(ctx, id).await?;
        // Attribution is best-effort and deliberately loose (see `Note::updated_by`): the signed-in
        // account if there is one, else whatever identity the credential resolved to, else nobody —
        // which is the honest answer for a single-user local library.
        let by = ctx
            .account
            .as_ref()
            .map(|a| a.username.clone())
            .or_else(|| ctx.identity.clone());
        let aid = *id;
        let note = self
            .db(move |s| s.set_note(&aid, &req.body, by.as_deref()))
            .await?;
        let source_id = self.db(move |s| s.asset_source(&aid)).await?;
        let _ = self.events.send(LibraryEvent::AssetChanged {
            id: aid,
            source_id,
            kind: ChangeKind::NoteSet,
        });
        Ok(note)
    }

    async fn list_comments(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Vec<Comment>, LibError> {
        // Read access to the asset is the whole gate: a message body can quote a path or filename
        // from a source this caller was never meant to reach.
        self.require_asset_visible(ctx, asset).await?;
        let asset = *asset;
        self.db(move |s| s.list_comments(&asset)).await
    }

    async fn post_comment(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
        req: NewComment,
    ) -> Result<Comment, LibError> {
        self.require_asset_visible(ctx, asset).await?;
        let author = require_account(ctx)?;
        let body = req.body.trim().to_string();
        if body.is_empty() {
            return Err(LibError::BadRequest("a message needs a body".into()));
        }
        let aid = *asset;
        let reply_to = req.reply_to;
        let comment = self
            .db(move |s| s.add_comment(&aid, &author, &body, reply_to))
            .await?;
        self.emit_commented(aid).await;
        Ok(comment)
    }

    async fn edit_comment(
        &self,
        ctx: &AuthContext,
        id: &CommentId,
        req: EditComment,
    ) -> Result<Comment, LibError> {
        let cid = *id;
        let existing = self.db(move |s| s.get_comment(&cid)).await?;
        self.require_asset_visible(ctx, &existing.asset).await?;
        let author = require_account(ctx)?;
        // Author only — an admin may *remove* a message (moderation) but never rewrite one, since
        // an edited message still carries its original author's name.
        if existing.author.id != author {
            return Err(LibError::Forbidden(
                "only the author may edit a message".into(),
            ));
        }
        let body = req.body.trim().to_string();
        if body.is_empty() {
            return Err(LibError::BadRequest(
                "a message needs a body (delete it instead)".into(),
            ));
        }
        self.db(move |s| s.edit_comment(&cid, &body)).await?;
        let updated = self.db(move |s| s.get_comment(&cid)).await?;
        self.emit_commented(existing.asset).await;
        Ok(updated)
    }

    async fn delete_comment(&self, ctx: &AuthContext, id: &CommentId) -> Result<(), LibError> {
        let cid = *id;
        let existing = self.db(move |s| s.get_comment(&cid)).await?;
        self.require_asset_visible(ctx, &existing.asset).await?;
        // The author, or a moderator. Checked against the *scope*, never the role name — `Scope` is
        // the single place a capability gains meaning (tech-spec 10 §4.2).
        let is_author = ctx
            .account
            .as_ref()
            .is_some_and(|a| a.account_id == existing.author.id);
        if !is_author && !ctx.scopes.has(Scope::Admin) {
            return Err(LibError::Forbidden(
                "only the author or an admin may delete a message".into(),
            ));
        }
        self.db(move |s| s.delete_comment(&cid)).await?;
        self.emit_commented(existing.asset).await;
        Ok(())
    }

    /// A job is readable when every source it touches is within the caller's ceiling — see
    /// [`Visibility::allows_job`]. Outside it the job is *absent*, not forbidden.
    ///
    /// This is what lets a share-based identity watch its own scan finish: the WebSocket delivers
    /// `JobProgress`, the client invalidates its jobs query, and the refetch has to agree with the
    /// event or the status bar would blink empty (issue #42).
    async fn get_job(&self, ctx: &AuthContext, id: &JobId) -> Result<JobStatus, LibError> {
        let jid = *id;
        let job = self.db(move |s| s.get_job(&jid)).await?;
        if !ctx.visibility.allows_job(&job) {
            return Err(LibError::NotFound(format!("job {id}")));
        }
        Ok(job)
    }

    async fn list_jobs(
        &self,
        ctx: &AuthContext,
        req: JobListRequest,
    ) -> Result<Page<JobStatus>, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| s.list_jobs(&req, &vis)).await
    }

    async fn cancel_job(&self, ctx: &AuthContext, id: &JobId) -> Result<(), LibError> {
        // Read-then-write split, matching `require_asset_writable`: a job outside the ceiling is
        // absent (404), one inside it but cancellable only by an unrestricted identity is forbidden.
        // Cancelling is a library-wide act — a share grants the right to *watch* a job, not stop it.
        self.get_job(ctx, id).await?;
        Self::require_full_visibility(ctx, "cancelling a job")?;
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
        ctx: &AuthContext,
        _req: SubscribeRequest,
    ) -> Result<EventStream<LibraryEvent>, LibError> {
        let rx = self.events.subscribe();
        let vis = ctx.visibility.clone();
        // A bounded broadcast receiver reports lag explicitly. Turn that gap into a payload the
        // transport can forward so every consumer performs one resync instead of staying stale.
        //
        // Restricted subscribers get a per-event ceiling check rather than a blanket withhold
        // (issue #42). Every event now carries the attribution the check needs — `source_id` on the
        // per-asset variants, the touched `sources` on `JobStatus` — so the decision is a set lookup
        // on data already in hand, with no database round-trip per event per subscriber. The rule
        // itself lives in `Visibility::allows_event` so the engine and any future transport enforce
        // one definition, and so adding a `LibraryEvent` variant fails to compile until it is judged.
        let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |r| {
            let event = match r {
                Ok(ev) if vis.allows_event(&ev) => Some(ev),
                Ok(_) => None,
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(_)) => {
                    Some(LibraryEvent::StreamLagged)
                }
            };
            async move { event }
        });
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
        assert!(resolve_sibling("m/s.gltf", "C:\\windows\\system32").is_err());
        assert!(resolve_sibling("m/s.gltf", "C:drive-relative.bin").is_err());
        assert!(resolve_sibling("m/s.gltf", "http://evil/x").is_err());
        assert!(resolve_sibling("m/s.gltf", "file:/etc/passwd").is_err());
        // Empty is rejected.
        assert!(resolve_sibling("m/s.gltf", "  ").is_err());
    }
}
