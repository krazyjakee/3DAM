# Spike — vector index at scale: sqlite-vec vs usearch

Status: **Decided** · Date: 2026-07-06 · Settles the open question in
[tech-spec 02 §vector-index storage](../../docs/tech-spec/02-data-model-and-storage.md) and
[tech-spec 05 similarity](../../docs/tech-spec/05-analysis-similarity-dedup.md) / PRODUCT_SPEC §10.

> **Superseded in part by [ADR 0016](../../docs/adr/0016-vector-index-backend.md) (2026-08-05,
> scale-amended 2026-08-08).**
> The *shape* this spike settled stands: the primary similarity index is a **sidecar HNSW**, not an
> exact scan inside SQLite. The *crates* named in the verdict below do not. 3DAM ships
> pinned **`usearch` 2.25.3** after #141 measured the pure-Rust replacement missing the 1M build,
> recall, sidecar, and memory bars — and **`sqlite-vec` is dropped entirely**, including
> the exact-re-rank role reserved for it here: the in-process rayon exact scan measured **68 ms/q**
> below is ~10× faster than sqlite-vec's own 726 ms at the same 100% recall, over vectors already in
> `library.db`. The checked-in product-candidate harness now qualifies `usearch` at 1M; read the
> original numbers below as evidence and ADR 0016 as the current call.

## Question

3DAM designs for **1M+ assets** with **instant similarity search** (PRODUCT_SPEC §8). The
similarity index can either be **embedded in SQLite** (`sqlite-vec`, one extension, same DB
file) or a **standalone/sidecar ANN** (`usearch` — HNSW). Which, and what does each cost in
build time, query latency, recall, and disk at scale?

## Method

[`src/main.rs`](src/main.rs) — `usearch 2.25` (HNSW, M=16, ef_construction=128, tunable
ef_search) vs `sqlite-vec 0.1.9` (`vec0`, cosine). Synthetic **clustered** 512-d unit vectors
(2000 centroids + noise) — realistic for embeddings, which live on a manifold; uniform-random
vectors are the pathological worst case for ANN and give meaningless recall (an early bug here:
9% recall on random data → 98%+ on clustered). Ground truth is an exact rayon brute-force scan;
recall@10 = overlap of each engine's top-10 with it.

```
cargo run --release -- [N] [DIM] [Q] [ef_search]   # default 1_000_000 512 200 64
```

## Results (dev box: 32-thread, release)

**1,000,000 × 512-d, K=10:**

| engine | build | query p50 | query p99 | recall@10 | disk |
|--------|-------|-----------|-----------|-----------|------|
| **usearch** (HNSW, ef=64) | 153 s | **0.48 ms** | 1.24 ms | 85 %* | 2196 MB (in-mem) |
| **sqlite-vec** (vec0, exact) | 15.6 s | **726 ms** | 760 ms | 100 % | 2076 MB (on-disk) |
| brute-force baseline | — | 68 ms/q** | — | 100 % | — |

\* recall is `ef_search`-tunable — see sweep below. \*\* rayon-parallel across all cores.

**usearch recall/latency knob (200k, varying ef_search):**

| ef_search | query p50 | recall@10 |
|-----------|-----------|-----------|
| 64 | 0.30 ms | 97.7 % |
| 128 | 0.39 ms | 99.5 % |
| 256 | 0.48 ms | **100 %** |

## Findings

1. **sqlite-vec `vec0` is an exact linear scan** (no ANN graph in 0.1.x). 100% recall, trivial
   to embed, transactional with the catalog, fast to build (64k rows/s) — but **O(N) queries:
   726 ms at 1M**. That is ~1500× slower than usearch and far past any "instant" budget.
2. **usearch (HNSW) hits the 1M target**: **sub-millisecond** queries, recall **tunable to
   ~100%** via `ef_search` at negligible latency cost. Price: a **separate ~2 GB in-memory
   index**, a slower background build (~150 s/1M; incremental in practice, not a rebuild), and
   approximate-by-nature results.
3. **Disk ≈ 2 GB either way** at 1M×512 f32 — dominated by raw vector bytes, not structure.
   usearch scalar quantization (f16/i8) would cut memory/disk ~2–4× for a small recall cost
   (untested — follow-up).

## Verdict

**Primary similarity index = a sidecar HNSW (`usearch` 2.25.3,
[ADR 0016](../../docs/adr/0016-vector-index-backend.md)), not sqlite-vec**, for the
1M-asset target. Tech-spec 02/05 should record:

- **HNSW sidecar** is the index for "find similar" at scale — sub-ms, recall-tunable. It is a
  derived, rebuildable artefact in the managed store (02's blob/index area), keyed by
  `EmbeddingSpace` (one index per embedding space), built incrementally as assets are analysed.
- ~~**sqlite-vec still earns a place** as (a) the zero-infra path for **small libraries**
  (<~100k, where ~50–70 ms exact is fine and 100% recall + one-file simplicity wins) and/or
  (b) an **exact re-rank** stage over an ANN candidate set. Keeping vectors in SQLite *and* the
  HNSW sidecar is cheap (vectors are ~the same bytes) and gives an exact fallback.~~
  **Not taken** ([ADR 0016](../../docs/adr/0016-vector-index-backend.md)): both roles are already
  filled by the in-process rayon exact scan over the `embedding` table — 68 ms/q at 1M here, ~10×
  faster than sqlite-vec's own exact scan, no extension, no second copy of the vectors.

## Caveats / follow-ups

- **Synthetic data.** Clustered random ≠ real CLIP/audio/shape embeddings; absolute recall will
  shift with the true manifold, though the tunability and latency story holds.
- **Quantization untested** — f16/i8 in usearch (memory/disk/latency vs recall) is the obvious
  next measurement; matters for the "out-of-core at 1M+" goal.
- **In-memory footprint.** 2 GB for 1M×512 f32 resident — fine on a workstation, tight on a
  small NAS; quantization or memory-mapped indexes needed for the low-powered serve host.
- `sqlite-vec` cold-disk (uncached) would be worse than the warm 726 ms measured here.
