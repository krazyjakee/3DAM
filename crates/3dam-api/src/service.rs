//! The `LibraryService` trait — the one seam every front-end depends on (tech-spec 03 §2).
//!
//! Two implementations satisfy it: `EmbeddedLibrary` (in-process, `3dam-core`) and `ApiClient`
//! (HTTP/WS → remote `3dam serve`, `3dam-client`). A front-end holds a `Box<dyn LibraryService>`
//! and cannot tell which it is. This trait carries the **phase-1 slice** of the full surface;
//! later methods (facets, similar, tags, collections, convert…) are added as their areas land.

use crate::dto::*;
use crate::error::LibError;
use crate::event::{LibraryEvent, SubscribeRequest};
use crate::id::{AssetId, JobId, SourceId};
use crate::page::Page;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// A stream of library events (the WS firehose when connected, a channel when embedded).
pub type EventStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// Carries identity, granted scopes, and a visibility ceiling. Opaque to this crate; owned by
/// tech-spec 10. Embedded mode uses a full-scope context.
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub identity: Option<String>,
    /// True for the in-process embedded impl (no boundary to guard).
    pub embedded: bool,
}

impl AuthContext {
    /// Full-scope context for the in-process engine.
    pub fn embedded() -> Self {
        Self {
            identity: None,
            embedded: true,
        }
    }
    /// A connected caller with an optional resolved identity.
    pub fn connected(identity: Option<String>) -> Self {
        Self {
            identity,
            embedded: false,
        }
    }
}

#[async_trait]
pub trait LibraryService: Send + Sync {
    // ── browse / search ────────────────────────────────────────────────────
    async fn query(
        &self,
        ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError>;

    async fn get_asset(&self, ctx: &AuthContext, id: &AssetId) -> Result<Asset, LibError>;

    /// Read an asset's raw bytes for a preview (the out-of-band data-handoff the WASM viewer islands
    /// consume — tech-spec 09 §B.3). Bounded to preview-sized reads; large assets return an error
    /// rather than streaming the whole file. Read-only, non-destructive (PRODUCT_SPEC §8).
    async fn read_content(&self, ctx: &AuthContext, id: &AssetId)
        -> Result<AssetContent, LibError>;

    /// Read (generating + caching on miss) a downscaled PNG thumbnail for an asset preview
    /// (tech-spec 04 §6.4). `max_edge` bounds the long side. Only raster images produce one;
    /// audio/3D return `Unsupported` (their previews are WASM islands) and the UI falls back to
    /// the honest typed tile. Returned as an `AssetContent` with `image/png`.
    async fn read_thumbnail(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
    ) -> Result<AssetContent, LibError>;

    async fn library_stats(&self, ctx: &AuthContext) -> Result<LibraryStats, LibError>;

    // ── convert (tech-spec 08) ───────────────────────────────────────────────
    /// Run a convert plan: dry-run (plan + estimate, no writes) or commit (encode + atomic write
    /// under `output_dir`). Non-destructive and source-safe by construction (§5.1). CLI-first in
    /// v1; returns the full per-item report.
    async fn convert(
        &self,
        ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<ConvertReport, LibError>;

    // ── sources ──────────────────────────────────────────────────────────────
    async fn list_sources(&self, ctx: &AuthContext) -> Result<Vec<SourceInfo>, LibError>;

    async fn get_source(&self, ctx: &AuthContext, id: &SourceId) -> Result<SourceInfo, LibError>;

    async fn add_source(&self, ctx: &AuthContext, req: AddSource) -> Result<SourceId, LibError>;

    async fn remove_source(
        &self,
        ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError>;

    // ── jobs: scan (analyze/convert land later) ──────────────────────────────
    async fn submit_scan(&self, ctx: &AuthContext, req: ScanRequest) -> Result<JobId, LibError>;

    async fn get_job(&self, ctx: &AuthContext, id: &JobId) -> Result<JobStatus, LibError>;

    async fn list_jobs(
        &self,
        ctx: &AuthContext,
        req: JobListRequest,
    ) -> Result<Page<JobStatus>, LibError>;

    async fn cancel_job(&self, ctx: &AuthContext, id: &JobId) -> Result<(), LibError>;

    // ── live delivery ────────────────────────────────────────────────────────
    async fn subscribe(
        &self,
        ctx: &AuthContext,
        req: SubscribeRequest,
    ) -> Result<EventStream<LibraryEvent>, LibError>;
}
