//! The `LibraryService` trait — the one seam every front-end depends on (tech-spec 03 §2).
//!
//! Two implementations satisfy it: `EmbeddedLibrary` (in-process, `3dam-core`) and `ApiClient`
//! (HTTP/WS → remote `3dam serve`, `3dam-client`). A front-end holds a `Box<dyn LibraryService>`
//! and cannot tell which it is. This trait carries the **phase-1 slice** of the full surface;
//! later methods (facets, similar, tags, collections, convert…) are added as their areas land.

use crate::dto::*;
use crate::error::LibError;
use crate::event::{LibraryEvent, SubscribeRequest};
use crate::federation::VectorSimilarRequest;
use crate::id::{AssetId, CollectionId, ContentHash, JobId, SourceId};
use crate::page::{Page, PageParams};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// A stream of library events (the WS firehose when connected, a channel when embedded).
pub type EventStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// One capability a caller may hold (tech-spec 10 §4.2). Guards check *scopes*, never roles, so
/// this enum is the single place a capability gains meaning. `Federate` is reserved for the
/// phase-6 federation surface; it is defined here so the scope set is stable across phases.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Read,
    Write,
    Admin,
    McpUse,
    Federate,
}

impl Scope {
    const ALL: [Scope; 5] = [
        Scope::Read,
        Scope::Write,
        Scope::Admin,
        Scope::McpUse,
        Scope::Federate,
    ];
    fn bit(self) -> u8 {
        match self {
            Scope::Read => 1 << 0,
            Scope::Write => 1 << 1,
            Scope::Admin => 1 << 2,
            Scope::McpUse => 1 << 3,
            Scope::Federate => 1 << 4,
        }
    }
}

/// A granted set of scopes (tech-spec 10 §4.2). A small bitset so a downstream guard is a pure
/// membership test, not a re-derivation. Serialises as a JSON array of scope strings so the flag
/// store and admin API stay human-readable.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Scopes(u8);

impl Scopes {
    /// The empty set.
    pub const fn none() -> Self {
        Scopes(0)
    }
    /// Every scope — the in-process embedded engine's context.
    pub fn all() -> Self {
        Scope::ALL.into_iter().fold(Scopes(0), |s, sc| s.with(sc))
    }
    /// What an anonymous caller is granted (tech-spec 10 §4.2 `anon_scopes`): read + read-tool use.
    pub fn anonymous() -> Self {
        Scopes::none().with(Scope::Read).with(Scope::McpUse)
    }
    /// The scopes an unauthenticated request gets when `Authentication = Off` — full local trust
    /// (owner posture), so an operator can never lock themselves out of their own localhost server.
    pub fn owner() -> Self {
        Scopes::none()
            .with(Scope::Read)
            .with(Scope::Write)
            .with(Scope::Admin)
            .with(Scope::McpUse)
    }
    pub fn with(self, s: Scope) -> Self {
        Scopes(self.0 | s.bit())
    }
    pub fn has(self, s: Scope) -> bool {
        self.0 & s.bit() != 0
    }
    pub fn to_vec(self) -> Vec<Scope> {
        Scope::ALL.into_iter().filter(|s| self.has(*s)).collect()
    }
    pub fn collect<I: IntoIterator<Item = Scope>>(it: I) -> Self {
        it.into_iter().fold(Scopes(0), |s, sc| s.with(sc))
    }
}

impl std::fmt::Debug for Scopes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.to_vec())
    }
}

impl serde::Serialize for Scopes {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        self.to_vec().serialize(ser)
    }
}

impl<'de> serde::Deserialize<'de> for Scopes {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let v = Vec::<Scope>::deserialize(de)?;
        Ok(Scopes::collect(v))
    }
}

/// Carries identity, granted scopes, and (later) a visibility ceiling. Resolved once per request by
/// the server's auth middleware (tech-spec 10 §1.2); the embedded engine uses a full-scope context.
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub identity: Option<String>,
    /// True for the in-process embedded impl (no boundary to guard — full trust).
    pub embedded: bool,
    /// The granted, effective scopes (tech-spec 10 §4.2).
    pub scopes: Scopes,
}

impl AuthContext {
    /// Full-scope context for the in-process engine.
    pub fn embedded() -> Self {
        Self {
            identity: None,
            embedded: true,
            scopes: Scopes::all(),
        }
    }
    /// A connected caller with a resolved identity and its granted scopes (tech-spec 10 §1.2).
    pub fn connected(identity: Option<String>, scopes: Scopes) -> Self {
        Self {
            identity,
            embedded: false,
            scopes,
        }
    }
    /// Guard: succeed iff this context holds `scope` (the embedded engine always does), else `403`
    /// (tech-spec 10 §1.2 `ctx.require`).
    pub fn require(&self, scope: Scope) -> Result<(), LibError> {
        if self.embedded || self.scopes.has(scope) {
            Ok(())
        } else {
            Err(LibError::Forbidden(format!("missing scope: {scope:?}")))
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

    /// Read a file referenced *by relative path* from an asset's own directory within the same
    /// source — the loose-glTF case where a `.gltf` points at sibling `.bin`/texture files by
    /// relative URI (issue #56). `rel` is resolved against the asset's directory and confined to the
    /// source root (traversal rejected). Read-only, bounded to preview-sized reads.
    async fn read_related_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
    ) -> Result<AssetContent, LibError>;

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

    /// Read (generating + caching on miss) the interactive 3D preview for a **model** asset: a
    /// compact self-contained `DMSH` mesh blob (geometry + PBR materials + downscaled textures) that
    /// the WASM viewer island uploads directly (tech-spec 09 §B.3). Decoded once server-side via the
    /// same Assimp path as the turntable thumbnail, so it covers the full professional format range
    /// with textures. Non-model assets — or a build without the server `render` feature — return
    /// `Unsupported`. Returned as an `AssetContent` with `model/x-dam-preview`.
    async fn read_model_preview(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError>;

    /// Library aggregates (totals, media/source/tag counts). `source` scopes the numbers to one
    /// source; for a **federated** source the engine proxies the call to the peer, so the counts
    /// are the peer's own, live (phase 6). `None` = the whole local library.
    async fn library_stats(
        &self,
        ctx: &AuthContext,
        source: Option<SourceId>,
    ) -> Result<LibraryStats, LibError>;

    /// Prefetch hint (issue #72): the client is about to render `req.assets`, so warm their
    /// thumbnails (and model preview meshes) ahead of the HTTP fetch. Fire-and-forget and idempotent
    /// — already-cached derivatives are a no-op, and a warm failure is swallowed (the on-demand GET
    /// still generates it). The bytes themselves stay on HTTP/2 (ADR 0012); this only moves
    /// generation ahead of render. Default: no-op, so a backend that doesn't warm is still valid.
    async fn prefetch(&self, _ctx: &AuthContext, _req: PrefetchRequest) -> Result<(), LibError> {
        Ok(())
    }

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

    /// The immediate subfolders directly under a source path (issue #66) — the lazy unit the folder
    /// tree expands, each with the asset count of its whole subtree. Sorted by name.
    async fn list_folders(
        &self,
        ctx: &AuthContext,
        req: FolderListing,
    ) -> Result<Vec<FolderEntry>, LibError>;

    async fn add_source(&self, ctx: &AuthContext, req: AddSource) -> Result<SourceId, LibError>;

    async fn remove_source(
        &self,
        ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError>;

    // ── remove / blocklist (issue #21) ───────────────────────────────────────
    /// Remove one asset from the catalog. With `block`, also record its content hash on the
    /// blocklist so no future scan/watch/auto-rescan re-imports the same bytes. Non-destructive:
    /// only catalog rows are touched; the file in the source is never deleted (PRODUCT_SPEC §8).
    async fn remove_asset(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        req: RemoveAsset,
    ) -> Result<(), LibError>;

    /// The blocked content hashes — the management surface for the "removed + blocked" set.
    async fn list_blocklist(&self, ctx: &AuthContext) -> Result<Vec<BlockEntry>, LibError>;

    /// Lift a block so the content can be re-imported by a subsequent scan.
    async fn unblock(&self, ctx: &AuthContext, hash: &ContentHash) -> Result<(), LibError>;

    // ── analysis / automation (tech-spec 05, phase 3) ────────────────────────
    /// Submit an analysis pass over due (or the requested) assets: embeddings, perceptual/tileability
    /// derivation, auto-tag/-category suggestions, and dedup grouping. Background job; incremental and
    /// versioned (§1.2, §7). Analysis only ever produces *suggestions* + derived data — it never
    /// mutates confirmed catalog state (DESIGN_GUIDELINES §1.2).
    async fn submit_analyze(
        &self,
        ctx: &AuthContext,
        req: AnalyzeRequest,
    ) -> Result<JobId, LibError>;

    /// Force the derived preview cache to be rebuilt for specific assets: drop each asset's cached
    /// thumbnail PNG(s) and 3D preview blob so the next read re-renders from source. Content-keyed
    /// and non-destructive — only regenerable derivatives are removed; the source is never touched
    /// (PRODUCT_SPEC §8). Synchronous (a cache purge, not a background job); regeneration happens
    /// lazily on the next thumbnail read.
    async fn regenerate_thumbnails(
        &self,
        ctx: &AuthContext,
        req: ThumbnailRegenRequest,
    ) -> Result<ThumbnailRegenReport, LibError>;

    /// "More like this": nearest neighbours of an asset in its media's embedding space, cosine-ranked
    /// and facet-filterable (§3). Returns empty if the asset has no embedding yet (§1.3).
    async fn find_similar(
        &self,
        ctx: &AuthContext,
        req: SimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError>;

    /// "Find similar" **by vector** — the federated entry point (phase 6, issue #40): a peer embeds
    /// locally, ships the vector, and this instance ranks it against its own index in the named
    /// space. Never merges across spaces: a `space` this instance doesn't serve for `media` is a
    /// `BadRequest`, not an empty page. Default: `Unsupported`, so non-serving backends opt out.
    async fn find_similar_by_vector(
        &self,
        _ctx: &AuthContext,
        _req: VectorSimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        Err(LibError::Unsupported(
            "similar-by-vector is not served by this backend".into(),
        ))
    }

    /// The duplicate groups for the review view — exact (content hash) or near (perceptual/embedding),
    /// each carrying its linking signal + a suggested keep (§4). Grouping only; nothing is deleted.
    async fn list_duplicates(
        &self,
        ctx: &AuthContext,
        req: DupRequest,
    ) -> Result<Vec<DupGroup>, LibError>;

    /// Accept or reject one auto-suggested tag (§1.4). Reversible; a reject is remembered so the same
    /// extractor version won't re-suggest it.
    async fn review_suggestion(
        &self,
        ctx: &AuthContext,
        req: SuggestionReview,
    ) -> Result<(), LibError>;

    /// Flag or unflag an asset as a favourite (issue #63). Reversible; persisted in the asset
    /// `flags` bitset so it survives re-scans.
    async fn set_favorite(&self, ctx: &AuthContext, req: FavoriteRequest) -> Result<(), LibError>;

    // ── collections / smart folders (phase 4) ────────────────────────────────
    /// All collections and smart folders. Manual folders carry an exact member count; a smart
    /// folder's live count is left `None` here (computed on demand).
    async fn list_collections(&self, ctx: &AuthContext) -> Result<Vec<Collection>, LibError>;

    async fn get_collection(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<Collection, LibError>;

    /// Create a manual collection or a smart folder (a smart folder must carry a saved query).
    async fn create_collection(
        &self,
        ctx: &AuthContext,
        req: NewCollection,
    ) -> Result<CollectionId, LibError>;

    /// Rename a collection and/or replace a smart folder's saved query.
    async fn update_collection(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        req: UpdateCollection,
    ) -> Result<(), LibError>;

    async fn delete_collection(&self, ctx: &AuthContext, id: &CollectionId)
        -> Result<(), LibError>;

    /// Add/remove members of a manual collection (rejects smart folders — their set is query-driven).
    async fn modify_collection_members(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        req: CollectionMembers,
    ) -> Result<(), LibError>;

    /// Resolve a collection to a page of assets: the explicit set for a manual collection, or the
    /// live query result for a smart folder.
    async fn collection_assets(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        page: PageParams,
    ) -> Result<Page<AssetSummary>, LibError>;

    // ── export / manifests (phase 4) ─────────────────────────────────────────
    /// Export a manifest (JSON/CSV/sidecar) over a selector (ids / collection / query / whole
    /// library) — metadata, license, attribution, and tags for use in engines and pipelines.
    async fn export(&self, ctx: &AuthContext, req: ExportRequest)
        -> Result<ExportReport, LibError>;

    // ── jobs: scan ───────────────────────────────────────────────────────────
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
