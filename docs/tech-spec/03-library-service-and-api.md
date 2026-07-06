# 03 — LibraryService trait & the HTTP/WebSocket API

Status: **Draft v0.1** · Scope: the `LibraryService` trait, its DTOs and error model, pagination/streaming/live-update delivery, and the HTTP/WS API that mirrors it.

This file defines the **one seam every front-end depends on** ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §4.2,
[DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.4, §2). Clients — desktop GUI ([12](12-desktop-gui.md)),
CLI ([13](13-cli.md)), web ([09](09-server-and-web-client.md)) — call `LibraryService`, never the engine or the
database directly. Two implementations satisfy it: an **in-process** one over the embedded engine, and an
**API-client** one that talks to a remote `3dam serve`. The server ([09](09-server-and-web-client.md)) exposes the
*same* surface over HTTP/WebSocket, so the API in this file is the wire form of the trait below, method-for-method.

**Borders (do not write outside them).** Crate placement of this seam — which crate the trait lives in and how the
two impls are wired/feature-gated — is [01](01-architecture-and-crates.md). The axum wiring, TLS, port sharing and
web-asset hosting are [09](09-server-and-web-client.md). Federated **fan-out mechanics** (which peers, in what order,
merge/re-rank, partial-peer handling) are [07](07-sources-and-federation.md) — this file only marks *where* a call
federates and defines the result-shape that makes merged results representable. **Auth/scope** — how a caller is
authenticated and what a scope permits — is [10](10-auth-accounts-and-flags.md); this file only names the
`AuthContext` seam and the error variants auth produces. DTO↔row mapping and the vector index are
[02](02-data-model-and-storage.md); the analysis/similarity semantics are [05](05-analysis-similarity-dedup.md);
convert jobs are [08](08-convert-pipeline.md); job/worker execution and cancellation are
[14](14-concurrency-performance-reliability.md).

This file gives **pseudocode**, per the spec convention (indicative signatures, not a frozen API).

---

## 1. Design constraints this surface must satisfy

Pulled from the product/design docs; the mechanics below exist to honour them.

- **Never block the UI** ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §8, [DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1).
  Every list-returning call is **paginated**, and the big/slow ones (`query`, `scan`, `analyze`, `convert`,
  `find_similar` across peers) can deliver **incrementally** — a partial page is usable immediately, more arrive as
  computed. No call returns "the whole library" in one blocking dump.
- **Fail soft** ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §2, [PRODUCT_SPEC](../PRODUCT_SPEC.md) §8). One bad
  asset, one offline source, one slow peer degrades *that item* — the call still returns. The error model separates
  a **hard failure of the whole call** (a `Result::Err`) from **soft per-item / per-peer partials** (carried inside
  a successful result as `warnings` / `partial`).
- **Transport parity.** In-process and API-client impls are behaviourally identical; the only differences are latency
  and that some soft warnings (e.g. "peer offline") only arise when connected/federated. A front-end cannot tell which
  impl it holds.
- **Federation is transparent** ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §4.4, §6.3). `query` and `find_similar` may fan
  out; results carry an `origin` peer and the envelope carries per-peer partial status. The *fan-out* is
  [07](07-sources-and-federation.md); the *shape that lets a merged result be returned* is here.
- **Auth-scoped** ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §6.7, [10](10-auth-accounts-and-flags.md)). Every method takes
  an `AuthContext`; scope is the ceiling on what is visible. In-process embedded mode passes an implicit full-scope
  context.

---

## 2. The `LibraryService` trait

One `async` trait. Grouped by area; every method takes `&self` and an `AuthContext` (§2.1). Types are defined in §3–§5.

```rust
#[async_trait]
pub trait LibraryService: Send + Sync {
    // ── browse / search / discovery ───────────────────────────────────────
    // Faceted query. May fan out to federated peers (07). Paginated; can stream (§6).
    async fn query(&self, ctx: &AuthContext, req: QueryRequest)
        -> Result<Page<AssetSummary>, LibError>;

    // Facet counts for a query *without* the rows — powers the filter chips (DG §3.3).
    async fn facets(&self, ctx: &AuthContext, req: FacetRequest)
        -> Result<FacetResult, LibError>;

    // Full record for one asset (all attributes, license block, tags, source, derivatives refs).
    async fn get_asset(&self, ctx: &AuthContext, id: &AssetId)
        -> Result<Asset, LibError>;

    // "Find similar" (DG §3.3, PRODUCT_SPEC §6.3). Seed by asset id OR an uploaded reference/
    // vector. Fans out to peers' similarity endpoints (07). Paginated; can stream (§6).
    async fn find_similar(&self, ctx: &AuthContext, req: SimilarRequest)
        -> Result<Page<SimilarHit>, LibError>;

    // Exact + near-duplicate groups (PRODUCT_SPEC §6.3). Paginated by group.
    async fn find_duplicates(&self, ctx: &AuthContext, req: DuplicateRequest)
        -> Result<Page<DuplicateGroup>, LibError>;

    async fn library_stats(&self, ctx: &AuthContext)
        -> Result<LibraryStats, LibError>;

    // ── tags ──────────────────────────────────────────────────────────────
    async fn list_tags(&self, ctx: &AuthContext, req: TagListRequest)
        -> Result<Page<TagInfo>, LibError>;
    // Add/remove tags on a set of assets. Non-destructive metadata edit. Write-scoped (10).
    async fn edit_tags(&self, ctx: &AuthContext, req: TagEditRequest)
        -> Result<TagEditResult, LibError>;
    // Accept/reject an auto-suggested tag (DG §1.2 suggestion→confirmed). Write-scoped.
    async fn resolve_suggestion(&self, ctx: &AuthContext, req: SuggestionDecision)
        -> Result<(), LibError>;

    // ── license / rights (first-class field, PRODUCT_SPEC §5) ─────────────
    async fn set_license(&self, ctx: &AuthContext, req: SetLicenseRequest)
        -> Result<(), LibError>;

    // ── collections & smart folders ──────────────────────────────────────
    async fn list_collections(&self, ctx: &AuthContext)
        -> Result<Vec<CollectionInfo>, LibError>;
    async fn get_collection(&self, ctx: &AuthContext, id: &CollectionId)
        -> Result<Collection, LibError>;
    // Create manual set OR smart folder (a saved QueryRequest). Write-scoped.
    async fn create_collection(&self, ctx: &AuthContext, req: CreateCollection)
        -> Result<CollectionId, LibError>;
    async fn update_collection(&self, ctx: &AuthContext, id: &CollectionId, req: UpdateCollection)
        -> Result<(), LibError>;
    async fn delete_collection(&self, ctx: &AuthContext, id: &CollectionId)
        -> Result<(), LibError>;
    // Add/remove members of a *manual* collection (no-op shape for smart folders).
    async fn edit_collection_members(&self, ctx: &AuthContext, req: MemberEdit)
        -> Result<(), LibError>;

    // ── sources (file + federated; PRODUCT_SPEC §6.1, §6.7) ───────────────
    async fn list_sources(&self, ctx: &AuthContext)
        -> Result<Vec<SourceInfo>, LibError>;
    async fn get_source(&self, ctx: &AuthContext, id: &SourceId)
        -> Result<SourceInfo, LibError>;
    // Add a file source (FS/SFTP/SMB) or a federated 3dam:// peer. Auth config, if any,
    // is a credential ref (10) — never a raw secret in the DTO. Write-scoped.
    async fn add_source(&self, ctx: &AuthContext, req: AddSource)
        -> Result<SourceId, LibError>;
    async fn remove_source(&self, ctx: &AuthContext, id: &SourceId, req: RemoveSource)
        -> Result<(), LibError>;
    // Test reachability/auth of a source without adding/scanning it.
    async fn probe_source(&self, ctx: &AuthContext, req: ProbeSource)
        -> Result<SourceProbe, LibError>;

    // ── preview / blob fetch (DG §3.2) ────────────────────────────────────
    // Thumbnail / waveform / turntable PNG etc. For federated assets the byte stream is
    // proxied from the origin peer and cached (07); the caller can't tell.
    async fn fetch_preview(&self, ctx: &AuthContext, req: PreviewRequest)
        -> Result<PreviewBlob, LibError>;

    // ── jobs: scan / analyze / convert (PRODUCT_SPEC §6.1, §6.2, §6.5) ─────
    // All three submit a background job and return a JobId immediately; progress is observed
    // via `watch_job` / the events stream (§7). Convert honours dry-run + non-destructive (08).
    async fn submit_scan(&self, ctx: &AuthContext, req: ScanRequest)
        -> Result<JobId, LibError>;
    async fn submit_analyze(&self, ctx: &AuthContext, req: AnalyzeRequest)
        -> Result<JobId, LibError>;
    async fn submit_convert(&self, ctx: &AuthContext, req: ConvertRequest)
        -> Result<JobId, LibError>;
    // Metadata/manifest export job (CLI `export` verb 13, MCP `export` tool 11). Returns a JobId.
    async fn submit_export(&self, ctx: &AuthContext, req: ExportRequest)
        -> Result<JobId, LibError>;
    async fn get_job(&self, ctx: &AuthContext, id: &JobId)
        -> Result<JobStatus, LibError>;
    async fn cancel_job(&self, ctx: &AuthContext, id: &JobId)
        -> Result<(), LibError>;
    async fn list_jobs(&self, ctx: &AuthContext, req: JobListRequest)
        -> Result<Page<JobStatus>, LibError>;

    // ── live delivery (§6, §7) ────────────────────────────────────────────
    // A stream of pages for a query/similar call — the incremental form used when a caller
    // wants results as computed rather than one Page (§6.2). `Stream` = futures::Stream.
    async fn query_stream(&self, ctx: &AuthContext, req: QueryRequest)
        -> Result<PageStream<AssetSummary>, LibError>;
    async fn find_similar_stream(&self, ctx: &AuthContext, req: SimilarRequest)
        -> Result<PageStream<SimilarHit>, LibError>;

    // Per-job progress stream (scan/analyze/convert).
    async fn watch_job(&self, ctx: &AuthContext, id: &JobId)
        -> Result<EventStream<JobEvent>, LibError>;

    // Library-wide live updates: new/changed/removed assets, source state changes, job
    // progress — the firehose behind the WS endpoint (§7). Subscription is scope-filtered.
    async fn subscribe(&self, ctx: &AuthContext, req: SubscribeRequest)
        -> Result<EventStream<LibraryEvent>, LibError>;
}
```

`PageStream<T>` and `EventStream<T>` are aliases for `Stream<Item = Result<Page<T>, LibError>>` and
`Stream<Item = Event>` respectively (§6, §7). The API-client impl backs streams with the WS transport; the in-process
impl backs them with in-memory channels — same type, both sides.

### 2.1 `AuthContext` — the auth seam (owned by [10](10-auth-accounts-and-flags.md))

```rust
// Opaque-to-this-file. Carries identity, granted scopes, and visibility ceiling.
// Constructed by the transport layer (09) from a token/OIDC/anonymous credential (10),
// or is `AuthContext::embedded()` (full scope) for the in-process impl.
pub struct AuthContext { /* identity, scopes, visibility — see 10 */ }
```

This file assumes only: (a) every method receives one; (b) an insufficient scope yields `LibError::Forbidden`
and an unauthenticated call on a protected surface yields `LibError::Unauthorized`; (c) the `visibility` ceiling
silently filters rows (fail-soft, not an error). All construction/enforcement is [10](10-auth-accounts-and-flags.md).

---

## 3. DTOs — requests

Naming: `*Request` for read queries, imperative nouns (`AddSource`, `CreateCollection`) for mutations. All are
`serde` (de)serialisable — the same struct is the Rust arg and the JSON request body. Field lists are indicative.

```rust
pub struct QueryRequest {
    pub text:     Option<String>,        // instant text search over name/tags/metadata
    pub filters:  Vec<Filter>,           // faceted filters, AND-combined (DG §3.3)
    pub sort:     Sort,                   // field + direction; default relevance/name
    pub scope:    QueryScope,             // Local | Federated | Sources(Vec<SourceId>)
    pub page:     PageParams,            // cursor/limit (§6.1)
    pub include_facets: bool,            // return facet counts alongside the first page
}

// One filter clause. `field` names a facet; media-specific facets validated per type.
pub struct Filter { pub field: FacetField, pub op: FilterOp, pub value: FilterValue }

pub enum FacetField {
    MediaType, Format, Source, Tag, SizeBytes, License, UsageRight,
    // media-specific (04/05): Bpm, Key, DurationMs, Channels,           // audio
    Width, Height, HasAlpha, Tileability, TileClass, DominantColor,       // image
    TriCount, HasRig, HasUv, Category,                                    // 3d
}
pub enum FilterOp { Eq, Ne, Lt, Lte, Gt, Gte, In, Range, Contains, Exists }
pub enum FilterValue { Str(String), Num(f64), Bool(bool), Range(f64,f64), List(Vec<FilterValue>) }
// UsageRight (License facet, PRODUCT_SPEC §5/§6.3): CommercialUse | NoAttribution |
//   Redistributable | ModificationAllowed | UnknownLicense — the "safe-to-ship" smart folder.

pub struct FacetRequest { pub base: QueryRequest, pub fields: Vec<FacetField> }

pub struct SimilarRequest {
    pub seed:    SimilaritySeed,          // ByAsset(AssetId) | ByVector(Embedding) | ByUpload(BlobRef)
    pub media:   Option<MediaType>,       // restrict target media type
    pub scope:   QueryScope,              // may federate (07)
    pub filters: Vec<Filter>,             // combine similarity with facets
    pub page:    PageParams,
    pub min_score: Option<f32>,
}

pub struct DuplicateRequest { pub scope: QueryScope, pub kind: DupKind, pub page: PageParams }
pub enum DupKind { Exact, Near, Both }

pub struct TagListRequest  { pub prefix: Option<String>, pub kind: TagKind, pub page: PageParams }
pub enum TagKind { All, Confirmed, Suggested }
pub struct TagEditRequest  { pub assets: Vec<AssetId>, pub add: Vec<String>, pub remove: Vec<String> }
pub struct SuggestionDecision { pub asset: AssetId, pub tag: String, pub accept: bool }
pub struct SetLicenseRequest { pub assets: Vec<AssetId>, pub license: LicenseInput }

pub struct CreateCollection { pub name: String, pub kind: CollectionKind }
pub enum   CollectionKind { Manual, Smart(QueryRequest) }   // smart folder = saved query
pub struct UpdateCollection { pub name: Option<String>, pub query: Option<QueryRequest> }
pub struct MemberEdit { pub collection: CollectionId, pub add: Vec<AssetId>, pub remove: Vec<AssetId> }

pub struct AddSource {
    pub kind: SourceKind,                 // LocalFs | Sftp | Smb | Federated (3dam://)
    pub uri:  String,
    pub auth: Option<CredentialRef>,      // opaque ref into the secret store (10) — never a raw secret
    pub options: SourceOptions,           // watch on/off, include/exclude globs, analysis policy
}
pub struct RemoveSource { pub keep_metadata: bool }   // fail-soft: offline sources keep cached rows (PRODUCT_SPEC §6.1)
pub struct ProbeSource  { pub kind: SourceKind, pub uri: String, pub auth: Option<CredentialRef> }

pub struct PreviewRequest { pub asset: AssetId, pub kind: PreviewKind, pub size: Option<PreviewSize> }
pub enum   PreviewKind { Thumbnail, Waveform, Turntable, Full }

pub struct ScanRequest    { pub sources: Vec<SourceId>, pub mode: ScanMode }  // Full | Delta
pub struct AnalyzeRequest { pub target: AnalyzeTarget, pub extractors: Option<Vec<ExtractorId>>, pub reanalyze: bool }
pub enum   AnalyzeTarget  { Assets(Vec<AssetId>), Query(QueryRequest), Source(SourceId) }
pub struct ConvertRequest {                                   // detailed shape owned by 08
    pub target: AnalyzeTarget, pub recipe: ConvertRecipe,
    pub output: OutputSpec, pub dry_run: bool,                // dry_run + non-destructive (DG §6)
}
pub struct JobListRequest { pub kinds: Vec<JobKind>, pub state: Option<JobState>, pub page: PageParams }
pub struct SubscribeRequest { pub topics: Vec<EventTopic>, pub scope: QueryScope }
pub enum   EventTopic { Assets, Sources, Jobs, Analysis }
```

---

## 4. DTOs — responses

Two asset shapes, deliberately: a **summary** (grid/table row — cheap, no heavy blobs) and the **full** `Asset`
(inspector). This keeps `query` light at 1M-asset scale ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §8) and defers the
expensive record to `get_asset`.

```rust
pub struct AssetSummary {
    pub id:        AssetId,
    pub name:      String,
    pub media:     MediaType,
    pub format:    String,
    pub size:      u64,
    pub thumb:     Option<PreviewRef>,    // ref, not bytes — fetched lazily via fetch_preview
    pub license:   LicenseBadge,          // id + status colour (permissive/attribution/restricted/unknown)
    pub top_tags:  Vec<String>,
    pub origin:    Origin,                // Local | Peer(PeerId) — attributable (PRODUCT_SPEC §4.4)
    pub key_attrs: SmallMap,              // a few media-specific display attrs (bpm, dims, tris)
}

pub struct Asset {                        // full inspector record
    pub summary:      AssetSummary,
    pub hash:         ContentHash,
    pub source:       SourceRef,
    pub path:         String,
    pub timestamps:   AssetTimes,         // created/modified/scanned/analyzed
    pub attributes:   MediaAttributes,    // enum over Audio/Image/Model attribute sets (PRODUCT_SPEC §5)
    pub license:      License,            // full rights block: id, summary, attribution, provenance
    pub tags:         Vec<TagRef>,        // confirmed + suggested, with source/explanation (DG §1.2)
    pub collections:  Vec<CollectionId>,
    pub derivatives:  Derivatives,        // thumb/waveform/embedding refs + analysis_version
}

pub struct SimilarHit { pub asset: AssetSummary, pub score: f32, pub explain: Option<SimExplain> }
pub struct DuplicateGroup { pub kind: DupKind, pub members: Vec<AssetSummary>, pub canonical: Option<AssetId> }

pub struct FacetResult { pub facets: Vec<FacetCounts>, pub partial: PartialStatus }
pub struct FacetCounts { pub field: FacetField, pub buckets: Vec<(FilterValue, u64)> }

pub struct TagInfo { pub name: String, pub count: u64, pub kind: TagKind }
pub struct TagEditResult { pub changed: u64, pub warnings: Vec<ItemWarning> }  // fail-soft per asset

pub struct CollectionInfo { pub id: CollectionId, pub name: String, pub kind: CollectionKindTag, pub count: u64 }
pub struct Collection { pub info: CollectionInfo, pub query: Option<QueryRequest> }  // query present iff smart

pub struct SourceInfo {
    pub id: SourceId, pub kind: SourceKind, pub uri: String,
    pub state: SourceState,               // Online | Offline | Scanning | Error(String) — fail-soft
    pub stats: SourceStats,               // asset count, last scan, last error
    pub auth_mode: AuthMode,              // Anonymous | Token | Oidc  (federated; details in 10)
}
pub struct SourceProbe { pub reachable: bool, pub auth_ok: bool, pub detail: Option<String> }

pub struct PreviewBlob { pub kind: PreviewKind, pub mime: String, pub bytes: Bytes, pub etag: String }

pub struct LibraryStats { pub total: u64, pub by_media: SmallMap, pub by_source: SmallMap,
                          pub unanalyzed: u64, pub duplicates: u64, pub partial: PartialStatus }

pub struct JobStatus {
    pub id: JobId, pub kind: JobKind,     // Scan | Analyze | Convert
    pub state: JobState,                  // Queued | Running | Paused | Done | Failed | Cancelled
    pub progress: Progress,               // done/total, current item, rate
    pub result: Option<JobResult>,        // convert manifest / scan delta counts, when Done
    pub warnings: Vec<ItemWarning>,       // per-item fail-soft (bad asset skipped, PRODUCT_SPEC §8)
}
```

### 4.1 The paginated envelope

```rust
pub struct Page<T> {
    pub items:  Vec<T>,
    pub cursor: Option<Cursor>,           // opaque; None ⇒ end of results (§6.1)
    pub total:  Option<u64>,              // best-effort; may be absent under fan-out/streaming
    pub partial: PartialStatus,           // fail-soft: which peers/sources were incomplete (§5.2)
    pub facets:  Option<FacetResult>,     // present on first page iff include_facets
}

// A soft, per-call degradation record — NOT an error. Carries fail-soft state up to the UI so
// it can show "3 of 4 peers responded" without failing the query (PRODUCT_SPEC §8, DG §2).
pub struct PartialStatus {
    pub complete: bool,
    pub peers:    Vec<PeerStatus>,        // per-federated-peer: responded | timed_out | offline | auth_failed (07)
    pub warnings: Vec<ItemWarning>,       // per-source/per-item soft failures
}
pub struct PeerStatus { pub peer: PeerId, pub state: PeerReplyState, pub took_ms: Option<u32> }
pub struct ItemWarning { pub subject: WarnSubject, pub code: WarnCode, pub message: String }
```

`PartialStatus` is the **fail-soft carrier** across the whole surface: any list/aggregate result can come back
`complete: false` with the reasons attached, and callers render a usable partial rather than an error. Peer-level
detail is populated by the fan-out layer ([07](07-sources-and-federation.md)); this file only fixes the shape.

---

## 5. The error model

### 5.1 `LibError`

One typed enum. It maps cleanly **both** to a Rust `Result::Err` and to an HTTP status (§8.4). It is reserved for
**hard failures of the whole call**; soft/partial degradation rides in `PartialStatus`/`warnings` on a *successful*
result (§4.1) — this split is the fail-soft rule made concrete.

```rust
#[derive(thiserror::Error, Debug)]
pub enum LibError {
    #[error("not found: {0}")]           NotFound(ResourceRef),          // 404
    #[error("invalid request: {0}")]     BadRequest(String),             // 400 (bad filter/cursor/recipe)
    #[error("unauthorized")]             Unauthorized,                   // 401 (auth required; 10)
    #[error("forbidden: {0}")]           Forbidden(ScopeNeed),           // 403 (scope/role insufficient; 10)
    #[error("conflict: {0}")]            Conflict(String),               // 409 (dup name, concurrent edit)
    #[error("unsupported: {0}")]         Unsupported(String),            // 422 (op not valid for this media/source)
    #[error("capability disabled: {0}")] Disabled(Capability),           // 403/404 (feature flag off; 10)
    #[error("source unavailable: {0}")]  SourceUnavailable(SourceId),    // 502 (file source offline — see note)
    #[error("upstream failed: {0}")]     Upstream(PeerId, String),       // 502 (federated peer hard-failed — see note)
    #[error("timed out")]                Timeout,                        // 504
    #[error("rate limited")]             RateLimited { retry_after: u32 },// 429
    #[error("cancelled")]                Cancelled,                      // 499 (client-cancelled job/stream)
    #[error("internal: {0}")]            Internal(String),               // 500 (never leaks internals to the wire)
}
```

**Fail-soft note on `SourceUnavailable`/`Upstream`.** These are `Err` **only when they sink the whole call** —
e.g. `get_asset` for an asset that lives *only* on an offline peer, or `probe_source` on an unreachable host. When a
`query`/`find_similar` fans out and *some* sources fail, that is **not** an error: the call returns `Ok(Page{ …,
partial })` with those sources marked in `PartialStatus.peers`. A call errors only if it can produce *no* usable
result at all.

### 5.2 Mapping rules

- **Rust:** `LibError` implements `std::error::Error`; every trait method returns `Result<_, LibError>`.
- **HTTP:** `impl From<LibError> for (StatusCode, Json<ErrorBody>)` in the server ([09](09-server-and-web-client.md)),
  per the code column above. `ErrorBody { code: ErrKind, message: String, detail: Option<Value> }` — a machine-stable
  `code` string plus a human message; `Internal` never serialises its inner string to the client.
- **API-client impl:** parses `ErrorBody.code` back into the matching `LibError` variant, so a connected client sees
  the *same* enum an embedded client would — parity holds through the wire.

---

## 6. Pagination & streaming

### 6.1 Cursor, not offset

All lists page by **opaque cursor**, not numeric offset — offset is O(n) and unstable under concurrent
inserts/deletes at 1M-asset scale, and it cannot describe a merged position across N federated peers.

```rust
pub struct PageParams { pub after: Option<Cursor>, pub limit: u32 }   // limit clamped to a server max
pub struct Cursor(String);  // opaque, server-encoded: sort keys + per-source positions + a query hash
```

- The `Cursor` encodes the **sort keys of the last item** plus, under fan-out, **each peer's continuation token**,
  plus a hash of the originating query. A cursor is only valid against the query that produced it (mismatch ⇒
  `BadRequest`).
- `cursor: None` in a returned `Page` means end-of-results. `total` is best-effort and often absent under fan-out or
  streaming (you don't know the total until every peer has answered).
- Federated continuation (interleaving N peer cursors, re-ranking a merged page) is [07](07-sources-and-federation.md);
  here the cursor is just the opaque container that makes it expressible.

### 6.2 Incremental / streamed delivery

Two delivery modes over the *same* query, so the UI never blocks ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1,
[PRODUCT_SPEC](../PRODUCT_SPEC.md) §8):

1. **Paged** (`query`, `find_similar`) — request a page, get a `Page`, ask for the next with its cursor. The
   default; simplest for the API-client and stateless HTTP.
2. **Streamed** (`query_stream`, `find_similar_stream`) — one call yields a `PageStream`: a sequence of `Page`
   chunks pushed **as they are computed** — the local index answers first, then each peer's page arrives and is
   merged, each chunk carrying an updated `PartialStatus`. The caller renders incrementally; a partial index/partial
   fan-out is usable immediately. The final chunk has `cursor: None`.

The in-process impl backs a `PageStream` with a bounded channel fed by the query engine; the API-client impl backs it
with the WS stream endpoint (§8.3). A front-end picks paged vs streamed by which method it calls — nothing else
changes.

---

## 7. Live updates at the trait level

Progress and change notifications are modelled as **event streams on the trait**, transport-carried over WebSocket
when connected (§8.3) and over in-memory channels when embedded.

```rust
pub enum LibraryEvent {
    AssetAdded(AssetSummary),
    AssetChanged(AssetId, ChangeKind),        // reanalyzed, retagged, license set …
    AssetRemoved(AssetId),
    SourceState(SourceId, SourceState),       // went offline/online, scan started/finished
    JobProgress(JobStatus),                   // scan/analyze/convert progress (mirrors watch_job)
    PeerState(PeerId, PeerReplyState),        // federated peer reachable/unreachable (07)
}

pub enum JobEvent { Progress(Progress), Warning(ItemWarning), Done(JobResult), Failed(LibError) }
```

- `subscribe` is the **library firehose**: `AssetAdded` as a scan discovers files, `JobProgress` as analysis runs,
  `SourceState`/`PeerState` as reachability changes. Filtered by `SubscribeRequest.topics` and scope (§2.1). This is
  what keeps the grid live during a scan ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §6.8, [DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1).
- `watch_job` is the **per-job stream** for a submitted scan/analyze/convert — the same `Progress` also surfaces in
  the firehose as `JobProgress`, so a client can watch one job or all activity.
- Streams are **scope-filtered** identically to reads: a subscriber sees only events for assets/sources within its
  visibility ceiling ([10](10-auth-accounts-and-flags.md)).
- Delivery is **at-least-once with a resume cursor** (a stream can carry a `since` position so a reconnecting WS
  client catches up); ordering is per-source, not globally total. Reconnect/backpressure specifics live with the
  server transport ([09](09-server-and-web-client.md)) and the worker/cancellation model ([14](14-concurrency-performance-reliability.md)).

---

## 8. HTTP + WebSocket API mapping

The wire form of §2, method-for-method. Bodies are the §3/§4 DTOs as JSON. **axum wiring, TLS, port-sharing with the
web client and MCP, and CORS are [09](09-server-and-web-client.md)** — this section is only the *contract* (route,
body, response, status).

### 8.1 Versioning

- All routes are under **`/api/v1`**. The major version is in the path; it changes only on a breaking contract change.
- Additive changes (new optional field, new facet, new event variant) are **non-breaking within `v1`** — clients
  ignore unknown fields, servers tolerate absent optional fields.
- The server advertises `GET /api/version` → `{ api: "v1", server: "3dam 0.x", capabilities: [...] }` so a client can
  negotiate and grey out unsupported features. `capabilities` reflects the enabled feature flags
  ([10](10-auth-accounts-and-flags.md)) — a disabled capability's routes return `Disabled` (and MCP's route is
  *removed*, per [ADR 0004](../adr/0004-feature-flags-admin.md)).
- The **federation contract between peers** uses this same `/api/v1` surface; its independent evolution/versioning is
  [07](07-sources-and-federation.md).

### 8.2 Route table (read + write)

| Method | Path | Body | → Response | Trait method |
|---|---|---|---|---|
| POST | `/api/v1/query` | `QueryRequest` | `Page<AssetSummary>` | `query` |
| POST | `/api/v1/facets` | `FacetRequest` | `FacetResult` | `facets` |
| GET | `/api/v1/assets/{id}` | — | `Asset` | `get_asset` |
| POST | `/api/v1/similar` | `SimilarRequest` | `Page<SimilarHit>` | `find_similar` |
| POST | `/api/v1/duplicates` | `DuplicateRequest` | `Page<DuplicateGroup>` | `find_duplicates` |
| GET | `/api/v1/stats` | — | `LibraryStats` | `library_stats` |
| GET | `/api/v1/tags` | *(query params)* | `Page<TagInfo>` | `list_tags` |
| POST | `/api/v1/tags/edit` | `TagEditRequest` | `TagEditResult` | `edit_tags` |
| POST | `/api/v1/tags/resolve` | `SuggestionDecision` | `204` | `resolve_suggestion` |
| POST | `/api/v1/assets/license` | `SetLicenseRequest` | `204` | `set_license` |
| GET | `/api/v1/collections` | — | `Vec<CollectionInfo>` | `list_collections` |
| GET | `/api/v1/collections/{id}` | — | `Collection` | `get_collection` |
| POST | `/api/v1/collections` | `CreateCollection` | `{ id }` `201` | `create_collection` |
| PATCH | `/api/v1/collections/{id}` | `UpdateCollection` | `204` | `update_collection` |
| DELETE | `/api/v1/collections/{id}` | — | `204` | `delete_collection` |
| POST | `/api/v1/collections/{id}/members` | `MemberEdit` | `204` | `edit_collection_members` |
| GET | `/api/v1/sources` | — | `Vec<SourceInfo>` | `list_sources` |
| GET | `/api/v1/sources/{id}` | — | `SourceInfo` | `get_source` |
| POST | `/api/v1/sources` | `AddSource` | `{ id }` `201` | `add_source` |
| DELETE | `/api/v1/sources/{id}` | `RemoveSource` | `204` | `remove_source` |
| POST | `/api/v1/sources/probe` | `ProbeSource` | `SourceProbe` | `probe_source` |
| GET | `/api/v1/assets/{id}/preview` | *(query: kind,size)* | `image/*` \| `audio/*` bytes | `fetch_preview` |
| POST | `/api/v1/jobs/scan` | `ScanRequest` | `{ job_id }` `202` | `submit_scan` |
| POST | `/api/v1/jobs/analyze` | `AnalyzeRequest` | `{ job_id }` `202` | `submit_analyze` |
| POST | `/api/v1/jobs/convert` | `ConvertRequest` | `{ job_id }` `202` | `submit_convert` |
| GET | `/api/v1/jobs` | *(query params)* | `Page<JobStatus>` | `list_jobs` |
| GET | `/api/v1/jobs/{id}` | — | `JobStatus` | `get_job` |
| POST | `/api/v1/jobs/{id}/cancel` | — | `204` | `cancel_job` |

Notes: `query`/`similar`/`facets` are **POST** because the request body (filters, vectors, cursors) is structurally
rich and often too large/complex for a URL — this is a read, not a mutation. `fetch_preview` returns raw bytes with an
`ETag`; it supports `If-None-Match` → `304` for cache reuse ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §3.2).
Preview/query for a **federated** asset transparently proxies from the origin peer ([07](07-sources-and-federation.md)).

### 8.3 Streaming & live-update endpoints

| Method | Path | Body | Delivery | Trait method |
|---|---|---|---|---|
| POST | `/api/v1/query?stream=1` | `QueryRequest` | NDJSON stream of `Page<AssetSummary>` chunks | `query_stream` |
| POST | `/api/v1/similar?stream=1` | `SimilarRequest` | NDJSON stream of `Page<SimilarHit>` chunks | `find_similar_stream` |
| GET (WS) | `/api/v1/ws` | *(subscribe frame)* | `LibraryEvent` frames | `subscribe` |
| GET (WS) | `/api/v1/ws/jobs/{id}` | — | `JobEvent` frames | `watch_job` |

- **Streamed queries** use HTTP chunked **NDJSON** (one `Page` JSON object per line) rather than WS — a query stream
  is request-scoped and one-directional, so it needs no socket. The API-client `PageStream` reads lines; the last line
  has `cursor: null`.
- **Live updates** use **WebSocket** at a single `/api/v1/ws` endpoint. The client opens the socket, sends a
  `SubscribeRequest` frame (topics + optional `since` resume cursor), and receives `LibraryEvent` frames. Job progress
  can be watched on the dedicated `/ws/jobs/{id}` socket or on the firehose as `JobProgress`. The socket is
  authenticated by the same `AuthContext` as the REST routes ([10](10-auth-accounts-and-flags.md)); the concrete WS frame
  envelope, ping/pong, and reconnect are [09](09-server-and-web-client.md).

### 8.4 Status-code mapping

`LibError` variants map to HTTP as annotated in §5.1: `NotFound`→404, `BadRequest`→400, `Unauthorized`→401,
`Forbidden`/`Disabled`→403 (a disabled *route* may instead be **absent**→404, per [ADR 0004](../adr/0004-feature-flags-admin.md)),
`Conflict`→409, `Unsupported`→422, `RateLimited`→429, `Cancelled`→499, `SourceUnavailable`/`Upstream`→502,
`Timeout`→504, `Internal`→500. A **fail-soft partial** is **200** with `partial.complete=false` in the body — never a
4xx/5xx, because usable results *were* returned.

---

## 9. Where this surface federates and where it is auth-scoped

Markers only — mechanics are deferred, per the borders.

- **Federates** ([07](07-sources-and-federation.md)): `query`/`query_stream`, `find_similar`/`find_similar_stream`,
  `facets`, `find_duplicates`, `fetch_preview` (proxy), and `library_stats` when `scope` includes peers. The
  fan-out, merge, re-rank, timeout and partial-peer policy are 07; this file guarantees the *result shapes*
  (`Origin` on every summary, `PartialStatus.peers` on every envelope) that let a merged answer be represented and a
  slow/offline peer be reported without failing the call.
- **Auth-scoped** ([10](10-auth-accounts-and-flags.md)): *every* method via `AuthContext`. Writes (`edit_tags`,
  `set_license`, `add_source`/`remove_source`, collection mutations, all `submit_*`) additionally require a
  write-capable scope and respect the **network-writes** feature flag; disabled → `Disabled`. Visibility scope
  silently filters rows and events (fail-soft), never errors.

---

## Open questions

- **Cursor stability across re-rank.** A federated cursor encodes per-peer positions plus a merge state; if a peer's
  ranking shifts between pages (new data, model update), the merged page boundary can drift. Snapshot the query at
  first page vs accept eventual drift? Interacts with [07](07-sources-and-federation.md)'s merge and
  [05](05-analysis-similarity-dedup.md)'s re-analysis.
- **Streamed-query transport choice.** NDJSON-over-HTTP (§8.3) vs reusing the WS channel vs SSE. NDJSON is simplest
  and stateless but has no server→client backpressure signal; revisit against real UI needs and the
  [09](09-server-and-web-client.md) transport layer.
- **`total` under fan-out.** Whether to ever compute a true total across peers (expensive, needs every peer to count)
  or always return `None` and let the UI show "N+"/"loading". Ties to [07](07-sources-and-federation.md).
- **Upload seed for `find_similar` (`ByUpload`).** Where the reference blob is embedded — client-side vs a server
  round-trip through the analysis pipeline ([05](05-analysis-similarity-dedup.md)) — and whether federated peers can
  accept a raw upload or only a precomputed vector (needs compatible embedding spaces, [PRODUCT_SPEC](../PRODUCT_SPEC.md) §10).
- **Event delivery guarantees.** At-least-once with a resume cursor (§7) vs stronger ordering; how much history the
  server buffers for a reconnecting `subscribe`; backpressure when a slow client falls behind a fast scan — overlaps
  [09](09-server-and-web-client.md) and [14](14-concurrency-performance-reliability.md).
- **Bulk-write result shape.** `edit_tags`/`set_license` over thousands of assets: return a summary count +
  warnings (current §4) vs a streamed per-asset result. Ties to the incremental principle.
- **Preview cache validators for federated assets.** Whether a peer's `ETag` can be trusted end-to-end through the
  proxy, or the local cache must mint its own — [07](07-sources-and-federation.md) / [02](02-data-model-and-storage.md).
```
