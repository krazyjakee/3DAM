//! `dam-core` — the pure engine. `EmbeddedLibrary` implements the `LibraryService` seam over the
//! store, media handlers, and sources (tech-spec 01 §3). No UI, transport, or GPU (ADR 0002).
//! This is the standalone, no-network path; `3dam-server` serves the same object over HTTP/WS.

mod analysis;
mod background;
mod cache;
mod content;
mod convert;
mod credentials;
mod derivatives;
mod export;
mod federation;
mod paths;
mod reliability;
mod resources;
mod scan;
pub mod semantic;
mod service;
mod upload;
mod visibility;
mod watch;
mod writer;

pub use background::PipelinePolicy;
pub use cache::CacheOptions;
/// Re-exported so a transport can stage an upload under a name the engine's scratch sweep knows
/// (issue #80). The server stages the request body itself but must not depend on `dam-sources`
/// directly — frontends reach the engine, not around it.
pub use dam_sources::UPLOAD_SCRATCH_PREFIX;
pub use paths::default_data_dir;
pub use resources::ResourceOptions;

/// Measure the production derivative cache's first existing-thumbnail hit while its startup
/// inventory runs concurrently. This narrow seam is public for `cargo xtask perf`; applications
/// should use [`LibraryService::thumbnail`] through [`EmbeddedLibrary`] instead.
#[doc(hidden)]
pub fn measure_derivative_cache_first_hit(
    data_dir: &Path,
    hit_path: &Path,
) -> std::io::Result<std::time::Duration> {
    cache::measure_first_thumbnail_hit(data_dir, hit_path)
}

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
use dam_api::LibError;
use dam_store::Store;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use content::*;
use derivatives::*;
use visibility::{require_account, require_single_selector};

const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Serialise an optional saved query to JSON for the `collection.query` column.
fn serialize_opt_query(q: &Option<QueryRequest>) -> Result<Option<String>, LibError> {
    q.as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| LibError::Internal(e.to_string()))
}

fn incompatible_smart_query() -> LibError {
    LibError::BadRequest(
        "this smart folder uses a saved query that this version cannot read; replace its query"
            .into(),
    )
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
        // Build the one-time cache inventory on the bounded worker pool without delaying startup.
        // Existing derivative hits remain readable while it runs; publications conservatively skip
        // caching until byte accounting is authoritative. The task owns an Arc and therefore may
        // safely outlive this constructor while the pool stays owned by the library.
        let inventory_cache = cache.clone();
        bg_pool.spawn(move || {
            inventory_cache.initialize_now();
        });
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
        reliability::publish_event(
            &self.events,
            LibraryEvent::AssetChanged {
                id: asset,
                source_id,
                kind: ChangeKind::Commented,
            },
            "publish comment change",
        );
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
        let report = tokio::task::spawn_blocking(move || {
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
        .map_err(|e| LibError::Internal(e.to_string()))?;
        // V23 is the durable derivative backlog. Clearing bytes must reset the corresponding work
        // marker or the background worker would believe the now-missing files were still warm.
        self.db(|store| store.mark_all_derivatives_pending())
            .await?;
        self.pipeline_wake.notify_one();
        Ok(report)
    }

    /// Drop the analysis layer (suggestions + embeddings + derived attrs) and mark every asset due
    /// for re-analysis, keeping user-confirmed tags. Emits `CatalogReset` so open grids refresh.
    pub async fn clear_analysis(&self) -> Result<ClearAnalysisReport, LibError> {
        let report = self.db(|s| s.clear_analysis()).await?;
        reliability::publish_event(
            &self.events,
            LibraryEvent::CatalogReset,
            "publish analysis reset",
        );
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
        reliability::publish_event(
            &self.events,
            LibraryEvent::CatalogReset,
            "publish catalog reset",
        );
        Ok(report)
    }
}

#[async_trait]
impl LibraryService for EmbeddedLibrary {
    async fn query(
        &self,
        ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        self.query_impl(ctx, req).await
    }

    async fn get_asset(&self, ctx: &AuthContext, id: &AssetId) -> Result<Asset, LibError> {
        self.get_asset_impl(ctx, id).await
    }

    async fn get_asset_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<Asset, LibError> {
        self.get_asset_from_impl(ctx, id, source).await
    }

    async fn read_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        self.read_content_impl(ctx, id).await
    }

    async fn read_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.read_content_from_impl(ctx, id, source).await
    }

    async fn content_metadata(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContentMetadata, LibError> {
        self.content_metadata_impl(ctx, id).await
    }

    async fn content_metadata_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContentMetadata, LibError> {
        self.content_metadata_from_impl(ctx, id, source).await
    }

    async fn stream_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
    ) -> Result<AssetContentStream, LibError> {
        self.stream_content_impl(ctx, id, range).await
    }

    async fn stream_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
        source: Option<SourceId>,
    ) -> Result<AssetContentStream, LibError> {
        self.stream_content_from_impl(ctx, id, range, source).await
    }

    async fn read_related_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
    ) -> Result<AssetContent, LibError> {
        self.read_related_content_impl(ctx, id, rel).await
    }

    async fn read_related_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.read_related_content_from_impl(ctx, id, rel, source)
            .await
    }

    async fn read_thumbnail(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
    ) -> Result<AssetContent, LibError> {
        self.read_thumbnail_impl(ctx, id, max_edge).await
    }

    async fn read_thumbnail_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.read_thumbnail_from_impl(ctx, id, max_edge, source)
            .await
    }

    async fn read_model_preview(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        self.read_model_preview_impl(ctx, id).await
    }

    async fn read_model_preview_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.read_model_preview_from_impl(ctx, id, source).await
    }

    async fn prefetch(&self, ctx: &AuthContext, req: PrefetchRequest) -> Result<(), LibError> {
        self.prefetch_impl(ctx, req).await
    }

    async fn library_stats(
        &self,
        ctx: &AuthContext,
        source: Option<SourceId>,
    ) -> Result<LibraryStats, LibError> {
        self.library_stats_impl(ctx, source).await
    }

    async fn convert(
        &self,
        ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<ConvertReport, LibError> {
        self.convert_impl(ctx, req).await
    }

    async fn submit_convert(
        &self,
        ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<JobId, LibError> {
        self.submit_convert_impl(ctx, req).await
    }

    async fn upload(
        &self,
        ctx: &AuthContext,
        req: UploadRequest,
        staged: &std::path::Path,
    ) -> Result<UploadOutcome, LibError> {
        self.upload_impl(ctx, req, staged).await
    }

    async fn list_sources(&self, ctx: &AuthContext) -> Result<Vec<SourceInfo>, LibError> {
        self.list_sources_impl(ctx).await
    }

    async fn get_source(&self, ctx: &AuthContext, id: &SourceId) -> Result<SourceInfo, LibError> {
        self.get_source_impl(ctx, id).await
    }

    async fn list_folders(
        &self,
        ctx: &AuthContext,
        req: FolderListing,
    ) -> Result<Vec<FolderEntry>, LibError> {
        self.list_folders_impl(ctx, req).await
    }

    async fn add_source(&self, ctx: &AuthContext, req: AddSource) -> Result<SourceId, LibError> {
        self.add_source_impl(ctx, req).await
    }

    async fn remove_source(
        &self,
        ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError> {
        self.remove_source_impl(ctx, id, req).await
    }

    async fn remove_asset(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        req: RemoveAsset,
    ) -> Result<(), LibError> {
        self.remove_asset_impl(ctx, id, req).await
    }

    async fn list_blocklist(&self, ctx: &AuthContext) -> Result<Vec<BlockEntry>, LibError> {
        self.list_blocklist_impl(ctx).await
    }

    async fn unblock(&self, ctx: &AuthContext, hash: &ContentHash) -> Result<(), LibError> {
        self.unblock_impl(ctx, hash).await
    }

    async fn list_collections(&self, ctx: &AuthContext) -> Result<Vec<Collection>, LibError> {
        self.list_collections_impl(ctx).await
    }

    async fn get_collection(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<Collection, LibError> {
        self.get_collection_impl(ctx, id).await
    }

    async fn create_collection(
        &self,
        ctx: &AuthContext,
        req: NewCollection,
    ) -> Result<CollectionId, LibError> {
        self.create_collection_impl(ctx, req).await
    }

    async fn update_collection(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        req: UpdateCollection,
    ) -> Result<(), LibError> {
        self.update_collection_impl(ctx, id, req).await
    }

    async fn delete_collection(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<(), LibError> {
        self.delete_collection_impl(ctx, id).await
    }

    async fn modify_collection_members(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        req: CollectionMembers,
    ) -> Result<(), LibError> {
        self.modify_collection_members_impl(ctx, id, req).await
    }

    async fn collection_assets(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        page: PageParams,
    ) -> Result<Page<AssetSummary>, LibError> {
        self.collection_assets_impl(ctx, id, page).await
    }

    async fn export(
        &self,
        ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<ExportReport, LibError> {
        self.export_impl(ctx, req).await
    }

    async fn submit_export(
        &self,
        ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<JobId, LibError> {
        self.submit_export_impl(ctx, req).await
    }

    async fn submit_scan(&self, ctx: &AuthContext, req: ScanRequest) -> Result<JobId, LibError> {
        self.submit_scan_impl(ctx, req).await
    }

    async fn submit_analyze(
        &self,
        ctx: &AuthContext,
        req: AnalyzeRequest,
    ) -> Result<JobId, LibError> {
        self.submit_analyze_impl(ctx, req).await
    }

    async fn regenerate_thumbnails(
        &self,
        ctx: &AuthContext,
        req: ThumbnailRegenRequest,
    ) -> Result<ThumbnailRegenReport, LibError> {
        self.regenerate_thumbnails_impl(ctx, req).await
    }

    async fn find_similar(
        &self,
        ctx: &AuthContext,
        req: SimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        self.find_similar_impl(ctx, req).await
    }

    async fn find_similar_by_vector(
        &self,
        ctx: &AuthContext,
        req: dam_api::VectorSimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        self.find_similar_by_vector_impl(ctx, req).await
    }

    async fn list_duplicates(
        &self,
        ctx: &AuthContext,
        req: DupRequest,
    ) -> Result<Page<DupGroup>, LibError> {
        self.list_duplicates_impl(ctx, req).await
    }

    async fn duplicate_membership(
        &self,
        ctx: &AuthContext,
        req: DupMembershipRequest,
    ) -> Result<Vec<DupMembership>, LibError> {
        self.duplicate_membership_impl(ctx, req).await
    }

    async fn duplicate_group(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Option<DupGroup>, LibError> {
        self.duplicate_group_impl(ctx, asset).await
    }

    async fn duplicate_group_members(
        &self,
        ctx: &AuthContext,
        req: DupGroupMembersRequest,
    ) -> Result<Page<DupMember>, LibError> {
        self.duplicate_group_members_impl(ctx, req).await
    }

    async fn review_duplicate(
        &self,
        ctx: &AuthContext,
        req: DupReviewRequest,
    ) -> Result<(), LibError> {
        self.review_duplicate_impl(ctx, req).await
    }

    async fn review_suggestion(
        &self,
        ctx: &AuthContext,
        req: SuggestionReview,
    ) -> Result<(), LibError> {
        self.review_suggestion_impl(ctx, req).await
    }

    async fn edit_tags(
        &self,
        ctx: &AuthContext,
        req: TagEditRequest,
    ) -> Result<TagEditResult, LibError> {
        self.edit_tags_impl(ctx, req).await
    }

    async fn list_tags(
        &self,
        ctx: &AuthContext,
        req: TagListRequest,
    ) -> Result<Vec<TagInfo>, LibError> {
        self.list_tags_impl(ctx, req).await
    }

    async fn set_favorite(&self, ctx: &AuthContext, req: FavoriteRequest) -> Result<(), LibError> {
        self.set_favorite_impl(ctx, req).await
    }

    async fn set_license(
        &self,
        ctx: &AuthContext,
        req: SetLicenseRequest,
    ) -> Result<LicenseEditResult, LibError> {
        self.set_license_impl(ctx, req).await
    }

    async fn get_note(&self, ctx: &AuthContext, id: &AssetId) -> Result<Option<Note>, LibError> {
        self.get_note_impl(ctx, id).await
    }

    async fn set_note(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        req: NoteRequest,
    ) -> Result<Option<Note>, LibError> {
        self.set_note_impl(ctx, id, req).await
    }

    async fn list_comments(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Vec<Comment>, LibError> {
        self.list_comments_impl(ctx, asset).await
    }

    async fn post_comment(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
        req: NewComment,
    ) -> Result<Comment, LibError> {
        self.post_comment_impl(ctx, asset, req).await
    }

    async fn edit_comment(
        &self,
        ctx: &AuthContext,
        id: &CommentId,
        req: EditComment,
    ) -> Result<Comment, LibError> {
        self.edit_comment_impl(ctx, id, req).await
    }

    async fn delete_comment(&self, ctx: &AuthContext, id: &CommentId) -> Result<(), LibError> {
        self.delete_comment_impl(ctx, id).await
    }

    async fn get_job(&self, ctx: &AuthContext, id: &JobId) -> Result<JobStatus, LibError> {
        self.get_job_impl(ctx, id).await
    }

    async fn list_jobs(
        &self,
        ctx: &AuthContext,
        req: JobListRequest,
    ) -> Result<Page<JobStatus>, LibError> {
        self.list_jobs_impl(ctx, req).await
    }

    async fn cancel_job(&self, ctx: &AuthContext, id: &JobId) -> Result<(), LibError> {
        self.cancel_job_impl(ctx, id).await
    }

    async fn subscribe(
        &self,
        ctx: &AuthContext,
        _req: SubscribeRequest,
    ) -> Result<EventStream<LibraryEvent>, LibError> {
        self.subscribe_impl(ctx, _req).await
    }
}
