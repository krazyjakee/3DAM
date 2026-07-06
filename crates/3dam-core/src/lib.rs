//! `dam-core` — the pure engine. `EmbeddedLibrary` implements the `LibraryService` seam over the
//! store, media handlers, and sources (tech-spec 01 §3). No UI, transport, or GPU (ADR 0002).
//! This is the standalone, no-network path; `3dam-server` serves the same object over HTTP/WS.

mod analysis;
mod convert;
mod export;
mod paths;
mod scan;
mod watch;

pub use paths::default_data_dir;

use async_trait::async_trait;
use dam_api::dto::*;
use dam_api::event::{ChangeKind, LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, CollectionId, JobId, SourceId};
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

/// Render (or read from cache) a downscaled PNG thumbnail for an image asset. Pure/blocking — runs
/// inside `spawn_blocking`. The cache lives under `<data_dir>/cache/thumbnails/<key>-<edge>.png`,
/// keyed by content hash (falling back to the asset id) so identical bytes share one derivative.
fn gen_thumbnail(
    data_dir: &Path,
    store: &Store,
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
        return Ok(png_content(bytes)); // cache hit → no source access at all
    }

    // Cache miss: resolve the source file (in place for local, downloaded for remote). `fetch`
    // guards `..` traversal out of the source root.
    let fetched = fetch_asset(store, asset)?;
    let det = dam_media::Detected {
        media: asset.summary.media,
        format: asset.summary.format.clone(),
    };
    let thumb = dam_media::render_thumbnail(fetched.path(), &det, max_edge).map_err(map_handler_err)?;

    // Best-effort cache write (a cold cache is a slow path, not an error).
    if std::fs::create_dir_all(&cache_dir).is_ok() {
        let tmp = cache_dir.join(format!(".{key}-{max_edge}.png.tmp"));
        if std::fs::write(&tmp, &thumb.bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &cache_path);
        }
    }
    Ok(png_content(thumb.bytes))
}

/// Serialise an optional saved query to JSON for the `collection.query` column.
fn serialize_opt_query(q: &Option<QueryRequest>) -> Result<Option<String>, LibError> {
    q.as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| LibError::Internal(e.to_string()))
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

/// The in-process engine. Cheap to clone the handle by wrapping in `Arc`.
pub struct EmbeddedLibrary {
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    data_dir: PathBuf,
    cancels: Mutex<HashMap<JobId, Arc<AtomicBool>>>,
    /// Auto-rescan watchers for `watch`-enabled sources (tech-spec 07 §3.1).
    watchers: watch::WatchManager,
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
        let watchers =
            watch::WatchManager::new(store.clone(), events.clone(), tokio::runtime::Handle::current());
        // NB: watchers are *not* started here. Auto-rescan only makes sense for long-running roles
        // (serve/mcp), which call `start_watchers()` explicitly. A run-and-exit CLI command must not
        // register OS watches — they add nothing to a one-shot and their setup would outlive the
        // command (keeping the runtime from shutting down). See tech-spec 07 §3.1.
        Ok(EmbeddedLibrary {
            store,
            events,
            data_dir: data_dir.to_path_buf(),
            cancels: Mutex::new(HashMap::new()),
            watchers,
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
            read_asset_content(s, &asset)
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
            gen_thumbnail(&data_dir, s, &asset, edge)
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

        let name = req.name.clone().unwrap_or(default_name);
        let watch = req.options.watch;
        let id = self
            .db(move |s| s.add_source(&conn, &name, watch))
            .await?;
        // Start watching immediately if requested (tech-spec 07 §3.1).
        if watch {
            self.watchers.ensure(id);
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
            .await
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

    async fn export(&self, _ctx: &AuthContext, req: ExportRequest) -> Result<ExportReport, LibError> {
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
        tokio::task::spawn_blocking(move || {
            analysis::run_analyze(store, events, job, targets, cancel);
        });
        Ok(job)
    }

    async fn find_similar(
        &self,
        _ctx: &AuthContext,
        req: SimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        let SimilarRequest { asset, k, filters } = req;
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
