//! The `LibraryService` trait — the one seam every front-end depends on (tech-spec 03 §2).
//!
//! Two implementations satisfy it: `EmbeddedLibrary` (in-process, `3dam-core`) and `ApiClient`
//! (HTTP/WS → remote `3dam serve`, `3dam-client`). A front-end holds a `Box<dyn LibraryService>`
//! and cannot tell which it is. This trait carries the **phase-1 slice** of the full surface;
//! later methods (facets, similar, tags, collections, convert…) are added as their areas land.

use crate::accounts::AccountIdentity;
use crate::dto::*;
use crate::error::LibError;
use crate::event::{LibraryEvent, SubscribeRequest};
use crate::federation::VectorSimilarRequest;
use crate::id::{AssetId, CollectionId, CommentId, ContentHash, JobId, SourceId};
use crate::page::{Page, PageParams};
use async_trait::async_trait;
use futures::Stream;
use std::collections::BTreeSet;
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
    /// Includes `Federate`: the owner is the machine's full authority and must not silently lack a
    /// capability the embedded engine holds (kept in step with [`Scopes::all`]).
    pub fn owner() -> Self {
        Scopes::none()
            .with(Scope::Read)
            .with(Scope::Write)
            .with(Scope::Admin)
            .with(Scope::McpUse)
            .with(Scope::Federate)
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

/// The visibility ceiling on a context (tech-spec 10 §4.3, issue #42): which sources and
/// collections this identity may *reach*. Resolved **once, at auth time, in the server** (union of
/// the identity's shares, intersected with any account ceiling) and enforced as a query predicate
/// inside the engine — so the engine never learns what an account or a group is, and no handler
/// can forget to filter.
///
/// Deliberately **not** `Default`: `Full` is the fail-open value, and a derived default would let a
/// future credential kind (or a `..Default::default()` struct update) inherit unrestricted reach in
/// silence. Every construction names its ceiling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Unrestricted — the local owner, admins, tokens, and the embedded engine.
    Full,
    /// A positive reachable set. Anything outside it is *absent* (404/empty), never a 403 — an
    /// unshared resource must not reveal its existence by erroring differently.
    Restricted(VisibilityScope),
}

/// The reachable set of a restricted identity. `sources`/`collections` gate reads; the `write_*`
/// subsets additionally gate writes to that resource. A `write` grant never implies `Scope::Write`
/// — both gates must pass independently (issue #42 resolution rule 4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisibilityScope {
    pub sources: BTreeSet<SourceId>,
    pub collections: BTreeSet<CollectionId>,
    pub write_sources: BTreeSet<SourceId>,
    pub write_collections: BTreeSet<CollectionId>,
}

impl Visibility {
    pub fn is_full(&self) -> bool {
        matches!(self, Visibility::Full)
    }
    /// The restriction, if any — the engine's query builders branch on this.
    pub fn restricted(&self) -> Option<&VisibilityScope> {
        match self {
            Visibility::Full => None,
            Visibility::Restricted(s) => Some(s),
        }
    }
    pub fn allows_source(&self, id: &SourceId) -> bool {
        self.restricted().is_none_or(|s| s.sources.contains(id))
    }
    pub fn allows_collection(&self, id: &CollectionId) -> bool {
        self.restricted().is_none_or(|s| s.collections.contains(id))
    }
    pub fn allows_source_write(&self, id: &SourceId) -> bool {
        self.restricted()
            .is_none_or(|s| s.write_sources.contains(id))
    }
    pub fn allows_collection_write(&self, id: &CollectionId) -> bool {
        self.restricted()
            .is_none_or(|s| s.write_collections.contains(id))
    }
    /// Whether a job may be *observed* by this ceiling — the predicate behind `get_job`, `list_jobs`,
    /// and the `JobProgress` arm of [`allows_event`](Self::allows_event).
    ///
    /// Deliberately `all`, not `any`: `Progress.current` names the file being worked on right now, so
    /// a job spanning a shared *and* an unshared source would dribble unshared paths into a restricted
    /// client's status bar. Requiring every touched source to be reachable makes the paths a job can
    /// emit reachable by construction, which is why nothing here has to scrub `current`.
    ///
    /// An unattributed job (`sources` empty — a row written before attribution existed) is observable
    /// only at `Full`.
    pub fn allows_job(&self, job: &crate::dto::JobStatus) -> bool {
        if self.is_full() {
            return true;
        }
        (!job.sources.is_empty() || !job.collections.is_empty())
            && job.sources.iter().all(|s| self.allows_source(s))
            && job.collections.iter().all(|c| self.allows_collection(c))
    }

    /// Whether a live event may be delivered to a subscriber holding this ceiling (issue #42).
    ///
    /// Every variant is matched explicitly — no positional catch-all — so a newly added
    /// `LibraryEvent` fails to compile here rather than being silently withheld (a dead client) or
    /// silently leaked (a hole in the ceiling).
    ///
    /// Per-asset events are judged on their `source_id` alone. That covers source shares completely;
    /// it does **not** cover an identity whose reach comes *only* from a collection share, because
    /// collection membership lives in `collection_member` and answering it would cost a query per
    /// event per subscriber on the firehose. Such a subscriber keeps today's behaviour — no per-asset
    /// events, a grid that refreshes on its own refetches — rather than gaining a leak. Closing that
    /// remainder means carrying an event's collection ids the way `source_id` is carried here.
    pub fn allows_event(&self, ev: &crate::event::LibraryEvent) -> bool {
        use crate::event::LibraryEvent as E;
        if self.is_full() {
            return true;
        }
        match ev {
            // A reset carries no ids — it says "your view is stale", which is true for everyone.
            E::CatalogReset => true,
            E::SourceState { id, .. } => self.allows_source(id),
            E::AssetAdded(a) => a.source_id.is_some_and(|s| self.allows_source(&s)),
            E::AssetChanged { source_id, .. } | E::AssetRemoved { source_id, .. } => {
                source_id.is_some_and(|s| self.allows_source(&s))
            }
            E::JobProgress(j) => self.allows_job(j),
        }
    }

    /// The write-half of this ceiling viewed as a read-shaped set — lets a writability check reuse
    /// the same reachability predicate the read path enforces (engine-side, issue #42 rule 4).
    pub fn write_view(&self) -> Visibility {
        match self {
            Visibility::Full => Visibility::Full,
            Visibility::Restricted(s) => Visibility::Restricted(VisibilityScope {
                sources: s.write_sources.clone(),
                collections: s.write_collections.clone(),
                write_sources: s.write_sources.clone(),
                write_collections: s.write_collections.clone(),
            }),
        }
    }
}

/// The caller's own resolved identity + effective scopes — the answer to `whoami` (tech-spec 10
/// §1.2). Lets a front-end shape its UI to what this credential may actually do (disable a write
/// button rather than let the request 403), so permissions are visible *before* an action, not
/// discovered by its failure. `anonymous` is the honest "no verified credential" signal: true under
/// `Off`/`Anonymous` with no token, false for a store-verified token or the local embedded owner.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WhoAmI {
    pub identity: Option<String>,
    pub scopes: Scopes,
    pub anonymous: bool,
    /// The signed-in account, when the credential is a session (phase 6, issue #42). Additive:
    /// absent for tokens, the local owner, and older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountIdentity>,
    /// True when this context is visibility-restricted (some sources/collections are absent for
    /// it). Lets a client explain "you may not be seeing everything" without learning what's hidden.
    #[serde(default)]
    pub restricted: bool,
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
    /// The visibility ceiling (tech-spec 10 §4.3): which sources/collections this context may
    /// reach. `Full` for tokens, the owner, and the embedded engine; a share-derived set for
    /// non-admin accounts. Enforced inside the engine's query path.
    pub visibility: Visibility,
    /// The signed-in account behind a session credential, if any (issue #42) — used for audit
    /// attribution and `whoami`; guards still check scopes, never roles.
    pub account: Option<AccountIdentity>,
}

impl AuthContext {
    /// Full-scope context for the in-process engine.
    pub fn embedded() -> Self {
        Self {
            identity: None,
            embedded: true,
            scopes: Scopes::all(),
            visibility: Visibility::Full,
            account: None,
        }
    }
    /// A connected caller with a resolved identity, granted scopes, and — **required, never
    /// defaulted** — its visibility ceiling (tech-spec 10 §1.2, §4.3). The ceiling is a positional
    /// argument precisely so a new credential kind cannot inherit `Full` by omission; a caller that
    /// genuinely has unrestricted reach (a token, the local owner) says `Visibility::Full` out loud.
    pub fn connected(identity: Option<String>, scopes: Scopes, visibility: Visibility) -> Self {
        Self {
            identity,
            embedded: false,
            scopes,
            visibility,
            account: None,
        }
    }
    /// Attach the signed-in account identity (builder-style; server auth layer).
    pub fn with_account(mut self, a: AccountIdentity) -> Self {
        self.account = Some(a);
        self
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
    /// This context described back to the caller (the `whoami` answer). The embedded engine always
    /// carries the full scope set, so it reports itself as a non-anonymous owner.
    ///
    /// `anonymous` marks a genuinely unauthenticated, non-owner caller — the `Anonymous`-mode
    /// fallback. The two credential-less contexts are the auth-off local *owner* (full trust, holds
    /// `Admin`) and the anonymous caller (read-only, no `Admin`); the `Admin` scope is what tells them
    /// apart, so the owner is not reported as anonymous even though it presented no token.
    pub fn whoami(&self) -> WhoAmI {
        WhoAmI {
            identity: self.identity.clone(),
            scopes: self.scopes,
            anonymous: !self.embedded && self.identity.is_none() && !self.scopes.has(Scope::Admin),
            account: self.account.clone(),
            restricted: !self.visibility.is_full(),
        }
    }
}

#[async_trait]
pub trait LibraryService: Send + Sync {
    // ── identity ─────────────────────────────────────────────────────────────
    /// Who this credential is and what it may do — the front-door model's "permissions decide after
    /// the gate" made legible to a client, which shapes its UI to the granted scopes instead of
    /// discovering them through 403s. The embedded engine answers from its own full-trust context;
    /// the connected client asks the server, whose answer reflects the presented token.
    async fn whoami(&self, ctx: &AuthContext) -> Result<WhoAmI, LibError> {
        Ok(ctx.whoami())
    }

    // ── browse / search ────────────────────────────────────────────────────
    async fn query(
        &self,
        ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError>;

    async fn get_asset(&self, ctx: &AuthContext, id: &AssetId) -> Result<Asset, LibError>;

    /// Read an asset using its locally-issued source attribution as an ownership hint. The hint is
    /// opaque outside the serving library: implementations must resolve it against their own
    /// source registry and must never interpret it as a caller-controlled endpoint.
    async fn get_asset_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        _source: Option<SourceId>,
    ) -> Result<Asset, LibError> {
        self.get_asset(ctx, id).await
    }

    /// Read an asset's raw bytes for a preview (the out-of-band data-handoff the WASM viewer islands
    /// consume — tech-spec 09 §B.3). Bounded to preview-sized reads; large assets return an error
    /// rather than streaming the whole file. Read-only, non-destructive (PRODUCT_SPEC §8).
    async fn read_content(&self, ctx: &AuthContext, id: &AssetId)
        -> Result<AssetContent, LibError>;

    async fn read_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        _source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.read_content(ctx, id).await
    }

    /// Describe the original representation without opening its byte stream. Servers use this to
    /// answer `HEAD`, resolve `Range`, and reject `416` before a local or remote transfer starts.
    async fn content_metadata(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContentMetadata, LibError>;

    async fn content_metadata_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        _source: Option<SourceId>,
    ) -> Result<AssetContentMetadata, LibError> {
        self.content_metadata(ctx, id).await
    }

    /// Stream an exact inclusive range of the original representation. Unlike [`Self::read_content`]
    /// this is not subject to the preview materialisation cap: memory is bounded by the producer's
    /// chunk/window size, and dropping the returned stream cooperatively cancels source I/O.
    async fn stream_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
    ) -> Result<AssetContentStream, LibError>;

    async fn stream_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
        _source: Option<SourceId>,
    ) -> Result<AssetContentStream, LibError> {
        self.stream_content(ctx, id, range).await
    }

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

    async fn read_related_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
        _source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.read_related_content(ctx, id, rel).await
    }

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

    async fn read_thumbnail_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
        _source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.read_thumbnail(ctx, id, max_edge).await
    }

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

    async fn read_model_preview_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        _source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        self.read_model_preview(ctx, id).await
    }

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

    async fn submit_convert(
        &self,
        ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<JobId, LibError>;

    // ── upload (issue #80, tech-spec 08 §5.1) ────────────────────────────────
    /// Write one file into a registered source and catalogue it.
    ///
    /// This is the **only** operation in 3DAM that writes inside a source tree, and it is
    /// create-only: [`UploadCollision`] has no overwrite arm, so an existing file is never
    /// replaced under any request. Convert's §5.1 guard is untouched and shares no code with this
    /// path — see tech-spec 08 §5.1 for why that separation is what keeps the invariant structural.
    ///
    /// `staged` is a **local file already holding the bytes**, not the destination: the caller
    /// streams the body to scratch first, so neither the transport nor the engine ever buffers a
    /// large asset in memory. An `ApiClient` forwards those bytes to a server; `EmbeddedLibrary`
    /// reads them straight through into the source.
    async fn upload(
        &self,
        ctx: &AuthContext,
        req: UploadRequest,
        staged: &std::path::Path,
    ) -> Result<UploadOutcome, LibError>;

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

    // ── notes (issue #81) ────────────────────────────────────────────────────
    /// The user's free-text note on an asset, or `None` if there isn't one. `Scope::Read`.
    ///
    /// [`get_asset`](Self::get_asset) already returns this on the record; this exists for the
    /// callers that want the note alone (the CLI, a client polling after an edit) without paying
    /// for the attribute/tag/collection joins.
    async fn get_note(&self, ctx: &AuthContext, id: &AssetId) -> Result<Option<Note>, LibError>;

    /// Set or clear an asset's note. An empty body clears it. `Scope::Write`, plus a write share on
    /// the asset's source once accounts restrict reach. Emits `AssetChanged { kind: NoteSet }`.
    async fn set_note(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        req: NoteRequest,
    ) -> Result<Option<Note>, LibError>;

    // ── discussion (issue #82) ───────────────────────────────────────────────
    /// An asset's thread, oldest first, tombstones included. Gated on **read** access to the asset:
    /// if you cannot see the asset you cannot see its discussion, because a message body can quote
    /// a path or filename you were never meant to learn.
    async fn list_comments(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Vec<Comment>, LibError>;

    /// Post a message. Requires a **signed-in account** plus read access to the asset — deliberately
    /// *not* `Scope::Write`, which would conflate "may modify the library" with "may talk about it"
    /// and lock out the reviewing art director who is the whole point of the feature. Anonymous and
    /// bearer-token callers cannot post: a shared machine credential is not a person to attribute a
    /// message to. Emits `AssetChanged { kind: Commented }`.
    async fn post_comment(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
        req: NewComment,
    ) -> Result<Comment, LibError>;

    /// Edit one's own message. Author only — not even an admin may put words in someone's mouth.
    async fn edit_comment(
        &self,
        ctx: &AuthContext,
        id: &CommentId,
        req: EditComment,
    ) -> Result<Comment, LibError>;

    /// Delete a message: the author, or any caller holding `Scope::Admin` (moderation). Soft —
    /// leaves a tombstone so replies keep their parent.
    async fn delete_comment(&self, ctx: &AuthContext, id: &CommentId) -> Result<(), LibError>;

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

    async fn submit_export(&self, ctx: &AuthContext, req: ExportRequest)
        -> Result<JobId, LibError>;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn job(sources: Vec<SourceId>, collections: Vec<CollectionId>) -> JobStatus {
        JobStatus {
            id: JobId::new(),
            kind: JobKind::Export,
            state: JobState::Queued,
            progress: Progress::default(),
            error: None,
            summary: None,
            warnings: Vec::new(),
            result_artifacts: Vec::new(),
            result: None,
            created_at: 0,
            updated_at: 0,
            initiator: None,
            sources,
            collections,
        }
    }

    #[test]
    fn restricted_job_visibility_requires_all_source_and_collection_attribution() {
        let shared_source = SourceId::new();
        let hidden_source = SourceId::new();
        let shared_collection = CollectionId::new();
        let hidden_collection = CollectionId::new();
        let visibility = Visibility::Restricted(VisibilityScope {
            sources: [shared_source].into_iter().collect(),
            collections: [shared_collection].into_iter().collect(),
            ..VisibilityScope::default()
        });

        assert!(visibility.allows_job(&job(vec![shared_source], vec![shared_collection])));
        assert!(!visibility.allows_job(&job(
            vec![shared_source, hidden_source],
            vec![shared_collection]
        )));
        assert!(!visibility.allows_job(&job(vec![shared_source], vec![hidden_collection])));
        assert!(!visibility.allows_job(&job(Vec::new(), Vec::new())));
    }
}
