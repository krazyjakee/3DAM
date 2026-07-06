//! `dam-core` — the pure engine. `EmbeddedLibrary` implements the `LibraryService` seam over the
//! store, media handlers, and sources (tech-spec 01 §3). No UI, transport, or GPU (ADR 0002).
//! This is the standalone, no-network path; `3dam-server` serves the same object over HTTP/WS.

mod convert;
mod paths;
mod scan;

pub use paths::default_data_dir;

use async_trait::async_trait;
use dam_api::dto::*;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, JobId, SourceId};
use dam_api::page::Page;
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

/// Render (or read from cache) a downscaled PNG thumbnail for an image asset. Pure/blocking — runs
/// inside `spawn_blocking`. The cache lives under `<data_dir>/cache/thumbnails/<key>-<edge>.png`,
/// keyed by content hash (falling back to the asset id) so identical bytes share one derivative.
fn gen_thumbnail(
    data_dir: &Path,
    source_root: &str,
    asset: &Asset,
    max_edge: u32,
) -> Result<AssetContent, LibError> {
    let key = asset
        .hash
        .map(|h| h.to_hex())
        .unwrap_or_else(|| asset.summary.id.to_string());
    let cache_dir = data_dir.join("cache").join("thumbnails");
    let cache_path = cache_dir.join(format!("{key}-{max_edge}.png"));
    if let Ok(bytes) = std::fs::read(&cache_path) {
        return Ok(png_content(bytes));
    }

    // Resolve + traversal-guard the source file (same rule as read_content).
    let rel = Path::new(&asset.path);
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(LibError::BadRequest(
            "asset path escapes its source root".to_string(),
        ));
    }
    let abs = Path::new(source_root).join(rel);
    let det = dam_media::Detected {
        media: asset.summary.media,
        format: asset.summary.format.clone(),
    };
    let thumb = dam_media::render_thumbnail(&abs, &det, max_edge).map_err(map_handler_err)?;

    // Best-effort cache write (a cold cache is a slow path, not an error).
    if std::fs::create_dir_all(&cache_dir).is_ok() {
        let tmp = cache_dir.join(format!(".{key}-{max_edge}.png.tmp"));
        if std::fs::write(&tmp, &thumb.bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &cache_path);
        }
    }
    Ok(png_content(thumb.bytes))
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

/// Resolve an asset to its on-disk file (source root + stored relative path) and read the bytes,
/// bounded and traversal-guarded. Pure/blocking — called inside a `spawn_blocking` closure.
fn read_asset_file(source_root: &str, asset: &Asset) -> Result<AssetContent, LibError> {
    let rel = Path::new(&asset.path);
    // Defence in depth: stored paths come from our own walk, but never let one escape the root.
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(LibError::BadRequest(
            "asset path escapes its source root".to_string(),
        ));
    }
    let size = asset.summary.size;
    if size > MAX_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "asset is {size} bytes; preview content is capped at {MAX_CONTENT_BYTES} bytes"
        )));
    }
    let abs = Path::new(source_root).join(rel);
    let bytes = std::fs::read(&abs)
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

/// The in-process engine. Cheap to clone the handle by wrapping in `Arc`.
pub struct EmbeddedLibrary {
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    #[allow(dead_code)]
    data_dir: PathBuf,
    cancels: Mutex<HashMap<JobId, Arc<AtomicBool>>>,
}

impl EmbeddedLibrary {
    /// Open (creating if needed) the library rooted at `data_dir`.
    pub async fn open(data_dir: &Path) -> Result<EmbeddedLibrary, LibError> {
        let dir = data_dir.to_path_buf();
        let store = tokio::task::spawn_blocking(move || Store::open(&dir))
            .await
            .map_err(|e| LibError::Internal(e.to_string()))??;
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Ok(EmbeddedLibrary {
            store: Arc::new(store),
            events,
            data_dir: data_dir.to_path_buf(),
            cancels: Mutex::new(HashMap::new()),
        })
    }

    /// Open at the platform default location.
    pub async fn open_default() -> Result<EmbeddedLibrary, LibError> {
        EmbeddedLibrary::open(&default_data_dir()).await
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
}

#[async_trait]
impl LibraryService for EmbeddedLibrary {
    async fn query(
        &self,
        _ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        self.db(move |s| s.query_assets(&req)).await
    }

    async fn get_asset(&self, _ctx: &AuthContext, id: &AssetId) -> Result<Asset, LibError> {
        let id = *id;
        self.db(move |s| s.get_asset(&id)).await
    }

    async fn read_content(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        let id = *id;
        self.db(move |s| {
            let asset = s.get_asset(&id)?;
            // Asset paths are stored relative to their source root (scan.rs); rejoin to read.
            let source = s
                .get_source(&asset.source_id)?
                .ok_or_else(|| LibError::NotFound(format!("source {}", asset.source_id)))?;
            read_asset_file(&source.uri, &asset)
        })
        .await
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
        self.db(move |s| {
            let asset = s.get_asset(&id)?;
            let source = s
                .get_source(&asset.source_id)?
                .ok_or_else(|| LibError::NotFound(format!("source {}", asset.source_id)))?;
            gen_thumbnail(&data_dir, &source.uri, &asset, edge)
        })
        .await
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

    async fn add_source(&self, _ctx: &AuthContext, req: AddSource) -> Result<SourceId, LibError> {
        if req.kind != SourceKind::LocalFs {
            return Err(LibError::Unsupported(format!(
                "source kind {:?} is not supported in this build (phase 1 = local filesystem)",
                req.kind
            )));
        }
        // Normalise the path and check it exists up front (fail early on an obvious typo).
        let path = PathBuf::from(&req.uri);
        if !path.exists() {
            return Err(LibError::BadRequest(format!(
                "path does not exist: {}",
                req.uri
            )));
        }
        let uri = path
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(req.uri.clone());
        let name = req.name.clone().unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| uri.clone())
        });
        let watch = req.options.watch;
        self.db(move |s| s.add_source(SourceKind::LocalFs, &uri, &name, watch))
            .await
    }

    async fn remove_source(
        &self,
        _ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError> {
        let id = *id;
        self.db(move |s| s.remove_source(&id, req.keep_metadata))
            .await
    }

    async fn submit_scan(&self, _ctx: &AuthContext, req: ScanRequest) -> Result<JobId, LibError> {
        // Resolve target sources (all local_fs when none specified).
        let all = self.db(|s| s.list_sources()).await?;
        let sources: Vec<SourceInfo> = if req.sources.is_empty() {
            all.into_iter()
                .filter(|s| s.kind == SourceKind::LocalFs)
                .collect()
        } else {
            all.into_iter()
                .filter(|s| req.sources.contains(&s.id))
                .collect()
        };
        if sources.is_empty() {
            return Err(LibError::BadRequest(
                "no scannable (local filesystem) sources selected".into(),
            ));
        }

        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let job = self
            .db(move |s| s.create_job(JobKind::Scan, &params, None))
            .await?;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());

        let store = self.store.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            scan::run_scan(store, events, job, sources, cancel);
        });

        Ok(job)
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
