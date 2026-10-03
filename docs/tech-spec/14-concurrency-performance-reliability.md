# 14 — Concurrency, performance & reliability

Status: **Draft v0.1** · Scope: the execution model — the tokio (I/O) + rayon (CPU) split, bounded worker pools with backpressure, the incremental non-blocking pipeline, cancellation/resumability, out-of-core data access at 1M+ assets, fail-soft as a pipeline mechanism, and the performance targets and how they are benchmarked.

The **tokio (async I/O) + rayon (CPU) foundation** described here is ratified by [ADR 0007](../adr/0007-concurrency-runtime-tokio-rayon.md) (2026-07-06); scale is validated by the [15](15-observability-config-testing-packaging.md) benchmark harness as a standing regression guard, not a go/no-go spike.

This file is the **cross-cutting concurrency/performance/reliability layer**. It does not decide *what* work is done — scanning ([07](07-sources-and-federation.md)), cheap metadata and decode ([04](04-media-handlers.md)), feature extraction / embeddings / similarity ([05](05-analysis-similarity-dedup.md)), render ([06](06-3d-render.md)), and convert ([08](08-convert-pipeline.md)) each own their domain logic — it owns **how that work is scheduled, bounded, cancelled, and streamed**. Those files plug their per-item functions into the primitives defined here; this file guarantees the UI stays at 60 fps and the process stays inside its memory/FD budget while they run ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §7, §8; [DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1, §2, §6).

**Borders (do not write outside them).**
- **Where these primitives live** in the crate graph is [01](01-architecture-and-crates.md): the runtime, pools, and pipeline are in **`3dam-core`** (which is the only crate allowed to depend on both `tokio` and `rayon`; front-ends never see them). This file names the topology; 01 fixes the crate that holds it.
- **The trait-level delivery shape** — `PageStream`, `EventStream`, `Progress`, `JobStatus`, cursors, the soft-warning vs hard-error split — is [03](03-library-service-and-api.md) §6–§7. This file drives those streams; it does not redefine their DTOs.
- **The persisted `job` row** (resume state, `state`/`progress`/`params`) is [02](02-data-model-and-storage.md) §3.6; the **derivative cache / vector index** it reads and writes are [02](02-data-model-and-storage.md) §7–§8. This file owns the *runtime* checkpoint/resume mechanics against that row, not its schema.
- **Domain per-item logic** is off-limits: this file must not respec extraction tiers ([04](04-media-handlers.md) §4), embedding production ([05](05-analysis-similarity-dedup.md)), the render path ([06](06-3d-render.md)), source I/O semantics ([07](07-sources-and-federation.md)), or the convert job model ([08](08-convert-pipeline.md)). It references them as **stage bodies**.
- **Wire-level backpressure/reconnect** on the WebSocket, and the axum task wiring, are [09](09-server-and-web-client.md). This file owns the in-process channels; 09 owns the socket.
- **Metrics, tracing, and the benchmark harness/CI regression gates** are [15](15-observability-config-testing-packaging.md). This file states the *targets and how they map to measurements*; 15 owns the *instrumentation and test rig*.

Pseudocode is indicative, per the spec convention.

---

## 1. What this layer must guarantee

Distilled from [PRODUCT_SPEC](../PRODUCT_SPEC.md) §8 and [DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1 / §2 / §6 — the mechanics below exist to make these true, not to re-argue them:

1. **The UI thread never blocks.** No scan, decode, hash, thumbnail, embed, ANN query, or DB write runs on the caller's thread. Front-ends call an `async` `LibraryService` ([03](03-library-service-and-api.md)); everything heavy is off-thread.
2. **Incremental over batch.** A partial index is usable *immediately*. Results are delivered as computed — a scan populates the grid as it discovers files, analysis fills attributes behind them, never one final dump.
3. **Bounded everything.** At 1M+ assets, unbounded queues/opens/allocations are the failure mode. Every pool has a fixed width; every queue is a bounded channel; ingest applies **backpressure** rather than buffering the world.
4. **Cancellable & resumable.** Any long op (scan, analyze, convert) cancels mid-flight promptly, and resumes after a restart from its last checkpoint.
5. **Out-of-core.** The working set is bounded by the viewport + a cache budget, not by library size. RAM does not scale with asset count.
6. **Fail-soft, structurally.** One bad file or offline source degrades *that item* and is captured as a per-item warning; the pipeline keeps running.
7. **Measured & regression-guarded.** The §8 numbers are benchmarked against messy, at-scale fixtures and watched in CI ([15](15-observability-config-testing-packaging.md)).

---

## 2. The two runtimes and their handoff

3DAM runs **two schedulers on purpose**, because its work splits cleanly into two profiles that starve each other if pooled together ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §7):

- **`tokio` — I/O-bound, async, high-cardinality.** Source access (local FS, SFTP, SMB — [07](07-sources-and-federation.md)), federated peer queries and preview fetches, the HTTP/WS server ([09](09-server-and-web-client.md)), and all SQLite access ([02](02-data-model-and-storage.md)). Thousands of these can be in flight cheaply because they are mostly *waiting*. This is where futures, timeouts, and cancellation tokens live.
- **`rayon` — CPU-bound, synchronous, core-saturating.** Decode ([04](04-media-handlers.md)), hashing, perceptual hashing, DSP/spectral features, embedding inference ([05](05-analysis-similarity-dedup.md)), thumbnail encode, mesh stats, and convert transcode ([08](08-convert-pipeline.md)). These want *all cores*, want work-stealing, and must **never** run on a tokio worker (a 200 ms decode on an async worker stalls every other future sharing that thread — including the request that keeps the UI at 60 fps).

### 2.1 The topology

```
                          3dam-core process
  ┌───────────────────────────────────────────────────────────────────────┐
  │                                                                         │
  │   tokio runtime (async, I/O)                rayon pool (CPU)            │
  │   ┌───────────────────────────┐             ┌────────────────────────┐ │
  │   │  N_io worker threads      │             │  N_cpu worker threads   │ │
  │   │  = min(cores, cap)        │             │  = cores-1 (work-steal) │ │
  │   │                           │  handoff    │                        │ │
  │   │  • source read/list (07)  │ ──spawn──►  │  • decode (04)          │ │
  │   │  • SFTP/SMB/net (07)      │  _blocking  │  • hash / phash         │ │
  │   │  • federated peers (07)   │  or oneshot │  • DSP / spectral (05)  │ │
  │   │  • SQLite (rusqlite) (02) │ ◄─result──  │  • embed inference (05) │ │
  │   │  • HTTP/WS server (09)    │             │  • thumbnail encode (04)│ │
  │   └─────────────┬─────────────┘             └───────────┬────────────┘ │
  │                 │                                        │              │
  │                 │        bounded mpsc channels           │              │
  │                 ▼        (backpressure, §3)              ▼              │
  │        ┌────────────────────── pipeline stages (§4) ───────────────┐   │
  │        │  scan ─► cheap-meta ─► store ─► [decode ─► thumb/embed]   │   │
  │        └───────────────┬───────────────────────────────────────────┘   │
  │                        │  results as computed                          │
  │                        ▼                                               │
  │        PageStream / EventStream (03 §6–§7) ─► LibraryService caller    │
  │                                                                         │
  │   Special: the wgpu render pool (06) is a *separate*, size-1..N GPU     │
  │   queue — GPU work is serialised per device, not run on rayon.         │
  └───────────────────────────────────────────────────────────────────────┘
```

Two runtimes, sized independently, connected only by **bounded channels** and explicit **handoff points**. Neither pool submits work directly onto the other's threads.

### 2.2 The handoff — never block an async worker

A pipeline stage that is async but must run CPU work hands off with a one-shot round-trip onto rayon, and awaits the result without occupying a tokio worker:

```rust
/// Run a CPU-heavy closure on the rayon pool from async code, without ever
/// executing it on a tokio worker thread. The tokio task parks on `rx.await`
/// (yields its worker); rayon runs `f` on a CPU thread; the result comes back.
pub async fn cpu<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Cancelled> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    RAYON_POOL.spawn(move || {
        let out = f();          // decode / hash / embed — on a CPU worker
        let _ = tx.send(out);   // drop = caller was cancelled; harmless
    });
    rx.await.map_err(|_| Cancelled)
}
```

- **rayon is a dedicated `ThreadPool`, not the global one**, so its width is explicit and it cannot be enlarged implicitly by a library. Default `N_cpu = max(1, cores - 1)` (one core reserved for the async/UI side); overridable via config ([15](15-observability-config-testing-packaging.md)).
- **`spawn_blocking` is reserved for blocking *I/O* that has no async form** (e.g. a synchronous SMB call, a `rusqlite` statement) — short waits, not CPU burn. CPU burn always goes to the rayon pool via `cpu(..)` above, because tokio's blocking pool is unbounded and would let a decode storm spawn thousands of threads. The rule: **`spawn_blocking` for *waiting*, rayon for *computing*.**
- **The GPU is its own lane.** Headless render ([06](06-3d-render.md)) is neither tokio nor rayon work; it is submitted to a small render queue (size 1..N per device) so GPU submissions serialise correctly. A stage that needs a 3D thumbnail hands off to that queue the same way `cpu(..)` hands off to rayon.

---

## 3. Bounded worker pools & backpressure

The core anti-pattern at scale is *discovery outrunning processing*: a scan enumerates a million paths in seconds, but decode+embed takes far longer, so an unbounded queue between them buffers the whole library in RAM and exhausts memory (and, if each queued item holds an open handle, file descriptors). The fix is **bounded channels between every stage** — a full downstream channel blocks the upstream producer, which is backpressure.

### 3.1 The pool primitive

Every stage that fans work across a pool uses one shared primitive: a bounded input channel, a fixed number of workers draining it, and a bounded output channel.

```rust
pub struct Stage<I, O> {
    input:   mpsc::Sender<I>,      // bounded: cap = width * depth_factor
    output:  mpsc::Receiver<O>,    // bounded: downstream applies backpressure here
    workers: usize,                // fixed width for this stage's resource class
    cancel:  CancellationToken,    // §5
}

// A stage worker: pull, run the (domain-owned) body, push. Fail-soft per item (§6).
async fn run_worker<I, O>(rx, tx, body, cancel) {
    while let Some(item) = rx.recv_or_cancel(&cancel).await {
        // permit acquisition IS the backpressure: capped concurrency per resource class
        let out = match body(item).await {           // body = decode / embed / convert ...
            Ok(o)  => StageOut::Ok(o),
            Err(e) => StageOut::Soft(ItemWarning::from(e)),  // one bad item, not a crash
        };
        if tx.send(out).await.is_err() { break; }    // downstream gone / cancelled
    }
}
```

`recv_or_cancel` returns `None` on either channel-closed or token-cancel, so a worker loop exits promptly on cancellation (§5).

### 3.2 Resource-class widths (not one global "worker count")

Different work exhausts different resources, so each class has its own width and its own bounded queue. A single global thread count would either starve the GPU or oversubscribe network handles.

| Resource class | Bound governs | Default width | Owner of the body |
|----------------|---------------|---------------|-------------------|
| **Source I/O** (open + read) | open FDs, socket count | per-source cap (e.g. FS = `N_io`; SFTP/SMB = small, e.g. 4–8 per host) | [07](07-sources-and-federation.md) |
| **CPU decode/feature** | cores, transient decode buffers | `N_cpu` (rayon) | [04](04-media-handlers.md) / [05](05-analysis-similarity-dedup.md) |
| **Embedding inference** | model RAM / accelerator | small, model-dependent (may be 1–2) | [05](05-analysis-similarity-dedup.md) |
| **GPU render** | one device, VRAM | render-queue depth (1..N) | [06](06-3d-render.md) |
| **DB write** | one SQLite writer (WAL) | **1 writer**, batched (§4.3); readers pooled | [02](02-data-model-and-storage.md) |
| **Convert transcode** | cores + output FDs | `N_cpu`, dry-run bypasses | [08](08-convert-pipeline.md) |

Federated fan-out ([07](07-sources-and-federation.md)) is bounded per-peer and does not consume the local CPU/GPU pools (a peer returns catalog rows, not bytes to process) — its backpressure is peer timeouts, surfaced as partials ([03](03-library-service-and-api.md) §5).

### 3.3 Memory as a first-class bound

Channel depth alone bounds *item count*, not *bytes* — one 8K texture or a dense mesh dwarfs a thousand small SFX. So the expensive lane additionally holds a **byte-budget semaphore**: a decode acquires permits proportional to the decoded size it is about to allocate, and releases them when the buffer is freed. This caps *in-flight decoded bytes* independently of item count, so a burst of huge assets throttles itself instead of OOMing. The budget is a config knob ([15](15-observability-config-testing-packaging.md)); its default is a fraction of available RAM, not a fixed number.

### 3.4 Host-resource governance (the good-neighbour layer)

Bounding our own pools is not enough on a **shared host**: sizing "cores − 2" from the machine's core count still assumes the whole machine is ours, and no thread bound prevents a whole-library pass from consuming all *memory* and swapping the box to death while co-tenant services (a media server, CI runners) starve. Background work — the hosted-mode pipeline's analysis + thumbnail pre-render — is opportunistic by definition, so it defers to everything else. Implemented in `dam-core/src/resources.rs`:

- **Effective budget, not machine size.** The background pool is sized from the *effective* CPU count — `available_parallelism` clamped by the minimum visible leaf/ancestor cgroup v2 quota (or v1 fallback) when containerised — minus the two interactive-headroom cores, and hard-capped at **4 threads** by default. Operators raise it explicitly (`[resources] background_threads`, or `3DAM_BG_THREADS`); an override still clamps to the effective budget.
- **Deprioritised workers.** On Linux every background worker runs at nice +10 and the idle I/O scheduling class, so any co-tenant workload (and any interactive 3DAM read) preempts the grind for both CPU and disk.
- **The pressure governor.** Between work items, background loops consult a cached (250 ms) sample of host state and **pause** while the host is under pressure: available memory (host `MemAvailable`, or cgroup headroom, whichever is smaller) below a floor — default 10% of the memory ceiling, clamped to [256 MiB, 2 GiB], configurable via `[resources] min_free_memory_mb` / `3DAM_MIN_FREE_MEMORY_MB` — or 1-minute load beyond 1.5× the CPU budget, or I/O stall beyond the ceiling below. Work resumes when pressure clears; job cancellation still exits promptly. Transitions are logged so an operator can see the engine yielding.
- **The I/O-stall signal (slow/bulk disk reads).** Memory and load are blind to a *disk* blockade: bulk reads (scan hashing, decode for analysis/thumbnail work) on a slow HDD queue behind each other until every task touching that disk — SQLite, health probes, co-tenants — blocks in `D` state, yet a handful of stalled workers never push loadavg past `1.5 × cpus`, and the idle I/O class only yields under the BFQ scheduler. So the governor also samples PSI (`/proc/pressure/io` and the cgroup's `io.pressure`, kernel ≥ 4.20, worse of the two): it pauses while the recent **`full total` counter-delta** stall share or **`full avg10`** stall share — the fraction of the last 10 s in which *every* non-idle task was blocked on I/O — exceeds a ceiling, default **25%** (`[resources] max_io_stall_pct` / `3DAM_MAX_IO_STALL_PCT`; ≥ 100 disables the gate). `full`, deliberately not `some`: one stalled task is normal life on any busy disk. The `avg10` window is the hysteresis — a saturated disk duty-cycles the grind into short read bursts with recovery gaps instead of flapping.
- **Per-resource disk admission.** Source materialisation and hashing yield in 256 KiB chunks; cache inventory checks between metadata operations. Hardware classes, shared-device permits, queue/latency feedback, operator mount overrides, an independent interactive generation worker, and admin diagnostics are specified in [Shared storage policy](../shared-storage.md). Background warm probes use metadata instead of rereading DMSH payloads.
- **The scan's byte phase is governed too.** The scan job (fetch + BLAKE3 hash of every new/changed file) is the single biggest bulk reader, so it paces against the same governor — after the delta unchanged-file short-circuit, so a paused scan still sweeps cheap metadata-only entries and only the byte reads wait.
- **Fail-soft probes.** All probes are best-effort reads of `/proc` and `/sys/fs/cgroup`; on platforms without them (or kernels without PSI) every pressure probe returns `None`; conservative unknown-storage byte/concurrency budgets still apply.

The interactive lane (inspector reads, queries, the HTTP surface) is *not* governed — the point is precisely that user-facing latency survives background pressure, and the paused pipeline keeps the host healthy enough for the health probes to answer.

---

## 4. The incremental, non-blocking pipeline

Ingest is a **staged pipeline** whose early, cheap stages complete fast and publish results immediately, while expensive stages run behind them and backfill. This is the mechanism behind "a partial index is usable immediately" ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1) and behind the live grid during a scan ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §6.8).

### 4.1 The stages

```
 (07)          (04 CHEAP)        (02)           (04/06 EXPENSIVE)     (05)
┌──────┐   ┌───────────────┐  ┌───────┐   ┌──────────────────┐  ┌──────────────┐
│ scan │──►│ cheap-meta    │─►│ store │─►│ decode           │─►│ thumbnail    │
│ walk │   │ detect +      │  │ upsert│  │ (on demand /     │  │ + features / │
│ +stat│   │ extract_meta  │  │ + emit│  │  deferred)       │  │  embed + ANN │
└──────┘   │ + content hash│  │ event │  └──────────────────┘  │  upsert      │
           └───────────────┘  └───┬───┘                        └──────┬───────┘
 fast, bounded prefix reads only  │  grid is live HERE                │  backfills
 (no full decode, no GPU)         ▼  (AssetAdded events, 03 §7)       ▼  behind it
```

- **Stage 1 — scan/walk** ([07](07-sources-and-federation.md)): enumerate the source, `stat` each entry, apply include/exclude globs. Uses the `(size, mtime)` gate ([02](02-data-model-and-storage.md) §3.2) to skip unchanged files on re-scan. Emits candidate paths into a bounded channel — this is where backpressure bites first when downstream is slow.
- **Stage 2 — cheap metadata** ([04](04-media-handlers.md) CHEAP tier): `detect` + `extract_metadata` over a bounded byte prefix, plus content hashing. **No full decode, no GPU** — this is why it can run across 1M assets at ingest. Produces a full-enough `AssetSummary` to display.
- **Stage 3 — store** ([02](02-data-model-and-storage.md)): upsert the row (single writer, batched — §4.3) and **emit `AssetAdded`** on the `subscribe` firehose ([03](03-library-service-and-api.md) §7). **The library is browsable at the end of this stage** — everything after is enrichment.
- **Stage 4 — decode** ([04](04-media-handlers.md) EXPENSIVE): deferred; runs when analysis is scheduled or a preview is demanded, gated by the byte-budget semaphore (§3.3).
- **Stage 5 — thumbnail / features / embed** ([05](05-analysis-similarity-dedup.md), [06](06-3d-render.md)): produce derivatives into the blob cache ([02](02-data-model-and-storage.md) §8), compute embeddings, upsert the vector index. Each completion emits an update event so the grid swaps placeholders for real thumbnails and the inspector fills in features **without any reload**.

The split at store↔decode is exactly the [04](04-media-handlers.md) §4 cheap/expensive line: the cheap tier is *inline* in the fast path; the expensive tier is a *deferred* stage. This file only schedules them; the tier definitions are 04's.

### 4.2 Delivery: stages feed the trait's streams

A stage's output channel is drained onto the `LibraryService` delivery types ([03](03-library-service-and-api.md) §6–§7) — the same in-memory channel that, in connected mode, the server ([09](09-server-and-web-client.md)) forwards over the WebSocket:

```rust
// A query that streams: the query engine pushes pages into a bounded channel as
// rows are produced; the caller consumes a PageStream. Backpressure flows from a
// slow consumer back into the engine, so we never produce faster than we deliver.
fn query_stream(req) -> PageStream<AssetSummary> {
    let (tx, rx) = mpsc::channel(PAGE_QUEUE_DEPTH);   // bounded
    spawn(async move {
        for page in engine.pages(req, cancel.clone()) {   // cursor-based (03 §6.1)
            if tx.send(Ok(page)).await.is_err() { break; } // consumer dropped → stop
        }
    });
    ReceiverStream::new(rx)   // = PageStream (03 §6)
}
```

The same pattern backs `watch_job` and `subscribe` ([03](03-library-service-and-api.md) §7): progress and change events are pushed into bounded per-subscriber channels. If a subscriber lags past its buffer, the server drops it to the resume cursor rather than growing memory unboundedly — the **coalesce-and-resume** policy: progress events are coalesced (only the latest `Progress` matters), and the client catches up via the `since` cursor ([03](03-library-service-and-api.md) §7). Wire specifics are [09](09-server-and-web-client.md).

### 4.3 The single DB writer — **shipped** (issue #137)

SQLite in WAL mode has one writer. `dam-store` owns that shape directly in `src/db.rs`: **one read-write connection** behind a mutex, a **bounded pool of read-only connections**, and a `maint` `RwLock` that lets whole-file maintenance drain both. Three accessors, three contracts:

| accessor | holds | for |
|---|---|---|
| `Store::read` | `maint.read()` + one pooled read-only connection inside `BEGIN DEFERRED` | every query |
| `Store::write` | `maint.read()` + the writer mutex | anything that mutates rows |
| `Store::exclusive` | `maint.write()` + the writer mutex | `VACUUM`, `wal_checkpoint(TRUNCATE)`, migrations |

Lock order is always `maint` → connection, so `write` and `exclusive` cannot deadlock against each other. Readers are never blocked by the writer — that is the WAL guarantee the old single-`Mutex<Connection>` store threw away, and `tests/concurrency.rs` asserts it as *progress* (readers complete many queries during a sustained write burst), never as a latency threshold.

Four properties are load-bearing:

- **Reads pin one snapshot.** `BEGIN DEFERRED` takes no lock until the first statement, so it never blocks the writer, but it fixes the WAL snapshot for the guard's whole lifetime. A multi-statement read — `stats()` reads `library_stat`, `media_stat`, and `source_stat` in three statements — is therefore internally consistent instead of stitched from three moments.
- **Writes are transactional and `Immediate`.** Because readers now run concurrently, every multi-statement write must be one transaction or a reader will observe a half-applied edit (an asset whose `media_type` no longer matches its `*_attr` row). And every such transaction takes `BEGIN IMMEDIATE`, never rusqlite's default deferred `BEGIN`: a deferred transaction that reads before it writes fails its lock upgrade with `SQLITE_BUSY_SNAPSHOT`, for which SQLite does **not** invoke the busy handler, so `busy_timeout` cannot rescue it. In-process the writer mutex already serialises us; a second process on the same data dir (the desktop shell plus a CLI run) has nothing but this.
- **`PRAGMA journal_mode` is the other place `busy_timeout` does not reach.** Entering WAL rewrites the database header and needs a brief exclusive lock, and SQLite refuses it with a bare `SQLITE_BUSY` without calling the busy handler. Two processes opening the *same fresh* data dir at the same instant therefore raced, and the loser's `Store::open` failed outright with "database is locked". `db::enter_wal` hand-rolls the wait the handler would have done, bounded by the same `BUSY_TIMEOUT`. Only the first open of a fresh (or legacy rollback-journal) file can contend: on an already-WAL file the pragma is a lock-free no-op.
- **CPU work never runs on a connection.** A pinned snapshot means an ever-growing WAL, so anything expensive against copied data — near-duplicate union-find, brute-force cosine scans, HNSW index construction — fetches its rows under a guard, drops it, and computes with none held. Those methods therefore observe more than one snapshot; they were always a best-effort view of a moving catalog, and the affected doc comments say so.
- **Pool size.** CPU count clamped to 2–4, overridable verbatim with **`3DAM_DB_READERS`** (mirroring `3DAM_BG_THREADS` in `dam-core`'s resource governor). Connections open lazily, so a freshly opened store has none until something reads. An in-memory store cannot be pooled at all — a second `:memory:` handle is a different database — so it aliases `read` and `write` onto one connection, and a debug-only per-thread guard-depth counter turns the resulting nested-guard hang into an immediate panic.

**Writes are batched, without a writer channel.** The size/time-bounded batching sketched here shipped in issue #138; the bounded writer *channel* deliberately did not. `Store::apply_scan_batch` and `Store::apply_analysis_batch` take one `Immediate` transaction per batch and one **savepoint per item**, so a failed item rolls back its own trigger effects — the V26 aggregates and the V20 folder hierarchy — alongside its base rows, and its batch-mates still commit. Scan flushes at 128 rows or 200 ms; analysis at 64 rows, 8 MiB of payload, or 250 ms. The row bound is a *writer-lock-hold* bound rather than a memory bound: the batch is one transaction on the single write connection, so its length is how long every other writer waits, `repair_aggregates` included. The byte bound exists because `document_text` reaches ~1 MiB per asset, so a row-only bound would let 64 pending documents pin ~64 MiB.

There is no writer task and no channel. Scan's writer stage is inline on the walk thread, so exactly one chunk is resident and the walk simply stops while a chunk flushes. In analysis, the rayon worker whose push crosses a bound takes the pending vec, drops the lock, and flushes on its own thread; another worker that fills the fresh vec blocks on the writer guard, and that blocking *is* the backpressure — no thread whose shutdown must interlock with cancellation.

---

## 5. Cancellation & resumability

### 5.1 Cancellation — one token tree per job

Every job carries a `tokio_util::sync::CancellationToken`. It is threaded into every stage worker and every child future/rayon closure spawned for that job; cancelling the root cancels the whole tree.

```rust
struct JobHandle {
    id:     JobId,
    cancel: CancellationToken,   // cancel() → all stages, all in-flight items stop
    state:  watch::Receiver<JobState>,  // Queued|Running|Paused|Done|Failed|Cancelled (03)
}

// cancel_job (03 §2) just fires the token; workers observe it at their next await point.
async fn cancel_job(&self, id) -> Result<(), LibError> {
    self.jobs.get(id).ok_or(NotFound)?.cancel.cancel();
    Ok(())   // returns immediately; teardown is async and prompt
}
```

- **Cooperative & prompt.** Workers check the token at every `await` (via `recv_or_cancel`, §3.1) and between rayon work items, so cancellation lands within one item's worth of latency — not after the batch. A long single item (a huge decode) is the granularity floor; it is not preemptively killed, but no *new* work starts.
- **In-flight items are abandoned cleanly, but computed items are kept.** A cancelled decode's `oneshot` result is simply dropped (§2.2) — no derivative is written and no partial item is committed, because each item is its own savepoint. What *has* already been computed is flushed and committed rather than rolled back: those rows were legitimately ingested, both jobs are idempotent and re-derive on the next pass, and discarding them would make the `AssetAdded` events already published to clients lie. Scan then reports `Cancelled` without running `finish_source_scan`, so a cancelled walk is never treated as authoritative and nothing is inferred missing; an analysis item that never flushed simply never advances `analysis_version` and is re-planned. Non-destructive guarantee holds: nothing half-written leaks ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.3).
- **Convert respects the output policy** ([08](08-convert-pipeline.md)): a cancelled convert leaves no partial output file at the destination (write-to-temp, atomic rename on success only).
- **Cancel maps to `LibError::Cancelled`** on any stream that was feeding the job ([03](03-library-service-and-api.md) §5, HTTP 499).

### 5.2 Resumability — the job row is the checkpoint

Long jobs must survive a restart ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §6.1). Resumability is built on the persisted `job` row ([02](02-data-model-and-storage.md) §3.6) plus the fact that **completed work is already durable in the catalog**:

- **Scan** resumes by re-deriving the frontier: on restart, a `job` row in `state='running'` (or `'paused'`) is re-entered, and the scan re-walks — but the `(size, mtime)` gate ([02](02-data-model-and-storage.md) §3.2) makes already-ingested files near-free to skip, so resume is *cheap*, not a full redo. A `params`-carried cursor (last directory / last path enumerated) skips the completed prefix where the source enumerates deterministically. No separate per-file progress table is needed — the catalog *is* the progress.
- **Analyze** resumes from the query "assets with `analysis_version < current` OR `analysed_at IS NULL`" ([02](02-data-model-and-storage.md) §8.2) — the set of not-yet-analysed items is derivable from the catalog, so an interrupted analysis simply re-selects the remainder. Idempotent: re-running produces the same derivatives (versioned, [DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §6).
- **Convert** resumes from its `job.params` manifest: outputs already written (present at destination + recorded in the job result) are skipped; the remainder re-run. Dry-run never touches disk, so it is trivially re-runnable.
- **On clean shutdown**, running jobs are set to `paused` and their cursor flushed; on startup, `paused`/`running` jobs are offered for resume (auto-resume for server mode; prompt/`--resume` for CLI — [13](13-cli.md)). A job whose source is now offline is left `paused` and surfaced as such, not failed ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §2).

The invariant that makes all three cheap: **every stage is idempotent and keyed by content**, so "resume" is "recompute the not-yet-done set and run it," never "replay a log."

---

## 6. Fail-soft as a pipeline mechanism

Fail-soft is not a `try/catch` sprinkled per handler — it is a **structural property of the stage loop** ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §2, [PRODUCT_SPEC](../PRODUCT_SPEC.md) §8). A stage body returns `Result`; an `Err` becomes a **soft per-item warning** carried alongside successful output, never a panic and never a stage abort (§3.1). The distinction is fixed by [03](03-library-service-and-api.md) §5: a hard `Result::Err` sinks the *whole call*; a per-item failure is a `warning`/`partial` inside a *successful* result.

```rust
// Domain bodies MUST NOT panic on bad input; they return HandlerError (04) etc.
// The stage converts a per-item Err into a captured warning and moves on.
match handler.extract_metadata(&mut input, &fmt) {
    Ok(meta) => emit_asset(meta),
    Err(e)   => {
        capture(ItemWarning { asset: path, stage: "cheap-meta", cause: e.to_string() });
        counters.soft_failures.inc();     // (15) — visible, not silent
        continue;                          // pipeline unbroken
    }
}
```

- **Per-item isolation.** A corrupt glTF, a truncated WAV, a codec 04 doesn't support: captured against *that* asset (which still gets a catalog row with an "analysis failed" marker so it is visible and searchable), pipeline continues. This is why a decode runs inside a `catch_unwind` boundary at the rayon handoff — a panic in a third-party decoder ([04](04-media-handlers.md)) degrades one item, it does not poison the pool.
- **Offline source isolation.** A source going offline mid-scan marks the source offline ([02](02-data-model-and-storage.md) §3.5) and pauses *its* stage; other sources' pipelines are unaffected. Cached metadata/derivatives for the offline source stay usable ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §6.1).
- **Warnings are first-class output**, surfaced on `JobEvent::Warning` and aggregated in `JobResult` ([03](03-library-service-and-api.md) §7), and counted for observability ([15](15-observability-config-testing-packaging.md)) — never swallowed.
- **The GPU-absent case** ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §6.8): if render is unavailable, the thumbnail stage degrades (software raster, or skip-and-serve-metadata) per [06](06-3d-render.md) — a stage-level degrade, same mechanism.

---

## 7. Out-of-core at 1M+ assets

The design target is **1M+ assets and datasets that don't fit in RAM** ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1). The rule: **working set ∝ viewport + cache budget, never ∝ library size.** This file owns the *data-access* side of that; the grid virtualisation that consumes it is [12](12-desktop-gui.md)'s UI concern.

- **Paged reads only.** No `LibraryService` call returns "the whole library." `query` is cursor-paginated ([03](03-library-service-and-api.md) §6.1); the data access underneath is a keyset (seek) query — `WHERE (sort_key, id) > (cursor)` `ORDER BY … LIMIT page` — so page *N* costs the same as page 1 regardless of library size. Offset pagination is banned (it degrades linearly). The engine holds at most a small window of pages, matched to the viewport plus a look-ahead margin the grid requests.
- **Derivatives are lazy and cache-bounded.** Thumbnails/waveforms/embeddings load on demand from the blob cache ([02](02-data-model-and-storage.md) §8), keyed by content hash, into an **LRU with a byte budget**. Scrolling past an asset evicts its decoded thumbnail; the on-disk derivative stays. So a million-asset grid holds a few hundred live thumbnails, not a million.
- **The vector index is a persisted, loaded sidecar.** The versioned `usearch` HNSW base is
  checksummed and deserialized once by the
  lifecycle worker; the checked-in 100k/1M benchmark records build time and resident memory so this
  deliberate cost remains visible. ANN returns bounded candidate ids that are exact-reranked and
  then facet-filtered/joined in SQLite ([02](02-data-model-and-storage.md) §7), so steady-state
  similarity is *candidate set → page*, never *load all canonical vectors per request*.
- **Scan streams, never accumulates.** Stage 1 emits paths into a bounded channel (§3) and never materialises the full file list; enumeration state is a cursor, not a `Vec<Path>` of a million entries.
- **Counts are estimated, not scanned.** Facet counts and `library_stats` come from maintained aggregates/indexed counts ([02](02-data-model-and-storage.md)), so the filter chips ([03](03-library-service-and-api.md) §2) don't table-scan a million rows on every keystroke.

The net effect: apart from the deliberately loaded ANN base (whose 100k/1M RSS is recorded by the
scale harness), RSS is a function of the byte budget (§3.3) + LRU cache budget — bounded and
configured rather than an accidental second copy of the catalog's vectors.

---

## 8. Performance targets & benchmarking

The numbers are [PRODUCT_SPEC](../PRODUCT_SPEC.md) §8; this file maps each to a measurable mechanism and a guard. The harness, fixtures, and CI gates are [15](15-observability-config-testing-packaging.md) — here we fix *what is measured and against what budget*.

| Target ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §8) | Concrete budget | Measured as | Guarded by (→ [15](15-observability-config-testing-packaging.md)) |
|-----------------------------------------------|-----------------|-------------|------------------------|
| **60 fps browse @ 100k+** | frame ≤ 16.6 ms; zero blocking I/O on UI thread | frame-time histogram scrolling a 100k / 1M fixture | perf test asserts p99 frame < 16.6 ms; asserts no sync DB/decode call on UI thread |
| **Instant search** | keystroke→first page ≤ ~50 ms local | latency of `query` first page on a 1M fixture | benchmark asserts p95 first-page latency budget |
| **Analysis saturates cores, UI unblocked** | rayon pool ≥ ~90% core utilisation under load; UI frames unaffected | `analysis_rayon_core_utilization_pct` worker busy-time + `browse_under_analyze_ms` p95 during the same fixture analysis pass | scale harness runs analyze against the profile fixture while sampling production browse and named Rayon-worker CPU time |
| **1M assets / out-of-core** | RSS bounded by budgets (§7), not asset count | RSS vs library size curve; page-N latency flat | assert RSS ceiling and O(1) page latency across sizes |
| **Fail-soft** | 0 crashes on a corrupt-file corpus; every bad item captured | soft-failure count vs injected-corruption count | fault-injection fixture (truncated/garbage files) must complete with 0 panics |
| **Cancel/resume** | cancel lands ≤ 1 item latency; resume skips done work | time-to-quiescent after cancel; resume re-work ratio | test kills mid-scan/analyze, asserts prompt stop and cheap resume |
| **ANN at 1M × 512** | ≤153 s build, ≥99% product candidate recall, ≤~2 GB graph resident; interactive lookup | `ann_scale` build/load/80-candidate search/CPU rerank/RSS lifecycle | #141's final `usearch` run: 129.8 s build, 8.817 ms ANN lookup, 67 µs CPU rerank, 99.6% candidate recall, 1,264,320 KiB graph RSS delta; `ann` remains off by default |

Principles for the harness (owned by [15](15-observability-config-testing-packaging.md), stated here so the numbers are honest):

- **At-scale, messy fixtures.** Benchmarks run against **synthetic 1M-asset libraries** and a **corrupt/edge-case corpus**, not tidy small sets ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §6). Fixtures are generated/described in [15](15-observability-config-testing-packaging.md).
- **Regression gates in CI.** Each budget above is an assertion; a PR that regresses p99 frame time or page-N latency past a threshold fails. Absolute numbers are hardware-relative, so gates are *ratios against a baseline* plus hard ceilings for the load-bearing ones (frame budget, crash count).
- **Measure before optimising** ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1) — instrumentation (spans, counters for soft-failures, pool saturation, queue depth) is [15](15-observability-config-testing-packaging.md); this layer must *emit* those signals (queue depth, in-flight bytes, per-stage throughput) so they can be watched.

---

## Open questions

> **Partly resolved 2026-07-06 in [ADR 0009 §11](../adr/0009-v1-scope-decisions.md)** — explicit
> `spawn_blocking` cap, no intra-item resume in v1, and a fixed `ubuntu-latest` 4-core CI ratio
> baseline (GPU benches on a self-hosted box). **Still open as calibration** against real hardware:
> the `N_cpu`/`N_io` split and whether the embedding lane gets its own reservation (tied to the
> embedding-model spike), and the byte-budget RAM fraction. These are tuned against the benchmark
> harness ([15](15-observability-config-testing-packaging.md)), not decided by fiat.

- **Runtime sizing defaults.** `N_cpu = cores - 1` and `N_io = min(cores, cap)` are starting points; the right split (and whether the embedding-inference lane deserves its own thread reservation, or should share `N_cpu`) needs a spike on real analysis workloads. Ties to the embedding-model choice ([PRODUCT_SPEC](../PRODUCT_SPEC.md) §10).
- **Byte-budget calibration.** The in-flight-decoded-bytes budget (§3.3) default as "a fraction of available RAM" needs validation across a 512 MB NAS container and a 64 GB workstation — the fraction may need to be absolute-floored and RAM-capped rather than purely proportional.
- **Backpressure vs latency on the firehose.** The coalesce-and-resume drop policy for lagging subscribers (§4.2) trades completeness for bounded memory. Whether progress coalescing is ever lossy in a way a client notices (vs `AssetAdded`, which must not be dropped) needs settling with [09](09-server-and-web-client.md)'s WS design.
- **Resume granularity for a single huge item.** Cancellation's floor is one item; a multi-minute decode/convert of a giant asset can't be interrupted mid-item today. Whether any stage body should support intra-item checkpointing (chunked convert?) is a [08](08-convert-pipeline.md) question this layer would need to expose a cooperative yield for.
- **`spawn_blocking` pool cap.** tokio's blocking pool is unbounded by default; we restrict CPU work off it (§2.2), but blocking *I/O* (SFTP/SMB/`rusqlite`) still uses it — whether that pool needs an explicit cap to bound FDs under a source-storm is open, and interacts with the per-source I/O widths ([07](07-sources-and-federation.md)).
- **Baseline hardware for CI ratios.** Regression gates as ratios need a fixed baseline runner; which reference machine (and how to keep GPU-render benchmarks meaningful on GPU-less CI) is a [15](15-observability-config-testing-packaging.md) decision this file's targets depend on.
