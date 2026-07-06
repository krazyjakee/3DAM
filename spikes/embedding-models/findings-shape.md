# Findings — 3D / shape embedding model for v1

Status: **Research** · Date: 2026-07-06 · Informs the open question in
[tech-spec 05 §2.1 / §8](../../docs/tech-spec/05-analysis-similarity-dedup.md) (concrete
embedding models per media type) under
[ADR 0006](../../docs/adr/0006-inference-runtime-candle.md) (runtime = `candle`, `ort`
fallback). Companion to `findings-image.md` (the image encoder) and `findings-audio.md`.

## Question

What concrete on-device model produces the **3D shape embedding** — the normalised vector
that drives "find similar", near-dup grouping, and shape-based facets for meshes/models
(glTF/OBJ/FBX/PLY/STL props, characters, environment pieces;
[PRODUCT_SPEC §1](../../docs/PRODUCT_SPEC.md), [§6.3](../../docs/PRODUCT_SPEC.md))? It must run
**on-device, no cloud** ([PRODUCT_SPEC §2](../../docs/PRODUCT_SPEC.md)) through **`candle`**, or
via the feature-gated **`ort`** (ONNX) fallback only if forced ([ADR 0006](../../docs/adr/0006-inference-runtime-candle.md)).

Two families are on the table:

- **(A) Multi-view render → 2D image embedding.** Render N canonical views of the mesh (the
  headless wgpu pipeline **already exists** and the gate passed — see
  [`spikes/headless-render/`](../headless-render/README.md)), embed each view with an image
  model, pool to one vector. This is the MVCNN family. tech-spec already designs for exactly
  this: [05 §2.4](../../docs/tech-spec/05-analysis-similarity-dedup.md) and
  [06 §7](../../docs/tech-spec/06-3d-render.md) specify a deterministic, versioned view set
  feeding a per-view image encoder with mean-pool.
- **(B) Native geometry / point-cloud models.** Consume vertices/points directly — PointNet /
  PointNet++, DGCNN, Point-BERT, Point-MAE, and the CLIP-aligned encoders ULIP / OpenShape /
  Uni3D that place shapes in a shared text–image–shape space.

## Approach comparison (A vs B)

### Feasibility in `candle` — the decisive axis

This is where the two families separate hard, and it is the binding constraint per
[ADR 0006](../../docs/adr/0006-inference-runtime-candle.md).

- **(A) is native in `candle` *today*.** `candle-transformers` ships `clip`, `openclip`,
  `siglip`, `mobileclip`, `dinov2`/`dinov2reg4`, `eva2`, and `vit` — i.e. every image encoder
  we'd want to embed a rendered view with is already implemented and loads from `safetensors`
  ([candle-transformers model list](https://docs.rs/candle-transformers/latest/candle_transformers/models/)).
  Multi-view is then **image inference we already have to build for the Image media type** plus
  a mean-pool — no new model, no `ort`. The render half is done and proven (headless wgpu +
  lavapipe fallback, [`spikes/headless-render/`](../headless-render/README.md)).
- **(B) has essentially no `candle` path.** PointNet/DGCNN/Point-BERT/Point-MAE and the
  CLIP-aligned encoders (ULIP/OpenShape/Uni3D) are PyTorch research code; none has a
  `candle-transformers` implementation, so adopting one means **porting the architecture to
  candle by hand** or leaning on the **`ort` fallback**. ONNX export of these is *not* clean
  either: point-cloud nets rely on custom ops — farthest-point sampling, ball-query grouping,
  k-NN gather (PointNet++/DGCNN) — that don't map to stock ONNX operators and typically need
  custom kernels/plugins to run under ONNX Runtime
  ([PointNet++ ONNX/TensorRT custom-plugin discussion](https://forums.developer.nvidia.com/t/torchpoints3d-pointnet2-deploy-to-tensorrt-with-custom-plugin/197074)).
  Plain PointNet (no grouping) exports more cleanly, but plain PointNet is also the *weakest*
  of the family. So (B) trades directly against the "one binary / candle-first" promise.

### Quality

Both families are strong; multi-view is at least competitive and often ahead on the
**retrieval** metric we actually care about (near-dup + "find similar"), not just classification:

- **Multi-view (A):** MVCNN gets ~90% ModelNet40 classification and **79.5% mAP retrieval**
  (80 views + metric learning; 70.1% mAP at 12 views)
  ([Su et al., MVCNN, ICCV'15](https://people.cs.umass.edu/~hsu/doc/shapes_iccv.pdf)). The
  modern multi-view successor **MVTN** reaches **82.9 mAP on ShapeNet Core55 with only 12
  views**, best among the methods it compares against
  ([Hamdi et al., MVTN, ICCV'21](https://openaccess.thecvf.com/content/ICCV2021/papers/Hamdi_MVTN_Multi-View_Transformation_Network_for_3D_Shape_Recognition_ICCV_2021_paper.pdf)).
- **Native/CLIP-aligned (B):** Uni3D hits **88.2% zero-shot ModelNet40** and 55.3% on
  Objaverse-LVIS ([Uni3D, ICLR'24](https://arxiv.org/pdf/2310.06773)); OpenShape reaches
  **83.6%** zero-shot ModelNet40 with colourless clouds
  ([OpenShape](https://colin97.github.io/OpenShape/)). Their genuine advantage is **cross-modal**:
  the shape vector lives in CLIP space, enabling **text→shape** and image→shape search directly.

Net: (B)'s headline numbers do not beat (A) on shape retrieval; (B)'s real draw is the shared
text–image space, which is a *feature* (text search of 3D), not a *quality* win for pure
shape-vs-shape similarity.

### Cost / operational

- **(A)** costs **N offscreen renders + N image-encoder passes per asset**. The render is
  cheap (the headless path is ~130–230 ms cold for a trivial scene and reuses one uploaded
  `GpuScene` across views, [06 §7](../../docs/tech-spec/06-3d-render.md)); the N image passes
  are the same code path as image ingest. It runs on the **software rasteriser** on a GPU-less
  serve host (lavapipe, gate passed), subject to the [06 §4.3](../../docs/tech-spec/06-3d-render.md)
  degradation policy. Zero new model artefacts beyond the image encoder.
- **(B)** costs a preprocessing stage 3DAM does **not** have — sample a normalised point cloud
  from an arbitrary mesh (surface sampling, FPS to a fixed point count, e.g. 8k–10k), plus the
  ported/`ort` model. More moving parts, a second heavy dependency, and no reuse of the render
  pipeline.

## Candidates

| Approach / model | Vector | Licence | candle / ONNX? | Notes |
|---|---|---|---|---|
| **(A) Multi-view + image encoder** | inherits image dim (**512–768**) | inherits image model's | **candle native today** (`clip`/`siglip`/`mobileclip`/`dinov2`) | reuses render + image stack; **own `EmbeddingSpace`, shared weights** with images; mean-pool + renorm |
| (B) PointNet / PointNet++ | ~256–1024 | varies | plain PointNet exports to ONNX-ish; **++/DGCNN need custom ops** — no candle impl | needs mesh→point-cloud sampling; weakest quality without grouping |
| (B) Point-BERT / Point-MAE | ~384–768 | mixed (research) | no candle impl; ONNX non-trivial (transformer over sampled/grouped patches) | strong SSL features; still needs the sampling front-end |
| (B) ULIP / OpenShape | 512/1280 (CLIP-dim) | research (see repos) | no candle; PyTorch | CLIP-aligned; text→shape; OpenShape 83.6% ModelNet40 |
| (B) Uni3D | 1024 (CLIP ViT-L/EVA) | research ([BAAI repo](https://github.com/baaivision/Uni3D)) | no candle; PyTorch; heavy | best zero-shot (88.2% ModelNet40) + cross-modal; large weights, GPU-oriented |

## Quality evidence (cited)

- **Multi-view retrieval:** MVCNN **70.1% mAP @12 views**, **79.5% mAP @80 views + metric
  learning** on ModelNet40; rendered as untextured shaded views, 12 azimuths × 30° at 30°
  elevation ([MVCNN, ICCV'15](https://people.cs.umass.edu/~hsu/doc/shapes_iccv.pdf)). MVTN
  **82.9 mAP on ShapeNet Core55 @12 views**
  ([MVTN, ICCV'21](https://openaccess.thecvf.com/content/ICCV2021/papers/Hamdi_MVTN_Multi-View_Transformation_Network_for_3D_Shape_Recognition_ICCV_2021_paper.pdf)).
- **Views ablation (matters for our N):** performance is **near-optimal by ~3–4 views**; even a
  **single view already beats VoxNet and PointNet**, and 12 views add only marginal gains over
  4 (one study: 89.98% @1 view → 95.26% @12 views on ModelNet10)
  ([Su et al., "A Deeper Look at 3D Shape Classifiers", 2018](https://arxiv.org/pdf/1809.02560)).
  This is the key result for us: a **small N (4–6) captures most of the signal**, keeping the
  per-asset render+embed cost low.
- **Native/CLIP-aligned:** Uni3D **88.2% ModelNet40 zero-shot**, 55.3% Objaverse-LVIS
  ([Uni3D, ICLR'24](https://arxiv.org/pdf/2310.06773)); OpenShape **83.6%**
  ([OpenShape](https://colin97.github.io/OpenShape/)).

## Recommendation

**Adopt Approach (A): multi-view render → image encoder → mean-pool, in `candle`.** This is the
pragmatic and correct v1 call, and it is exactly what tech-spec already designed the seam for
([05 §2.4](../../docs/tech-spec/05-analysis-similarity-dedup.md),
[06 §7](../../docs/tech-spec/06-3d-render.md)).

Concretely:

- **Model = the same image encoder chosen for the Image media type** (see `findings-image.md`).
  Reusing one encoder means one artefact to ship, one runtime path, and it is the encoder MVCNN
  itself uses conceptually (a 2D CNN/ViT over views). The 3D vector inherits the image encoder's
  **dim (512–768)** — consistent with [05 §2.1](../../docs/tech-spec/05-analysis-similarity-dedup.md).
- **Views = 6** — a fixed yaw ring (0/60/…/300°) at a gentle downward pitch, optionally a 7th
  top-down. The ablation evidence shows 3–4 views already hit near-optimal and >6 gives only
  marginal returns, so 6 is a safe quality/cost point. The pose set is the versioned
  `canonical_views(FramingVersion)` from [06 §7](../../docs/tech-spec/06-3d-render.md);
  bounds-relative auto-fit so a tiny prop and a large terrain frame identically (shape, not
  scale).
- **Pooling = mean-pool the 6 per-view vectors, then L2-renormalise** (the [05 §2.4](../../docs/tech-spec/05-analysis-similarity-dedup.md)
  v1 default). Cheap, order-independent, and robust; max-pool is the obvious later A/B.
- **Normalisation before rendering (load-bearing).** Assets arrive in arbitrary scale, units,
  and orientation. Before the view set, canonicalise: **recentre on the AABB/bounding-sphere
  centre and scale to a unit bounding sphere** (the render's auto-fit already does the
  scale-invariance for framing, [06 §5](../../docs/tech-spec/06-3d-render.md); the geometry side
  is the AABB/weld math mined in [3d-handler-notes §2/§4](../../docs/3d-handler-notes.md)). **Up-axis
  is the one real hazard** — glTF is Y-up, many OBJ/FBX/STL exports are Z-up; render the same
  logical orientation or two near-identical meshes land far apart. v1: apply an up-axis heuristic
  from format + geometry (bbox aspect) at ingest; carry it as a versioned part of the render
  extractor so a fix is a clean re-embed ([05 §7](../../docs/tech-spec/05-analysis-similarity-dedup.md)).
  Multi-view is **not rotation-invariant** the way a well-trained point encoder can be — this is
  the honest cost of (A) and the main thing to validate.

### Embedding space: same space as images, or its own?

Recommend **its own `EmbeddingSpace`**, even though it reuses the image encoder. Rationale
grounded in [`spikes/cross-peer-similarity/`](../cross-peer-similarity/README.md): the
`space_id` is content-addressed on `(model_id, model_version, dim, metric, normalization)` and
gates similarity. A pooled-6-view shape vector is **not distributionally the same** as a single
natural-image embedding even from the same weights (it's a mean of shaded-render views), so
mixing them in one index/graph would let 3D and image results cross-rank incorrectly. Keep
`media: 3D` distinct in the space id (one HNSW index per space, [05 §3.1](../../docs/tech-spec/05-analysis-similarity-dedup.md)),
while the *weights* are shared. This also keeps the door open to later swapping the 3D pooling
without touching the image space.

### Native models (B) as a post-v1 option

Revisit (B) — specifically a **CLIP-aligned encoder (Uni3D/OpenShape)** — only when we want
**text→shape / cross-modal 3D search** (its genuine advantage), and only once there is a candle
port or we accept the `ort` fallback + a mesh→point-cloud sampling stage. It is a feature
expansion, not a v1 quality gap: its retrieval numbers don't beat multi-view for pure
shape-vs-shape similarity. Track it against `candle-transformers` gaining a point-cloud
architecture.

### Dim + HNSW memory at 1M+

At the image dim (say 512-d f32), 3D vectors cost the **same ~2 GB resident per 1M** as the
image/audio indexes measured in [`spikes/vector-index/`](../vector-index/README.md) (one HNSW
index per space). 3D catalogs are typically far smaller than image/texture catalogs, so this is
the least-pressured of the three indexes; the same f16/i8 quantisation follow-up from the
vector-index spike applies if needed on a small serve host. Choosing a **768-d** image encoder
raises all three indexes ~1.5×, so the image-model dim choice (findings-image) directly sets the
3D memory too — a reason to prefer 512-d unless image quality demands 768.

## Remaining to validate

- **Real multi-view retrieval numbers on messy game assets** (not ModelNet CAD): pool 4/6/8
  views with the actual chosen image encoder over a fixture of re-exported/duplicated meshes;
  confirm strong-near-dup separation and pick N + pool op. Ties into the dedup-threshold tuning
  ([05 §4.2](../../docs/tech-spec/05-analysis-similarity-dedup.md)).
- **Up-axis / orientation robustness** — the main (A) weakness. Measure how far a Y-up vs Z-up
  re-export of the same mesh drifts under the canonicalisation heuristic; if too large, consider
  an orientation-canonicalising pass or a small pose-augmented view set.
- **Textured vs shaded views.** MVCNN uses untextured shaded renders; 3DAM's PBR-lite shader
  ([06 §9](../../docs/tech-spec/06-3d-render.md)) does apply base colour. Decide whether to embed
  albedo-lit or flat-shaded views (flat = purer *shape* signal, less material confound) — a view
  set flag, versioned.
- **Cross-peer gate on real 3D vectors** — the [`cross-peer-similarity`](../cross-peer-similarity/README.md)
  spike validated the space-id gate on synthetic vectors; re-confirm same-space rank correlation
  on two real builds of the pooled multi-view embedder before freezing.
- **`findings-image` dependency** — this recommendation is intentionally coupled: the concrete
  image encoder and its dim are chosen there and inherited here. Finalise together.
