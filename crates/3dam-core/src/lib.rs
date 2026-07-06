//! `dam-core` — the pure engine. `EmbeddedLibrary` implements the `LibraryService` seam over the
//! store, media handlers, and sources (tech-spec 01 §3). No UI, transport, or GPU (ADR 0002).
//! This is the standalone, no-network path; `3dam-server` serves the same object over HTTP/WS.

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

    async fn library_stats(&self, _ctx: &AuthContext) -> Result<LibraryStats, LibError> {
        self.db(|s| s.stats()).await
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
