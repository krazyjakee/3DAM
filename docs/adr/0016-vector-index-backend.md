# ADR 0016 — Vector index backend: a `usearch` HNSW sidecar; `sqlite-vec` dropped

Status: **Accepted; amended after #141 tripped the original scale-revisit trigger** · Date:
2026-08-05 · Amended: 2026-08-08 · Deciders: 3DAM core

Related: [the vector-index spike](../../spikes/vector-index/README.md),
[tech-spec 02 §7](../tech-spec/02-data-model-and-storage.md),
[tech-spec 05 §3](../tech-spec/05-analysis-similarity-dedup.md), and issues
[#182](https://github.com/krazyjakee/3DAM/issues/182) and
[#141](https://github.com/krazyjakee/3DAM/issues/141).

## Context

The spike established that an HNSW sidecar is the viable shape at 1M × 512 dimensions. Its
reference `usearch` run built in 153 s and served raw K=10 in 0.48 ms; raising `ef_search` from 64
to 256 moved raw recall from 85% to 100%. `sqlite-vec` 0.1.9 was an exact scan at 726 ms/query,
slower than the existing 68 ms in-process exact cosine scan, so it has no primary-index or rerank
role.

The first version of this ADR selected pure-Rust `instant-distance` to avoid another C++ build.
Issue #141 then measured that choice at the acceptance scale:

| backend, 1M × 512 | build | lookup p50 | recall@10 | sidecar | graph/process memory |
|---|---:|---:|---:|---:|---:|
| `instant-distance` 0.6 | 1,173 s | 1.607 ms raw K=10 | 89.6% | 2.388 GB | 4.419 GiB process RSS |
| spike `usearch` 2.25, f32/M16/ef128/64 | 153 s | 0.48 ms raw K=10 | 85% | ~2.2 GB | ~2.2 GB |

`instant-distance` missed build time, recall, sidecar size, and resident-memory bars by decisive
margins. That was the ADR's explicit replacement trigger, not an optional optimization.

## Decision

Ship a pinned **`usearch = 2.25.3`** HNSW sidecar behind the off-by-default `ann` Cargo feature.
Drop `instant-distance` and continue to exclude `sqlite-vec`.

- Keep one immutable graph per `EmbeddingSpace`; image, audio, and shape vectors never mix.
- Use cosine distance over L2-normalised vectors, f16 graph storage, `M=16`,
  `ef_construction=256`, and `ef_search=2048`.
- For a requested K, retrieve `max(64, K×8)` approximate candidates (plus the bounded durable
  overlay, capped at 8,192), then fetch current f32 vectors from SQLite and exact-rerank them.
  Quantisation therefore affects candidate recall, never final score precision.
- SQLite remains canonical. Schema V27 transactionally records each space's generation and latest
  upsert/tombstone overlay. A background worker snapshots and builds outside interactive database
  critical sections, fsyncs a checksummed/versioned generation sidecar, and atomically publishes it
  while preserving newer deltas.
- A missing, corrupt, wrong-version, wrong-backend, wrong-space, wrong-shape, or native-failed
  sidecar is never published as a ready empty graph. Build errors leave exact fallback active;
  lookup errors evict the process cache and return to exact fallback while the worker reloads or
  rebuilds.
- The feature remains off by default. The exact rayon cosine scan is the no-feature path, recovery
  path, and parity ground truth.

## Scale evidence

The checked-in harness uses deterministic clusters with known exact top-10 truth and measures the
same 80-candidate over-fetch used by a product K=10 query. Final results on a shared Linux host with
31 GiB RAM were:

| vectors × dims | build | ANN lookup p50 | CPU exact rerank p50 | candidate recall@10 | sidecar | graph RSS delta |
|---:|---:|---:|---:|---:|---:|---:|
| 100,000 × 512 | 4,575 ms | 4,800 µs | 59 µs | 1.000 | 119,647,296 B | 130,964 KiB |
| 1,000,000 × 512 | 129,774 ms | 8,817 µs | 67 µs | 0.996 | 1,196,492,412 B | 1,264,320 KiB |

The 1M run used:

```sh
DAM_ANN_BENCH_SIZE=1000000 DAM_ANN_BENCH_DIM=512 DAM_ANN_BENCH_QUERIES=50 \
  cargo bench -p dam-store --features ann-bench --bench ann_scale --locked
```

The 1M JSON reported absolute process RSS of 2,075,664 KiB before build and 3,339,984 KiB with the
graph, hence the 1,264,320 KiB graph delta. Its 5,774,476 KiB synthetic lifecycle peak deliberately
retained the 2 GiB canonical input for rerank measurement while also holding encoded and reloaded
graphs; production drops the build snapshot before publication, so that number is a conservative
harness peak rather than steady-state graph memory. `exact_rerank_cpu_p50_us` measures in-memory ID
lookup, f32 cosine, and sorting for 80 candidates; it excludes SQLite retrieval and metadata
hydration. `ann_lookup_p50_us` is native lookup only. The old spike's 0.48 ms number is raw K=10 at
`ef_search=64`, so it is not directly comparable to this higher-recall product-candidate query.

The evidence-driven tuning path was also recorded: at 1M, product candidate recall was 0.910 with
`ef_search=512`, 0.980 with 1024, and 0.996 with 2048; every other final parameter was held fixed.

## Consequences

- ANN now meets the acceptance scale: build is below 153 s, candidate recall is above 99%, and
  graph resident size is below the ~2 GB comparison bar. An ~9 ms native candidate lookup remains
  comfortably interactive, while exact rerank adds tens of microseconds of CPU work plus database
  retrieval.
- `usearch` adds `cxx`, a C++ compiler/build script, and native release-target work. Release CI must
  continue to compile and run the `ann` feature on every supported target; this cost is accepted
  because the measured pure-Rust backend failed the product scale.
- f16 reduces graph/vector storage, but only because the 8× candidate recall guard passed at both
  acceptance scales. Any change to quantisation, graph degree, construction/search breadth, or
  over-fetch requires rerunning both scales.
- Sidecars written by the former backend are rejected by backend/version metadata and rebuilt from
  SQLite; they are derived data, so this is migration by recovery rather than catalog migration.
- `sqlite-vec` remains rejected: it is an exact linear scan that duplicates vectors and loses to
  the exact Rust fallback on measured latency.

## Revisit triggers

Revisit if a supported release target cannot build the pinned native dependency, if representative
production data falls below 99% candidate recall, if end-to-end SQLite retrieval plus rerank exceeds
the interactive budget, or if memory-constrained deployments require a smaller graph. Compare any
replacement against the reproducible 100k and 1M product-candidate harness, not raw K=10 alone.
