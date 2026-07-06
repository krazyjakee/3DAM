# 07 — Sources & Federation

Status: **Draft v0.1** · Scope: the `Source` trait spanning file and federated kinds; file sources (local FS, SFTP, SMB) with watch/delta re-scan and offline handling; the 3DAM-server federated source; and the federated query engine (fan-out, merge/re-rank, cross-peer similarity, timeouts/partial results).

This file is the low-level design for 3DAM's **source layer** — everything below `3dam-core`'s scan/query orchestration and above the wire (a filesystem, an SSH channel, an SMB session, or a peer's HTTP API). It fills in the mechanics behind the product spec's two-kinds-of-source model ([PRODUCT_SPEC §4.3–§4.4](../PRODUCT_SPEC.md), [§6.1](../PRODUCT_SPEC.md), [§6.7](../PRODUCT_SPEC.md)) and the "federate, don't reprocess" invariant ([DESIGN_GUIDELINES §2](../DESIGN_GUIDELINES.md)). It does not re-argue those decisions; it makes them implementable.

**Borders.** This file owns the source seam and the query fan-out that runs *on the client side of a federated edge*. It does not own:

- the `LibraryService` trait, its DTOs, error model, pagination, or the HTTP/WS surface a peer exposes — that is [03-library-service-and-api.md](03-library-service-and-api.md). The federated source is a *client* of that surface; where this file names an endpoint it is naming 03's contract, not defining it.
- **auth mechanics and credential storage** — that is [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md). This file *consumes* an opaque `AuthContext` and never inspects, negotiates, or persists a credential.
- how **similarity vectors are produced** (embedding models, ANN index, versioning) — that is [05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md). This file *transports* a query vector to peers and *merges* ranked hits; it never computes an embedding.
- the **remote-engine `--connect` mode** — that is a client-wiring concern in [01-architecture-and-crates.md](01-architecture-and-crates.md) / [03](03-library-service-and-api.md), not federation. See [§8](#8-not-federation-the---connect-remote-engine-mode) for the distinction.

Sibling reads: source records and the local blob/derivative cache in [02-data-model-and-storage.md](02-data-model-and-storage.md); byte processing done on file-source output in [04-media-handlers.md](04-media-handlers.md).

---

## 1. The two kinds behind one trait

Every place content can come from is a **`Source`**. There are exactly two kinds, and the trait's job is to let `3dam-core` treat them uniformly *up to the point where they diverge* — and to make that divergence explicit rather than leaky.

- **FILE sources** (`local`, `sftp`, `smb`) yield **raw bytes**. The engine enumerates entries, stats them, opens readers, and hands the bytes to [media handlers (04)](04-media-handlers.md) for local decode/thumbnail/embed. All local processing happens here and *only* here.
- **FEDERATED sources** (`3dam` — a peer 3DAM server) yield **catalog rows**. The engine forwards a query and merges the peer's already-built results. It **never** opens a byte reader against a federated asset for processing. Previews are remote-owned references, fetched on demand and cached ([§7.4](#74-license-read-only-references-and-remote-previews)).

The load-bearing rule ([DESIGN_GUIDELINES §2](../DESIGN_GUIDELINES.md)): **federate, don't reprocess.** Local processing is only ever for *your* file sources. A federated source that tried to expose a byte-reader for reprocessing would be a bug, and the trait is shaped so it cannot: the two capability surfaces are separate traits (`FileSource`, `FederatedSource`) behind one `Source` enum-of-capabilities, so a caller must first ask *which kind* before it can reach byte I/O.

```
                         trait Source (identity, kind, health, auth)
                                        │
                 ┌──────────────────────┴──────────────────────┐
          kind = File                                    kind = Federated
                 │                                              │
          trait FileSource                              trait FederatedSource
   enumerate · stat · open_reader                query · similar · fetch_preview
   scan (incremental/resumable/cancel)           advertise() -> peer capabilities
   watch (local only) · online/offline           NO byte reader for processing
                 │                                              │
        bytes → media handlers (04)               catalog rows → merge (§6)
        → local index + embeddings (05)           remote-owned; not reprocessed
```

### 1.1 Common surface — `Source`

```rust
/// Kind is fixed at construction and never changes.
pub enum SourceKind { Local, Sftp, Smb, Federated }

/// Runtime reachability, persisted on the source record (02) and surfaced in UI.
pub enum Health {
    Online,
    Offline { since: Instant, last_error: SourceError },
    /// Reachable but the peer/host declined (auth expired, permission).
    /// Distinct from Offline so UI can prompt re-auth vs "check the network".
    Degraded { reason: DegradeReason },
}

pub trait Source: Send + Sync {
    fn id(&self) -> SourceId;              // stable, from the source record (02)
    fn kind(&self) -> SourceKind;
    fn display_name(&self) -> &str;

    /// Cheap liveness probe. MUST NOT block long: bounded by a short connect
    /// timeout. Updates and returns Health; callers persist it (02) and never
    /// treat Offline as fatal — cached results stay usable (§4.4, §7.3).
    async fn health_check(&self) -> Health;

    /// Narrow to a capability surface. Exactly one is Some per kind; this is the
    /// only door to byte I/O, so a Federated source structurally cannot leak one.
    fn as_file(&self) -> Option<&dyn FileSource> { None }
    fn as_federated(&self) -> Option<&dyn FederatedSource> { None }

    /// Every source carries an auth context. Its contents are opaque here — 10
    /// owns what's inside and how it was obtained/stored. We pass it through on
    /// every remote call and, on a 401/expired signal, mark Health::Degraded and
    /// ask 10 to refresh; we never read or persist the credential ourselves.
    fn auth(&self) -> &AuthContext;        // defined by 10-auth-accounts-and-flags.md
}
```

`AuthContext` is defined and populated by [10](10-auth-accounts-and-flags.md). For a `local` source it is the trivial "no auth"; for `sftp`/`smb` it is the connection secret; for `3dam` it is anonymous / token / OIDC per the peer's mode. This file only ever *holds and forwards* it.

---

## 2. File sources — the `FileSource` surface

`FileSource` is the byte-yielding half. Its contract is: enumerate a tree lazily, stat entries cheaply, open a reader on demand, and drive a scan that is **incremental, resumable, and cancellable** ([DESIGN_GUIDELINES §1.1](../DESIGN_GUIDELINES.md)).

```rust
pub struct Entry {
    pub rel_path: RelPath,        // source-root-relative, normalised to '/'
    pub kind: EntryKind,          // File | Dir | Symlink | Other
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Cheap change-token when the backend offers one (mtime+size, or an
    /// SFTP/SMB attribute set). NOT a content hash — content hashing is a
    /// media-handler concern (04) done on bytes we actually open.
    pub etag: ChangeToken,
}

pub trait FileSource: Send + Sync {
    /// Lazily list a directory. Streamed so a million-entry tree never
    /// materialises in RAM (out-of-core, DESIGN_GUIDELINES §1.1).
    fn list_dir(&self, dir: &RelPath) -> BoxStream<Result<Entry, SourceError>>;

    /// Stat one entry without opening it. Used by delta re-scan to decide
    /// "changed?" before paying to open bytes.
    async fn stat(&self, path: &RelPath) -> Result<Entry, SourceError>;

    /// Open a byte reader. AsyncRead + AsyncSeek where the backend supports
    /// seek (local, SFTP); SMB is AsyncRead with best-effort seek. Handlers (04)
    /// that need random access (container header probes) check the capability.
    async fn open_reader(&self, path: &RelPath)
        -> Result<Box<dyn AsyncReadSeek>, SourceError>;

    fn capabilities(&self) -> FileCaps;   // { seekable, watchable, case_sensitive }

    /// Watch for changes; None if unsupported (SFTP/SMB → poll instead, §3.3).
    fn watch(&self) -> Option<BoxStream<WatchEvent>>;
}

pub enum WatchEvent {
    Created(RelPath), Modified(RelPath), Removed(RelPath),
    Renamed { from: RelPath, to: RelPath },
    /// Backend lost sync (overflow, remount) — caller must fall back to a full
    /// delta re-scan of the affected subtree rather than trust events.
    Desync { subtree: RelPath },
}
```

### 2.1 The scan flow — incremental, resumable, cancellable

The scan is driven by `3dam-core`, not the source; the source only supplies `list_dir`/`stat`/`open_reader`. The flow below is the contract every file source must satisfy through those primitives.

```
scan(source, cursor?, cancel_token) -> stream of ScanEvent
  1. seed the frontier from cursor (resume) or the source root (fresh)
  2. loop, checking cancel_token between every unit of work:
       a. pop a dir from the frontier
       b. list_dir(dir) → for each Entry:
            - Dir       → push to frontier
            - File      → compare Entry.etag to the stored source record (02):
                            unchanged → skip (emit Unchanged for progress)
                            new/changed → enqueue for processing
       c. persist an updated ScanCursor (frontier + last-committed position)
          transactionally after each dir — this is what makes it resumable
  3. processing (off the scan walk, on the bounded worker pool, 14):
       open_reader → media handler (04) → index + embeddings (05)
       each asset commits independently → partial index usable immediately (§1.1)
```

- **Incremental.** Every processed asset is committed and visible before the scan finishes; the UI/API stream `ScanEvent`s (`Discovered`, `Processed`, `Skipped`, `Failed`, `Progress`) as they happen. A partial index is a usable index.
- **Resumable.** The `ScanCursor` (serialised frontier + last-committed dir) is persisted transactionally per directory in the sources store ([02](02-data-model-and-storage.md)). A crash, a `--connect` drop, or an operator Ctrl-C resumes from the cursor instead of restarting the walk. Cursors are keyed by `(source_id, scan_epoch)`; bumping the epoch (e.g. after a config change) forces a clean full walk.
- **Cancellable.** A `CancellationToken` ([14](14-concurrency-performance-reliability.md)) is checked between every directory listing and every processing unit. Cancel is cooperative and prompt; the last persisted cursor lets a later scan resume where cancel landed.
- **Fail-soft.** A bad file, an unreadable format, or a mid-scan I/O error on one entry emits `ScanEvent::Failed { path, error }` and continues. One corrupt asset never aborts the scan; a dropped share mid-walk marks the source `Offline` and pauses (resumable) rather than failing ([DESIGN_GUIDELINES §2](../DESIGN_GUIDELINES.md), [§6.1](../PRODUCT_SPEC.md)).

### 2.2 Delta re-scan

A re-scan (manual, watch-triggered, or watch-`Desync` fallback) is the same walk with `stat`-based short-circuiting: for each known asset, compare the live `Entry.etag` to the stored token; open bytes only for new/changed/removed entries. Removed entries are marked absent (not deleted — [non-destructive, §1.3](../DESIGN_GUIDELINES.md)); their catalog rows persist as offline until the user prunes. Content hashing / near-dup re-evaluation on changed bytes is [04](04-media-handlers.md)/[05](05-analysis-similarity-dedup.md)'s job downstream.

---

## 3. Concrete file sources

All three implement `FileSource` behind the trait; `3dam-core` above them is identical. Differences are confined to connection setup, seek/watch capability, and offline semantics.

### 3.1 Local FS (`local`)

- `std::fs` / `tokio::fs`. `list_dir` streams `read_dir`; `open_reader` returns a seekable file. `capabilities = { seekable: true, watchable: true }`.
- **Watch** via a filesystem-notification crate (inotify/FSEvents/ReadDirectoryChangesW behind `notify`), debounced (coalesce a burst of writes to one `Modified`), mapped to `WatchEvent`. On `Desync` (event-queue overflow, or a bind-mount going away and returning) → delta re-scan of the subtree ([§2.2](#22-delta-re-scan)).
- **Offline** = the path is gone (unmounted drive, missing network mount presented as a local path). `health_check` stats the root; failure → `Offline`. Cached catalog rows and derivatives remain browsable and searchable; the assets show an offline badge and cannot be re-opened until the mount returns.

### 3.2 SFTP (`sftp`)

- Client via **`russh`** (pure-Rust, async, preferred) with **`ssh2`** (libssh2 binding) as the fallback/alt-feature, per [PRODUCT_SPEC §7](../PRODUCT_SPEC.md); selected by crate feature (see [01](01-architecture-and-crates.md)). Connection params + secret come from the `AuthContext` ([10](10-auth-accounts-and-flags.md)).
- One pooled session per source; SFTP file handles for `open_reader`. `capabilities = { seekable: true, watchable: false }` — SFTP has no push notifications.
- **No watch → poll.** `watch()` returns `None`; delta re-scan runs on a configurable interval (default off; opt-in per source) using `stat` etags. Cheap because it walks attributes, not bytes.
- **Offline / unreachable.** Connect/handshake/timeout errors → `Offline { last_error }`; auth-declined (permission, expired key) → `Degraded` so [10](10-auth-accounts-and-flags.md) can prompt re-auth rather than the UI blaming the network. Reconnect is lazy with backoff on next access; the source's cached rows stay usable throughout.

### 3.3 SMB (`smb`)

- Client via an SMB crate (SMB2/3), per [PRODUCT_SPEC §7](../PRODUCT_SPEC.md). Share + credentials from the `AuthContext`.
- `open_reader` is `AsyncRead` with best-effort seek; `capabilities.seekable` reflects what the negotiated dialect supports. Handlers (04) needing random access fall back to buffered read when seek is unavailable.
- **No reliable watch → poll**, as SFTP. (SMB change-notify exists but is uneven across servers; v1 treats SMB as poll-only and revisits notify as an open question.)
- **Offline / unreachable.** Same policy as SFTP: unreachable host → `Offline`; auth rejection → `Degraded`. Session re-established lazily with backoff.

**Shared offline invariant (all file sources).** Losing a source is never fatal ([§6.1](../PRODUCT_SPEC.md), [DESIGN_GUIDELINES §2](../DESIGN_GUIDELINES.md)): mark it `Offline`/`Degraded`, keep every cached catalog row, thumbnail, waveform, and embedding fully usable for browse/search/similarity, pause any in-flight scan at its cursor, and resume automatically when health returns.

---

## 4. The federated source — the `FederatedSource` surface

A `3dam` source is a peer 3DAM server. It satisfies `Source` (`kind = Federated`) and exposes `FederatedSource` instead of `FileSource`. It is a **client of the peer's [LibraryService HTTP/WS API (03)](03-library-service-and-api.md)** — it calls the *same* endpoints the peer's own web client and CLI call. It holds no byte reader for processing and computes nothing locally except the final merge ([§6](#6-the-federated-query-engine)).

```rust
pub trait FederatedSource: Send + Sync {
    /// Peer self-description, cached with a TTL. Advertises catalog size, the
    /// facets it supports, and — load-bearing for cross-peer similarity —
    /// its embedding model+version per media type (§5). Shape defined by 03.
    async fn advertise(&self) -> Result<PeerCapabilities, SourceError>;

    /// Forward a text+facet query. Returns catalog rows + preview refs the peer
    /// already built. NO bytes fetched or processed. Paginated per 03's cursor
    /// contract; carries a per-call deadline (§6.3).
    async fn query(&self, q: &FederatedQuery, deadline: Instant)
        -> Result<PeerPage, SourceError>;

    /// Forward a similarity query BY VECTOR. The peer searches ITS OWN index
    /// (05) and returns ranked hits. We never send bytes and never run ANN for
    /// the peer — we only merge what comes back (§5, §6.2).
    async fn similar(&self, embedding: &QueryVector, media: MediaType,
                     k: usize, deadline: Instant)
        -> Result<PeerRankedPage, SourceError>;

    /// Fetch a remote-owned preview (thumbnail/waveform PNG, etc.) on demand.
    /// This is the ONLY byte transfer a federated source does, and it is a
    /// derivative, never a source file for reprocessing. Cached locally (§7.4).
    async fn fetch_preview(&self, asset: &PeerAssetRef)
        -> Result<PreviewBlob, SourceError>;
}
```

`FederatedQuery`, `PeerPage`, `PeerAssetRef`, and the row DTOs are the wire shapes **owned by [03](03-library-service-and-api.md)** — this file consumes them. Every call carries the source's `AuthContext` ([10](10-auth-accounts-and-flags.md)) in the transport headers; a `401`/expired response flips the source to `Degraded` and asks [10](10-auth-accounts-and-flags.md) to refresh, exactly as SFTP/SMB auth rejection does.

### 4.4 Offline / partial peers

A slow, unreachable, or auth-lapsed peer is handled by the **same fail-soft posture** as an offline file source, but the payoff differs: rather than pausing a scan, the query engine *drops that peer from this round* and returns the rest ([§6.3](#63-timeouts-and-partial-results)). Whatever slice of the peer's catalog is cached locally ([§7.5](#75-how-much-to-cache)) stays searchable while the peer is down, tagged stale.

---

## 5. Cross-peer embedding-space compatibility

Merging *text/facet* hits across peers is safe — scores are comparable enough to interleave by the peer-reported relevance plus local tie-breaks ([§6.1](#61-merge-and-re-rank)). Merging *similarity* hits across peers is only meaningful when the peers embed into the **same space**: a cosine distance from peer A's index is comparable to peer B's only if both used the same model **and** version ([PRODUCT_SPEC §10 — cross-peer similarity](../PRODUCT_SPEC.md)). Vectors are produced by [05](05-analysis-similarity-dedup.md); this file only checks compatibility and decides how to present the result.

**Advertise.** `advertise()` returns, per media type, an `EmbeddingSpace { model_id, model_version, media, dim, metric }`. The local engine knows its own space (from [05](05-analysis-similarity-dedup.md)). Compatibility is exact-match on `(model_id, model_version, media, dim, metric)`.

**Gate, then choose a presentation:**

```
for a "find similar" across peers P1..Pn (media = M):
  local_space = analysis(05).embedding_space(M)
  compatible  = { Pi : Pi.advertise().space(M) == local_space }
  incompatible = peers - compatible

  compatible peers   → send query vector, merge ranked hits into ONE ranked list
                       (unified cross-peer ranking, §6.2)
  incompatible peers → do NOT interleave by score. Either:
                         (a) omit from similarity (default, safest), or
                         (b) run their similar() and present as a SEPARATE,
                             per-peer-grouped section labelled "similar on <peer>"
                             — never mixed into the unified ranking.
```

The fallback is **per-peer-grouped, not cross-ranked** results — the option [PRODUCT_SPEC §10](../PRODUCT_SPEC.md) calls out. Which of (a)/(b) is default, and whether to attempt space negotiation, is an [open question](#open-questions). Text/facet fan-out is unaffected by space mismatch and always merges.

---

## 6. The federated query engine

The federated query engine runs **inside `3dam-core` on the querying side**. It fans a single library query out to the local index and every online federated source, merges the returns into one view, re-ranks, and tags each hit with its origin peer. It is the concrete implementation of [PRODUCT_SPEC §4.4](../PRODUCT_SPEC.md) / [§6.3](../PRODUCT_SPEC.md) / [§6.7](../PRODUCT_SPEC.md).

```
                    federated query  (text | facets | similarity-vector)
                                     │
                        ┌────────────┴─────────────┐
                        ▼                           ▼
                 local index (05/02)         fan-out to peers  (bounded concurrency, 14)
                        │                    ┌───────┬───────┬───────┐
                        │                    ▼       ▼       ▼       ▼
                        │                  peer 1  peer 2  peer 3   peer N
                        │                  query/  query/  (slow)  (offline)
                        │                  similar similar   │        │
                        │                    │       │     deadline  dropped
                        │                    │       │     exceeded    │
                        │                    ▼       ▼       ✗          ✗
                        └────────────►  ┌──────────────────────────────────┐
                                        │   MERGE  (interleave by score)    │
                                        │   RE-RANK (§6.1) + DEDUP (§6.4)    │
                                        │   tag each hit with origin peer   │
                                        └──────────────────┬───────────────┘
                                                           ▼
                                        one ranked page  +  { peers_ok, peers_partial,
                                                              peers_dropped }  (§6.3)
```

### 6.1 Merge and re-rank

Each source returns a page of scored rows: local hits with local scores, each peer with its own scores. Because scoring is not globally normalised across independent indexes, the merge:

1. **Normalises** each source's scores to a common `[0,1]` within the returned page (min-max on the page, or the peer's advertised score scale when it provides one), so no single peer dominates purely by score magnitude.
2. **Interleaves** by normalised score into one ordered list.
3. **Tie-breaks** deterministically: local before remote at equal score (locality is cheaper to open), then by stable asset identity, so pagination is stable across pages.
4. **Tags** every hit with `origin: SourceId` and carries the peer's `PeerAssetRef` so previews resolve back to the right peer ([§7.4](#74-license-read-only-references-and-remote-previews)) and results stay attributable ([PRODUCT_SPEC §5](../PRODUCT_SPEC.md), [§6.3](../PRODUCT_SPEC.md)).

Facet aggregation (counts per license, per format, etc.) sums facet buckets across the sources that answered, flagged partial when any peer was dropped.

### 6.2 Federated similarity

"Find similar" sends the **query embedding** (produced locally by [05](05-analysis-similarity-dedup.md)) to each *compatible* peer's `similar()`. Each peer runs ANN against **its own** vector index and returns its top-k with distances. The engine merges those into the local top-k by distance ([§5](#5-cross-peer-embedding-space-compatibility) gates which peers are eligible for unified ranking; incompatible peers are grouped or omitted). We transport the vector and merge the ranks; we never run a peer's ANN and never re-embed a peer's asset.

### 6.3 Timeouts and partial results

The engine never blocks the user on the slowest peer ([DESIGN_GUIDELINES §1.1](../DESIGN_GUIDELINES.md); [PRODUCT_SPEC §10 — federated query semantics](../PRODUCT_SPEC.md)).

- **Per-query deadline.** Each query carries a wall-clock `deadline` (default ~2 s for interactive, configurable per source and per call). Local results always return; each peer call is raced against the deadline.
- **Partial results are first-class.** When the deadline fires, peers that answered are merged and returned *now*; peers still in flight are **dropped from this round**, not awaited. The response carries `{ peers_ok, peers_partial, peers_dropped }` so the UI can show "showing local + 2 of 4 sources" and offer a refresh rather than silently under-reporting.
- **Offline / error peers** are dropped identically and reported in `peers_dropped` with reason (`Offline` | `Degraded` | `Timeout`). A dropped peer never taints the merged results from the peers that succeeded — degrade one edge, not the query ([DESIGN_GUIDELINES §2](../DESIGN_GUIDELINES.md)).
- **Bounded fan-out.** Peer calls run on a bounded-concurrency pool ([14](14-concurrency-performance-reliability.md)) so N peers cannot exhaust connections; slow peers occupy a slot only until the deadline.

### 6.4 Pagination and result caps across peers

- **Per-peer caps.** Each fan-out request asks each peer for at most `k` rows (a bounded over-fetch of the page size, so the merge has enough candidates to rank fairly without pulling whole catalogs). Peer catalogs are never enumerated wholesale by a query.
- **Cross-peer pagination** uses an **opaque composite cursor**: a map of `SourceId → per-peer cursor` (each per-peer cursor is 03's own pagination token) plus the merge high-water mark. Paging forward advances each peer's sub-cursor and resumes the interleave deterministically, so page 2 continues cleanly from page 1 even though the sources are independent.
- **Global cap.** A hard ceiling on total merged rows per query bounds worst-case memory when many peers each return a full `k`; beyond it, the user pages.

---

## 7. Federated assets: license, references, previews, caching

### 7.4 License, read-only references, and remote previews

- **License travels with the asset.** Each federated row carries the peer-published license/rights block ([PRODUCT_SPEC §5](../PRODUCT_SPEC.md), [§6.7](../PRODUCT_SPEC.md)) — SPDX id, rights summary, attribution, provenance — verbatim from the peer. The [license facet (03)](03-library-service-and-api.md) therefore works across peers unchanged ("commercial-use assets across every store I've connected"). 3DAM transports and displays it; pricing/gating by license stays the peer's concern.
- **Read-only references.** A federated asset is a reference owned by a peer, tagged with its origin. 3DAM stores enough to list/filter/rank it (identity, key attributes, tags, license, preview ref) but **never** regenerates its derivatives — they were computed remotely ([DESIGN_GUIDELINES §2](../DESIGN_GUIDELINES.md)). No local write path mutates a federated asset.
- **Remote-owned previews, fetched on demand + cached.** Previews resolve lazily through `fetch_preview()` and land in the local blob cache ([02](02-data-model-and-storage.md)) keyed by `(source_id, peer_asset_id, derivative_kind, remote_etag)`, so a thumbnail is pulled once and reused; a change of `remote_etag` (from a later `query`/`advertise`) invalidates the cache entry. The original file is downloaded only if the user explicitly asks *and* the peer's permissions allow — that is an explicit user action, never part of a query, and never triggers local reprocessing.

### 7.5 How much of a peer's catalog to cache

To stay responsive while a peer is slow or offline, 3DAM caches a **bounded local shadow** of federated results ([PRODUCT_SPEC §10 — federated query semantics](../PRODUCT_SPEC.md)):

- **Rows** returned by any `query`/`similar` are cached (identity, attributes, tags, license, preview ref) with a TTL and an LRU cap per source, so recent/repeated queries and offline browsing work against the shadow. This is a cache, not a mirror — 3DAM does not pull a peer's whole catalog (that would re-create the mesh the [non-goals](../PRODUCT_SPEC.md) exclude in v1).
- **Freshness.** Cached rows carry the `advertise()`/response `etag`; a peer coming back online refreshes lazily on next query. Stale rows are shown labelled stale, never as if live.
- **Preview blobs** are cached as [§7.4](#74-license-read-only-references-and-remote-previews). The exact cap sizing and eviction policy are an [open question](#open-questions).

---

## 8. Not federation: the `--connect` remote-engine mode

`--connect host:port` is a **different thing** and must not be confused with a federated source ([PRODUCT_SPEC §4.3 — remote *engine*](../PRODUCT_SPEC.md)):

- **Federated source** (this file): a peer is *one source among many* in a local library. The local engine still runs, owns the local index, and *merges* the peer's catalog into local results. Many peers, one merged view.
- **`--connect` remote engine**: the GUI/CLI runs **no local engine** and points its `LibraryService` at a single remote `3dam serve` as its *entire* backend. There is no local library and no fan-out — it is pure client↔engine wiring.

That wiring (which `LibraryService` implementation a client binds, how `--connect` selects it) is owned by [01-architecture-and-crates.md](01-architecture-and-crates.md) and [03-library-service-and-api.md](03-library-service-and-api.md), not here. A `--connect`ed client whose *remote* engine has federated sources of its own still gets federated results — because the fan-out described here runs on that server, transparently to the thin client.

---

## Open questions

Carried from [PRODUCT_SPEC §10](../PRODUCT_SPEC.md); this file scopes them to source/federation mechanics.

- **Cross-peer similarity** ([§5](#5-cross-peer-embedding-space-compatibility)). Exact-match gating on `(model_id, model_version, media, dim, metric)` is the floor. Open: whether the default for incompatible peers is *omit* (safe) or *per-peer-grouped section*; whether to attempt embedding-space negotiation or a shared reference space; and how peers advertise per-media-type spaces as models are re-versioned by [05](05-analysis-similarity-dedup.md). Needs a spike.
- **Federated query semantics** ([§6.3](#63-timeouts-and-partial-results)–[§6.4](#64-pagination-and-result-caps-across-peers), [§7.5](#75-how-much-of-a-peers-catalog-to-cache)). Default interactive deadline; whether deadlines should be adaptive per peer latency; result caps and per-peer over-fetch `k`; the composite-cursor stability contract under peers going offline mid-pagination; and the local shadow-cache cap/TTL/eviction sizing. Needs measurement at N peers.
- **Federation protocol & versioning.** The federated source calls [03](03-library-service-and-api.md)'s endpoints, but the *inter-instance contract* — which subset of 03's surface is the stable federation API, how `advertise()` carries protocol/schema version, how a newer peer degrades gracefully for an older caller (and vice-versa), and the auth-standard subset [10](10-auth-accounts-and-flags.md) supports first — needs pinning as the seed of a future mesh ([PRODUCT_SPEC §9](../PRODUCT_SPEC.md) future direction). Cross-links: [03](03-library-service-and-api.md) (surface), [10](10-auth-accounts-and-flags.md) (auth subset).

---

See also: [00-overview.md](00-overview.md) · [01-architecture-and-crates.md](01-architecture-and-crates.md) · [02-data-model-and-storage.md](02-data-model-and-storage.md) · [03-library-service-and-api.md](03-library-service-and-api.md) · [04-media-handlers.md](04-media-handlers.md) · [05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md) · [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md) · [14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md) · [PRODUCT_SPEC §4.4](../PRODUCT_SPEC.md) · [DESIGN_GUIDELINES §2](../DESIGN_GUIDELINES.md)
