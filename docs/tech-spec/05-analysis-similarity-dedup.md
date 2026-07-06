# 05 — Analysis, Similarity & Deduplication

Status: **Draft v0.1** · Scope: the analysis orchestration that turns extractor output into suggestions, embeddings + on-device inference, the similarity/ANN query, exact + near-duplicate detection, the tileability metric, and extractor versioning/re-analysis.

This file owns the *automation brain* of 3DAM: the pipeline that runs after a
[`MediaHandler`](04-media-handlers.md) produces features/embeddings, and everything that
consumes them — auto-tagging, auto-categorisation, dedup, the similarity index, and
tileability. It implements the differentiator described in
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.2 (analysis & automation), §6.3 (similarity,
duplicate review), and §5 (per-media features + the tileability definition), under the
automation rules of [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §1.2
(reviewable/reversible/explainable) and §6 (reproducible/versioned analysis).

**Border discipline.** This file draws hard lines against its neighbours:

- [04-media-handlers.md](04-media-handlers.md) owns the decode/metadata contract — the
  `MediaHandler` trait, the cost-tiered `extract_metadata` vs `thumbnail`/`extract_features`
  split, and format detection. This file **consumes** `extract_features` output; it does not
  decode files.
- [02-data-model-and-storage.md](02-data-model-and-storage.md) owns *how* vectors,
  perceptual hashes, tags (the suggested-vs-confirmed distinction), and derivatives are
  **stored** — the schema, the vector-index on-disk layout, and the blob cache. This file
  says what to compute and how to query it; it references, not defines, the tables.
- [06-3d-render.md](06-3d-render.md) owns the actual multi-view render (the wgpu
  render-to-image path). This file specifies the *shape-embedding contract* the multi-view
  case depends on, and calls into 06 for pixels.
- [07-sources-and-federation.md](07-sources-and-federation.md) owns query fan-out across
  peers. This file notes where similarity fans out and the cross-peer embedding-compatibility
  constraint, then defers the mechanics.
- [14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md) owns
  the worker-pool primitives (tokio/rayon split, bounded queues, cancellation). This file
  describes the pipeline's *stages* and where work lands; 14 owns the pool.

---

## 1. The analysis pipeline

### 1.1 Where analysis sits

Analysis is a stage in the ingest/scan flow, downstream of the media handlers and upstream of
the suggestion surface. It never runs on the UI thread and never mutates confirmed catalog
state — it **produces suggestions and derived data**, which are stored and surfaced for the
user to accept or reject ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.2).

```
 scan/watch (01)                     off-UI worker pools (14)
   │  new/changed asset
   ▼
 MediaHandler (04)          ┌──────────── analysis pipeline (this file) ───────────┐
   extract_metadata  ─────► │ 1. plan: which extractors are due (versioning §7)     │
   (cheap tier, ingest)     │ 2. extract_features (04) → raw features + embeddings  │
   extract_features ─┐      │ 3. derive: perceptual/content hash, tileability (§5)  │
   thumbnail         │      │ 4. classify → auto-tag / auto-categorise suggestions  │
   (cost tier)       └────► │ 5. index: upsert vectors into ANN (§3)                │
                            │ 6. dedup: exact + near-dupe grouping (§4)             │
                            └──────┬─────────────────────────┬──────────────────────┘
                                   ▼                          ▼
                       suggestions (suggested tags,     vector index + hashes
                       category guess, dupe groups)     (storage: 02)
                                   ▼
                       inspector accept/reject (12) · CLI (13) · API (03)
```

### 1.2 Stages (concrete)

Each stage is a pure-ish function keyed by `(content_hash, extractor_id, extractor_version)`
so it is cacheable and re-runnable (§7). The orchestrator is `AnalysisRunner`, living in
`3dam-core::analysis`.

1. **Plan.** Given an asset row (with `content_hash` and current `analysis_version`), compute
   the set of extractors that are *due*: any whose `(id, version)` is not already recorded as
   completed for this `content_hash`. This is the cache-invalidation gate (§7). If nothing is
   due, the asset is skipped — re-scanning an unchanged file is free.
2. **Extract.** Call the handler's `extract_features` (04) for the due extractors. This is the
   expensive tier: decode, DSP/FFT (audio), colour + perceptual analysis (image), multi-view
   render (3D, via 06). Output is a `FeatureBundle` — scalar features, class logits, and one
   or more embedding vectors, each tagged with the extractor id/version that made it.
3. **Derive.** Compute cheap derived signals that don't need a model: the content hash (if not
   already present from scan), perceptual hashes for images (§4.2), and the **tileability
   metric** (§6) for images classified as textures. These are deterministic and versioned like
   any extractor.
4. **Classify → suggest.** Turn features/logits into *suggestions*: auto-tags (e.g.
   `metallic`, `seamless`, `loop`) and an auto-category guess (e.g. image→texture,
   audio→one-shot vs loop, 3D→prop). Each suggestion carries a **confidence** and an
   **explanation** payload (which features/logits drove it) so the inspector can answer "why?"
   (DESIGN_GUIDELINES §1.2). Suggestions are written as *suggested* tags/attributes, never
   *confirmed* — the distinction is the schema's, defined in
   [02-data-model-and-storage.md](02-data-model-and-storage.md).
5. **Index.** Upsert each embedding into the ANN index (§3) keyed by asset id, tagged with its
   `EmbeddingSpace` `(model_id, model_version, media, dim, metric)` so cross-space queries can be
   rejected (§3.4, §8).
6. **Dedup.** Feed the content hash into the exact-dup group and the perceptual/embedding
   signals into near-dup grouping (§4). Grouping is incremental: a new asset joins or forms a
   group as it lands.

### 1.3 Incremental, never a batch dump

Per [DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.1/§1.2, stages emit results as they
complete, per asset. The runner pushes progress + partial results over the live-update channel
([03-library-service-and-api.md](03-library-service-and-api.md) WebSocket/stream) so the grid
and inspector update as suggestions arrive. A partial index is queryable immediately: an asset
with metadata but no embedding yet is browsable and text-searchable; it simply doesn't appear
in similarity results until stage 5 completes for it.

### 1.4 Suggestion lifecycle (accept/reject)

- A suggestion is a row in the suggested-tag / suggested-attribute space (schema: 02) with
  `{value, source: extractor_id@version, confidence, explanation}`.
- The inspector (12), CLI (13), and API (03) offer a **one-action accept/reject** per
  suggestion (DESIGN_GUIDELINES §3.4). Accept promotes it to a **confirmed** tag/attribute;
  reject records a negative so re-analysis at the same extractor version does not re-suggest
  it.
- Everything is **reversible**: confirmed values can be un-confirmed; the underlying suggestion
  and its explanation are retained (until re-analysis supersedes them). No stage ever deletes,
  moves, or silently overwrites confirmed user data.
- **Bulk accept/reject** (e.g. "confirm all `seamless` suggestions above 0.9") is a batch over
  the same primitive, previewed before it applies (DESIGN_GUIDELINES §3.4).

---

## 2. Embeddings & the on-device inference runtime

### 2.1 One embedding per media type (v1)

Each media type gets one *primary* embedding space used for similarity and near-dup. All are
L2-normalised float vectors so cosine similarity is a dot product.

| Media | Embedding (v1 pick) | Input | Dim |
|-------|--------------------|-------|-----|
| Image | **SigLIP** base/patch16-224 (candle-native); **DINOv2** ViT-S/14 paired for pure-visual dedup | decoded RGB, resize to model input | **768** (DINOv2 384) |
| Audio | **LAION-CLAP** `clap-htsat-unfused` (via `ort`/ONNX — no candle impl) | mel-spectrogram from the DSP stage | **512** |
| 3D | **multi-view render → SigLIP → mean-pool** (6 canonical views, L2-renorm) | N fixed-pose renders → per-view features → pooled | **768** |

Concrete model selection was the **last open spike** — researched 2026-07-06
([`spikes/embedding-models/`](../../spikes/embedding-models/README.md)); the picks above are the v1
target (dims **provisional** pending on-domain validation). The pipeline is written against the
*contract* (an `Embedder` producing a normalised vector tagged with an embedding-space id), not a
specific checkpoint, so a model swap is a `model_version` bump (§7), not a rewrite. Two findings
carry forward: **audio forces the `ort`/ONNX path** (candle has no CLAP-class model), and **image +
3D share the SigLIP weights but hold distinct `EmbeddingSpace`s** (pooled multi-view vectors must
never cross-rank against single images).

The `Embedder` seam:

```rust
struct EmbeddingSpace { model_id: &'static str, model_version: u32, media: MediaType, dim: usize, metric: Metric }

trait Embedder {
    fn space(&self) -> EmbeddingSpace;        // identifies the vector space (see §3.4, §8)
    fn dim(&self) -> usize;
    fn embed(&self, input: &ModelInput) -> Result<Vec<f32> /* normalised */>;
}
```

### 2.2 Runtime: `candle` vs ONNX Runtime (`ort`)

Both are named in the candidate stack (PRODUCT_SPEC §7). This file records the tradeoff; the
final pick is a spike-gated ADR follow-up, and the pipeline depends only on the `Embedder`
trait, so either can back it.

- **`candle`** (pure-Rust tensor lib): no native/C++ dependency, so it packages cleanly into
  one static-ish binary across the three OSes and keeps the "one binary" promise (PRODUCT_SPEC
  §2). Weights load from safetensors. GPU via CUDA/Metal features, CPU otherwise. Downside:
  fewer pre-converted models, more per-model porting effort, and newer/less battle-tested on
  exotic ops.
- **ONNX Runtime via `ort`** (bindings to the C++ ORT): huge model availability (anything
  exportable to ONNX), mature CPU kernels, and execution providers (CoreML/DirectML/CUDA).
  Downside: a native dependency to ship and load per platform, larger footprint, and a
  heavier packaging story that cuts against the single-binary ideal.

**Decided: `candle`** ([ADR 0006](../adr/0006-inference-runtime-candle.md), 2026-07-06) — the
single-binary/no-native-dep win keeps the "one binary" promise (PRODUCT_SPEC §2), with `ort` kept
as a **compile-time feature-gated fallback** only where a required model has no viable `candle`
port. The runtime sits behind the `Embedder` trait and is feature gated
([01-architecture-and-crates.md](01-architecture-and-crates.md)); heavy analysis is optional
and pluggable (DESIGN_GUIDELINES §2 "analysis is pluggable and optional").

### 2.3 Model packaging & loading

- Models are **not** bundled into the base binary. They are optional artefacts fetched/placed
  in 3DAM's managed store (blob cache, owned by 02) on first use of an extractor that needs
  them, or shipped as an optional install component. This keeps a metadata-only or
  convert-only install small and respects "no unsolicited network calls" (DESIGN_GUIDELINES
  §1.5) — model download is a user-initiated action, surfaced, not silent.
- Each model artefact records `{model, model_version, sha256, dim, input_spec}`. The
  `model_version` feeds the embedding-space id (§2.1) and the extractor version (§7).
- Models load **lazily and once**, cached in memory per process, shared across the worker pool
  (14). Missing model → the extractor is skipped fail-soft (the asset keeps its metadata and
  cheap-tier features; it just has no embedding), never a crash (DESIGN_GUIDELINES §2 fail
  soft, §6 graceful degradation).

### 2.4 The 3D multi-view dependency on [06](06-3d-render.md)

The shape embedding is a *composition* over the render crate, not a separate renderer:

1. This pipeline asks [06-3d-render.md](06-3d-render.md) for **N deterministic views** of the
   model — a fixed camera ring using the reproducible framing from
   [ADR 0001](../adr/0001-3d-render-backend.md) (bounding-sphere auto-fit, `radius * 2.8` at
   45° FOV) so the same mesh always renders the same views. Determinism is load-bearing for
   versioned re-analysis (§7): the embedding is only reproducible if the views are.
2. 06 returns view images (from the wgpu headless path, or its software-raster fallback on a
   GPU-less serve host per ADR 0001's consequences).
3. This pipeline runs each view through the image encoder (§2.1) and **pools** the per-view
   vectors (mean-pool then re-normalise for v1) into one shape embedding.

Contract 06 must honour: **view count, poses, framing, resolution, and background are fixed
and versioned.** If any change, it is a new render/extractor version and 3D assets must be
re-embedded (§7). This file defines *how many views and how they're pooled*; 06 defines *how a
view is rendered*.

---

## 3. Similarity search (vector / ANN)

### 3.1 Index

- Similarity uses an **approximate nearest-neighbour (ANN)** index over the normalised
  embeddings — an **HNSW graph** is the working choice (candidates per PRODUCT_SPEC §7:
  `usearch`, an HNSW crate, or `sqlite-vec`). Cosine metric (dot product on normalised
  vectors).
- **One logical index per embedding-space id** (§2.1): image, audio, and 3D vectors are never
  mixed in one graph — they're different spaces and different dims. Queries are always
  scoped to a media type's space.
- The **on-disk persistence and storage layout** of the index (mmap vs in-memory, on-disk
  format, how vectors associate to asset ids, out-of-core at 1M+) is owned by
  [02-data-model-and-storage.md](02-data-model-and-storage.md) and revisited for scale in
  [14](14-concurrency-performance-reliability.md). This file owns query semantics.

### 3.2 The "find similar" query flow

Two entry points, one execution:

- **By asset id** ("more like this"): look up the asset's stored embedding for the relevant
  space; use it as the query vector.
- **By uploaded reference**: run the reference through the same media handler + `Embedder`
  (§2) to produce a query vector on the fly (nothing is catalogued). This powers Sononym-style
  "drop a file, find neighbours" and the MCP `find_similar` with an uploaded ref
  ([11-mcp-server.md](11-mcp-server.md)).

Execution:

```
find_similar(query, media, k, filters):
  1. q = query.embedding_for(space(media))        # stored, or embed-on-the-fly
     if none: return empty (asset not yet embedded — §1.3)
  2. hits = ann_index[space(media)].search(q, k * OVERFETCH)   # ANN top-k, cosine
  3. hits = drop self (query asset) and rejected/hidden
  4. hits = apply_facet_filters(hits, filters)     # §3.3
  5. rank by cosine; return top k with score + explanation ("distance in <space>")
  6. federated: if peers present, also fan out q to each peer's
     similarity endpoint and merge/re-rank — DEFERRED to 07 (§3.4)
```

`OVERFETCH` (fetch more than `k` from the ANN, then filter) covers the fact that facet
filtering is applied *after* the ANN — the graph is content-only. This composition order is
deliberate: the ANN can't filter by facet, so we over-fetch and post-filter.

### 3.3 Composing with facet filters

Similarity composes with the same faceted filtering as text search (DESIGN_GUIDELINES §3.3,
PRODUCT_SPEC §6.3): type, tags, format, source, size, license, and media-specific facets
(e.g. `tileability`, BPM). Two viable strategies, chosen per query size:

- **Post-filter (default):** ANN → over-fetch → filter candidates against the metadata DB.
  Simple; fine when the facet set isn't extremely selective.
- **Pre-filter (selective facets):** resolve the facet predicate in SQLite first to an id set;
  if small, brute-force cosine over just those vectors (skip the ANN). Cheaper and exact when
  the filter already narrows to a handful.

The planner picks based on the estimated selectivity of the facet predicate. Either way, the
result is "similar **and** matching the filters", with each hit still carrying its similarity
score. `license` as a facet here is what makes "find similar, commercial-use only" work
(PRODUCT_SPEC §6.3).

### 3.4 Cross-peer fan-out (pointer to [07](07-sources-and-federation.md))

Similarity **fans out across federated peers**: the local engine sends the query *vector* to
each peer's similarity endpoint, each searches its own index, and hits merge/re-rank locally,
tagged with origin peer (PRODUCT_SPEC §4.4, §6.7). The mechanics — timeouts, partial results,
result caps across N peers — live in [07-sources-and-federation.md](07-sources-and-federation.md).

The one constraint this file imposes: **cross-peer ranking requires a matching embedding-space
id** (§2.1) — same model + version. A hit from a peer on a different space cannot be
distance-compared to local hits. The API advertises each peer's space id; when it mismatches,
fall back to **per-peer-ranked, grouped** results rather than a bogus unified ranking. This is
carried as an open question (§8, PRODUCT_SPEC §10).

---

## 4. Duplicate detection

Two tiers, per PRODUCT_SPEC §6.3 (a review view grouping exact and near-duplicates).

### 4.1 Exact duplicates (content hash)

- Every asset has a **content hash** over the raw file bytes (computed at scan; owned as a
  field by 02, PRODUCT_SPEC §5 identity). Identical hash = exact duplicate, byte-for-byte.
- Grouping is trivial and exact: **group by `content_hash`**. Any group of size > 1 is an
  exact-dup cluster. This is metadata-only — no decode, so it's free and runs on every asset.
- Exact dups short-circuit re-analysis: the same `content_hash` already has cached extractor
  results (§7), so a second copy inherits them instead of re-computing.

### 4.2 Near-duplicates (perceptual, per media)

Near-dupes are content-similar but not byte-identical (re-encoded, resized, resampled,
re-exported). Per-media signal:

- **Image:** a **perceptual hash** (pHash/dHash via `img_hash`, PRODUCT_SPEC §7; stored per 02,
  PRODUCT_SPEC §5). Near-dup = small **Hamming distance** between hashes. pHash is a coarse,
  cheap first pass; the image embedding (§2.1) is the finer signal (high cosine + low pHash
  distance = strong near-dup).
- **Audio:** near-dup via **audio-embedding cosine** (a re-encoded/trimmed clip has near-identical
  embedding), optionally corroborated by matching duration/sample-rate class. (A dedicated
  audio fingerprint is a later refinement — the embedding carries v1.)
- **3D:** near-dup via **shape-embedding cosine** (§2.4) plus a cheap geometry-stats match
  (vertex/triangle counts, bounding-box dims from the cheap tier, 04) as a corroborating
  signal — a re-exported mesh keeps its stats and its multi-view shape.

**Thresholds framing (not final numbers).** Each signal has a configurable threshold band:
*exact* (hash match), *strong near-dup* (very high cosine / very low Hamming — auto-grouped),
and *possible near-dup* (a review-only band that surfaces as a lower-confidence suggestion).
The bands are extractor-versioned config, tuned against scale fixtures
([15](15-observability-config-testing-packaging.md)); v1 ships conservative defaults and lets
them be adjusted, because "duplicate" is a judgement call the user disposes (DESIGN_GUIDELINES
§1.2). Near-dup grouping never merges or deletes anything — it only groups for review.

### 4.3 Grouping for the review view

- Groups are formed incrementally as assets are embedded/hashed. Exact groups key on
  `content_hash`; near-dup groups are connected components over the "strong near-dup" relation
  within one media space (union-find as edges arrive).
- Each group exposes: members, the pairwise signal that linked them (Hamming distance / cosine
  — the **explanation**, DESIGN_GUIDELINES §1.2), and a suggested "keep" (e.g. highest
  resolution / bit depth / most-permissive license). The user chooses; 3DAM never
  auto-deletes (PRODUCT_SPEC §6.2, DESIGN_GUIDELINES §1.2, §6).
- The review view (PRODUCT_SPEC §6.3) reads these groups via the API (03). Federated near-dups
  can appear cross-peer where spaces match (§3.4), deferred to 07.

---

## 5. Per-media derived signals (non-embedding)

Beyond embeddings, this analysis pass derives the media-specific attributes in PRODUCT_SPEC §5 that feed
facets and auto-tags. These are computed from `extract_features` output (04), not re-decoded
here:

- **Audio:** BPM, key, loudness, spectral descriptors (brightness, harmonicity) → auto-tags
  (`loop`/`one-shot`, `sfx`/`music`) and facets.
- **Image:** dominant colours, alpha/colour-space, perceptual hash (§4.2), classified type,
  and the **tileability metric** (§6) → auto-tags (`seamless`, `tileable`) and facets.
- **3D:** geometry stats, rig/animation/UV presence, category guess → facets and near-dup
  corroboration (§4.2).

The tileability metric gets its own section because it is an algorithm this file owns end to
end.

---

## 6. The tileability metric (algorithm)

Implements PRODUCT_SPEC §5 "Tileability metric (image)". Output is a **score + optional
`repeat_period` + classification**, cheap enough to run on every image at ingest (edge test is
~O(w+h); the periodicity pass runs on a thumbnail). It is a versioned derived extractor (§7).

### 6.1 Input preparation

1. Work on a **downscaled copy** (e.g. longest edge ~256 px) — enough for seam/periodicity,
   cheap to process.
2. Convert to **linear colour space** (undo sRGB gamma) before any per-pixel math, so seam
   and gradient comparisons are perceptually/physically meaningful.
3. If the image is a **normal map** (detected via 04's classification / naming heuristics),
   **decode RGB→vector** (`v = 2*rgb - 1`, renormalise) and run the edge test on **decoded
   vectors**, not raw RGB — a normal map's seam is a discontinuity in *direction*, not colour
   (PRODUCT_SPEC §5).

### 6.2 Edge-continuity score (the core)

The idea (PRODUCT_SPEC §5): compare opposite edges as if tiled and score the seam
discontinuity **relative to the texture's own internal gradient**, so a noisy texture and a
smooth one are judged fairly rather than against a fixed threshold.

```
tileability_edge(img):
  # seam discontinuity: how different are the pixels that would touch when tiled
  seam_LR = mean( diff(img.col[0],  img.col[W-1]) )   # left edge vs right edge
  seam_TB = mean( diff(img.row[0],  img.row[H-1]) )   # top edge  vs bottom edge
  seam    = 0.5 * (seam_LR + seam_TB)

  # internal reference: the texture's own neighbour-to-neighbour variation
  internal = mean( diff(img[x,y], img[x+1,y]) and diff(img[x,y], img[x,y+1]) over interior )

  # normalise the seam against the internal gradient (avoid div-by-0 on flat images)
  ratio = seam / (internal + eps)

  # map ratio→score in [0,1]: ratio≈1 (seam looks like ordinary internal variation) ⇒ ~1.0;
  # ratio ≫ 1 (seam is a visible discontinuity vs the interior) ⇒ →0
  score = clamp(1 - k*(ratio - 1), 0, 1)   # k tuned on fixtures (§15); linear v1, sigmoid later
  return score
```

- `diff(a,b)` is per-channel absolute difference (RGB), or angular difference for decoded
  normals (§6.1).
- Normalising by `internal` is what makes the score fair: a noisy texture has a large
  `internal`, so a moderately different seam still scores well; a smooth gradient has a tiny
  `internal`, so even a small seam step reads as a real discontinuity.

### 6.3 `repeat_period` (optional periodicity pass)

Detect whether the image *already contains internal repetition* (e.g. a 512 tile packed 2×2
into a 1K file), via **autocorrelation / FFT peaks** (PRODUCT_SPEC §5):

```
repeat_period(img):
  # per axis, on the thumbnail (linear grayscale)
  for axis in [x, y]:
    ac = autocorrelation_1d(project(img, axis))      # or FFT power spectrum
    peak = strongest non-zero-lag peak in ac
    if peak.prominence > tau: period[axis] = peak.lag   # in source-pixels
    else:                     period[axis] = None       # non-repeating on this axis
  return period  # {x, y}, either may be None
```

Absent (`None`) when the content is non-repeating. `tau` (peak prominence threshold) is tuned
on fixtures. Runs on the thumbnail, so it's cheap.

### 6.4 Classification

Derive the label (PRODUCT_SPEC §5) from the two signals:

```
classify(score, period):
  if period.x is Some or period.y is Some:  return TILED       # contains internal repetition
  if score >= seamless_threshold:            return SEAMLESS    # edges wrap cleanly
  return NON_TILING                                             # neither
```

`seamless_threshold` is versioned config. Output stored as `{tileability: score, repeat_period:
period, class: seamless|tiled|non_tiling}` (fields per 02). Feeds auto-tags (`seamless`,
`tileable`) and becomes a search facet ("show non-seamless textures I need to fix" / "only
truly seamless materials", PRODUCT_SPEC §6.3). The score + which test drove it is the
**explanation** shown in the inspector (DESIGN_GUIDELINES §1.2).

---

## 7. Extractor versioning & re-analysis

Reproducibility is an invariant (PRODUCT_SPEC §8, DESIGN_GUIDELINES §6): analysis is versioned
and results are explainable and regenerable.

### 7.1 Versioned extractors + cache key

- Every extractor — each embedder, the tileability metric, each perceptual hash, each
  classifier, the multi-view render config — has a stable **`extractor_id`** and a monotonic
  **`extractor_version`**. The version bumps whenever the algorithm, model weights, model
  input spec, or (for 3D) the render view set changes.
- Extractor output is cached under the **content-hash-keyed key**:

  ```
  cache_key = (content_hash, extractor_id, extractor_version)
  ```

  Keying on `content_hash` (not asset id) means identical files share results — a duplicate
  or a re-scanned-in-place file reuses the cache; and it means the cache is naturally
  invalidated when a file's bytes change (new hash ⇒ new key ⇒ due for extraction). The
  storage of this cache is owned by [02-data-model-and-storage.md](02-data-model-and-storage.md).

### 7.2 Re-analysis when models improve

- Each asset records the `(extractor_id, extractor_version)` set it has been analysed with
  (the `analysis_version` in PRODUCT_SPEC §5 "Derived"). The **Plan** stage (§1.2) diffs the
  installed extractor versions against what each asset has, and enqueues only the assets whose
  extractors are behind — incremental, not a full re-scan.
- Bumping an embedder version invalidates its slice of the ANN index (that embedding-space id,
  §3.1): affected assets are re-embedded and re-upserted; the old space's vectors are
  superseded. Cross-peer similarity requires **matching** space ids, so a version bump also
  breaks cross-peer ranking until peers upgrade (§3.4, §8).
- Re-analysis is a background, cancellable job (14) and never touches confirmed user data:
  new suggestions are surfaced for accept/reject as fresh suggestions (§1.4); prior
  confirmations stand until the user acts. Analysis can be enabled/disabled per extractor
  (DESIGN_GUIDELINES §2, PRODUCT_SPEC §6.11 "Analysis & watch" flag), so a low-powered serve
  host can serve a static catalog with re-analysis off.

---

## 8. Open questions

Carried from PRODUCT_SPEC §10 and rolled up in [00-overview.md](00-overview.md):

- ~~**Concrete embedding models per media type**~~ — **Researched 2026-07-06**
  ([`spikes/embedding-models/`](../../spikes/embedding-models/README.md)): **SigLIP 768-d** (image,
  candle-native; + DINOv2 384-d for dedup), **LAION-CLAP 512-d** (audio, via `ort` — no candle
  impl), **multi-view→SigLIP 768-d** (3D). Dims recorded in §2.1. **Remaining: a follow-up *code*
  spike** to validate on-domain retrieval quality on real game assets and real candle/`ort` latency
  before dims freeze. Still gated on the `Embedder` contract (§2.1), so any swap is a `model_version`
  bump (§7).
- ~~**Inference runtime pick**~~ — **Decided: `candle`** ([ADR 0006](../adr/0006-inference-runtime-candle.md), 2026-07-06), `ort` as a feature-gated fallback. Only the *concrete models* (above) remain open.
- **Vector index choice** (§3.1) — embedded extension (`sqlite-vec`) vs standalone crate
  (`usearch`/HNSW), in-memory vs on-disk at 1M+ scale. Storage layout is 02's; the
  perf/out-of-core call is shared with [14](14-concurrency-performance-reliability.md).
- ~~**Cross-peer similarity** (§3.4)~~ — **Decided 2026-07-06** ([`spikes/cross-peer-similarity/`](../../spikes/cross-peer-similarity/README.md)):
  **advertise the `EmbeddingSpace` `space_id` and gate cross-peer ranking on exact match**; fall
  back to per-peer-ranked grouped results when spaces differ; negotiation deferred. The spike
  measured same-space rank corr **0.817** vs **~0** for mismatched spaces with a zero-error gate.
  The `space_id` should be **content-addressed on the model-artefact sha256** (§2.3) so it can't
  false-positive. Mechanics live in [07-sources-and-federation.md](07-sources-and-federation.md).
- **Dedup/tileability thresholds** (§4.2, §6) — the strong/possible near-dup bands, the
  edge-score mapping constant `k`, `seamless_threshold`, and periodicity `tau` need tuning
  against real, messy scale fixtures ([15](15-observability-config-testing-packaging.md))
  before defaults are frozen.

---

See also: [00-overview.md](00-overview.md) · [04-media-handlers.md](04-media-handlers.md) ·
[02-data-model-and-storage.md](02-data-model-and-storage.md) ·
[06-3d-render.md](06-3d-render.md) ·
[07-sources-and-federation.md](07-sources-and-federation.md) ·
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) · [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md)
