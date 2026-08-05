# ADR 0016 — Vector index backend: an `instant-distance` HNSW sidecar; `sqlite-vec` dropped

Status: **Accepted** · Date: 2026-08-05 · Deciders: 3DAM core
Supersedes: the backend half of the [vector-index spike](../../spikes/vector-index/README.md)'s verdict (2026-07-06) as recorded in PRODUCT_SPEC §10 · Related: [ADR 0011](0011-assimp-import-backend.md) (the one native-toolchain dependency we do carry), [ADR 0015](0015-video-decode-backend.md) (the last time we declined a second one), [ADR 0006](0006-inference-runtime-candle.md) (what produces the vectors), [tech-spec 02 §7](../tech-spec/02-data-model-and-storage.md) (storage framing), [tech-spec 05 §3](../tech-spec/05-analysis-similarity-dedup.md) (query semantics), issues [#182](https://github.com/krazyjakee/3DAM/issues/182), [#141](https://github.com/krazyjakee/3DAM/issues/141) (index persistence)

## Context

The [vector-index spike](../../spikes/vector-index/README.md) benchmarked two ways to serve
"find similar" at the 1M-asset target, and the numbers were decisive. At 1M × 512-d, K=10:

| engine | build | query p50 | recall@10 | disk / memory |
|---|---|---|---|---|
| `usearch` (HNSW, ef=64) | 153 s | **0.48 ms** | 85 % (→ 100 % at ef=256) | 2196 MB in-mem |
| `sqlite-vec` 0.1.9 (`vec0`) | 15.6 s | **726 ms** | 100 % | 2076 MB on-disk |
| rayon brute-force baseline | — | **68 ms** | 100 % | — |

PRODUCT_SPEC §10 wrote that up as *"sidecar HNSW (`usearch`) as the primary index, `sqlite-vec`
for small libraries / exact re-rank."*

**The shipped engine does neither.** `crates/3dam-store/src/ann.rs` wraps `instant-distance`'s
HNSW behind the off-by-default `ann` Cargo feature; the default similarity path is the exact
rayon cosine scan in `analysis.rs`, which is also the ground truth the ANN parity test checks
against. Issue #141, which scopes index persistence and incremental maintenance, keeps that
backend. So the spec has named one crate and the tree has shipped another for a full phase, and
every downstream issue inherits the ambiguity. That divergence — not a performance problem — is
what this ADR closes.

It is worth being precise about what the spike settled and what it did not.

- **It settled the *shape*, durably.** `sqlite-vec` 0.1.x's `vec0` is an exact linear scan with no
  ANN graph at all; 726 ms/query at 1M is ~1500× off any "instant" budget. An HNSW graph held
  beside the catalog, not an exact scan inside it, is the only structure that reaches the target.
  Nothing here revises that.
- **It did not price the backend.** `usearch` is the crate the spike benchmarked because it is the
  reference HNSW implementation, not because a build-and-packaging review had been done on it. It
  is a C++ library behind a `cxx` FFI bridge with a cmake build script. We carry exactly one
  dependency of that class today — Assimp ([ADR 0011](0011-assimp-import-backend.md)) — and it is
  the sole reason `.cargo/config.toml` exists at all. [ADR 0015](0015-video-decode-backend.md)
  declined to take a second one for video poster frames. Release CI cross-builds four targets on
  three OSes (#45), and `cargo xtask ci` *runs* dam-store's `ann` tests rather than merely linting
  them; both of those get more expensive the moment the index needs a C++ toolchain.
- **`instant-distance` is the same algorithm without that bill.** One pure-Rust crate, no build
  script, no system library, no cross-compilation story to maintain — it builds wherever `rustc`
  does. Same HNSW, same cosine metric over the same L2-normalised vectors.

## Decision

**Ship an `instant-distance` HNSW sidecar as the vector index. Do not take `usearch`. Drop
`sqlite-vec` entirely.**

1. **The index shape is unchanged and is not up for revision.** An HNSW graph, **one logical index
   per `EmbeddingSpace`** (tech-spec 05 §3.1 — image/audio/shape are never mixed), cosine metric
   over L2-normalised vectors, **derived and rebuildable** from the `embedding` table (schema V3).
   Losing the index is a re-index, never data loss.
2. **The implementation crate is `instant-distance`** (`crates/3dam-store/src/ann.rs`), chosen for
   being one pure-Rust crate with no build script and no C++ toolchain, which `usearch` is not.
3. **It stays behind the off-by-default `ann` Cargo feature.** The exact rayon cosine scan remains
   the default path, the fallback when the feature is off, and the parity ground truth. Golden
   rule 4 applies to capabilities; this one is a build-time accelerator with an unchanged API, and
   the parity test is what lets the store swap it in without changing results.
4. **`sqlite-vec` is dropped, not reserved — including as the exact re-rank stage.** Both roles the
   spike held open for it are already filled, better, by code we ship. The in-process rayon scan
   over `embedding` measured **68 ms/query at 1M** in the same spike run: ~**10× faster than
   `sqlite-vec`'s own exact scan** (726 ms) at identical 100 % recall, over vectors that are
   already in `library.db`, with no loadable extension, no second copy of every vector, and no C
   amalgamation compiled by a build script. A dependency that is dominated on latency, ties on
   recall, and adds build weight has no role left to play. Exact re-rank over an ANN candidate set,
   when we want it, is a dot-product loop over a few hundred vectors — not a database extension.
5. **Vectors' canonical home stays the `embedding` table in `library.db`**, preserving tech-spec 02
   §7's portability invariant (copy one file, get a complete library). `vectors/` holds only
   derived index artefacts.

## Consequences

- **The store's build stays toolchain-free.** Enabling `ann` adds a crate and nothing else: no
  cmake, no C++ compiler, no `.cargo/config.toml` entry, no per-target packaging work across the
  four release legs. It is also why `cargo xtask ci` can afford to *run* the `ann` tests on every
  leg rather than lint them, which is the only way the "swap HNSW in behind an unchanged API"
  claim is actually verified.
- **We accept a coarser recall knob than `usearch`, and no measurement at 1M.** `instant-distance`
  sets `ef_search` on the `Builder`, so the recall/latency trade is fixed **when the index is
  built**, not per query — there is no equivalent of the spike's 64→256 sweep at query time. We
  have no 1M×512 numbers for it at all. This is a real, named gap and #141 owns closing it.
- **We give up scalar quantization.** `usearch`'s f16/i8 modes (the spike's open follow-up, worth
  2–4× on the ~2 GB/1M resident footprint) have no `instant-distance` equivalent. That matters for
  the low-powered serve host, not the workstation, and it is the most likely trigger below.
- **This ADR fixes the backend, not the lifecycle.** Today the index is an in-memory per-space
  cache built lazily and invalidated by a global embedding generation
  (`analysis.rs::ann_for_space`). Persistence, incremental per-space maintenance, atomic swap,
  corruption recovery, and the 100k/1M recall+latency benchmarks are #141's scope, and they now
  have a named backend to build against.
- **The spike's numbers survive as the acceptance bar even though its crate did not.** They are the
  target `instant-distance` has to hit, and the evidence any replacement would have to beat.
- **Revisit if** #141's benchmarks show a **measured miss against those numbers** at 1M×512 — a p50
  materially off **0.48 ms**, a recall that cannot be tuned to ~99 %+ at acceptable latency, or a
  build time / resident footprint materially worse than **153 s / ~2 GB**. Also revisit if
  quantization becomes load-bearing for a memory-constrained host. In either case the trade to
  argue is a *measured* recall/latency/memory win against a cmake + C++ dependency on four
  cross-built targets, and this ADR is the baseline that change has to beat.
- **The decision is cheap to reverse, which is part of why the light option is defensible now.**
  The whole seam is `AnnIndex::build` / `AnnIndex::nearest` in one small module behind one feature
  flag, with a parity test against the exact scan already guarding the contract. Swapping the graph
  implementation later is a module-local change, not an architectural one.

## Alternatives considered

- **`usearch` (the spike's named winner)** — fastest measured by a wide margin, per-query
  `ef_search` tuning, f16/i8 quantization, mmap/out-of-core. Rejected for v1 on **build weight
  alone**: a C++ library over a `cxx` bridge with a cmake build script is a second dependency of
  Assimp's class, paid on every cross-built release target and every CI leg, to buy headroom we
  have not yet measured ourselves to need. Nothing about the *quality* of the option is disputed —
  reconsider it under the triggers above, with numbers.
- **`sqlite-vec`** — one extension, vectors transactional with the catalog, facet joins expressed
  in plain SQL, 100 % recall, and a "just copy one file" story. Dropped, for two independent
  reasons: `vec0` in 0.1.x is an exact linear scan with no ANN graph, so it never was a candidate
  for the primary index; and at 1M it is ~10× slower than the exact scan we already run in-process,
  so it loses even the small-library and exact-re-rank roles it was reserved for. It is also a C
  amalgamation built by a build script and needs `load_extension` enabled on an otherwise
  `bundled` rusqlite — not the free option it looks like.
- **Exact cosine scan only, no ANN at all** — this is what the default build ships today, and at v1
  scale it is honest: exact, rayon-parallel, 68 ms/query at 1M, zero extra dependencies. Rejected
  as the *ceiling* because that cost is per query and does not compose with the interactive dedup
  and hybrid-search paths that issue several, and because it scales with core count rather than
  with the data structure. Kept as the default and the fallback rather than discarded.
- **Deferring the pick again** — rejected on principle. The spec-versus-tree divergence is itself
  the cost being paid, #141 cannot persist an index without a named backend, and "we benchmarked
  `usearch` once" is not a decision record.
