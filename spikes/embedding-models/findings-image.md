# Spike — image embedding model for v1: SigLIP (candle) as the semantic space, DINOv2 as the dedup signal

Status: **Research** · Date: 2026-07-06 · Feeds the open question in
[tech-spec 05 §2.1 / §8](../../docs/tech-spec/05-analysis-similarity-dedup.md) (concrete
embedding model per media type, and the dim it fixes) under the runtime constraint of
[ADR 0006](../../docs/adr/0006-inference-runtime-candle.md) (**candle** primary, `ort` fallback).
Sibling spikes: [vector-index](../vector-index/README.md), [cross-peer-similarity](../cross-peer-similarity/README.md).

## Question

Which concrete **image embedding model** should 3DAM ship for v1? The pipeline
([05 §2.1](../../docs/tech-spec/05-analysis-similarity-dedup.md)) is written against the
`Embedder` contract — one L2-normalised, fixed-dim vector per image, feeding the HNSW similarity
index ([ADR 0016](../../docs/adr/0016-vector-index-backend.md)) and near-dup detection. Choosing the model *fixes the dim* (and therefore index memory at
1M+, per the [vector-index spike](../vector-index/README.md)), and fixes the `EmbeddingSpace`
`space_id` that the [cross-peer spike](../cross-peer-similarity/README.md) gates on.

Hard constraints and domain specifics:

- **Runtime (ADR 0006):** must run on-device via **candle**, or (only if forced) via a clean
  ONNX export behind the feature-gated `ort` fallback. **Strong preference for a model already
  implemented in `candle-transformers`** — otherwise we pay a port or take the native-dep hit.
- **On-device, no cloud, single binary** ([PRODUCT_SPEC §2](../../docs/PRODUCT_SPEC.md)): must
  run on a normal workstation (CPU default, GPU optional) and ideally a modest CPU-only NAS/serve
  host. Licence must be permissive enough to ship.
- **Two jobs, one asset type.** 3DAM images are **game-asset textures, tiling materials, normal
  maps, sprites, UI, concept/reference art, thumbnails** ([PRODUCT_SPEC §5, §6.2](../../docs/PRODUCT_SPEC.md)).
  The embedding must serve *both* **"find similar" + near-duplicate** (visual similarity) **and**
  ideally **text→image search** ("drop a word, find the texture"). Those two goals pull toward
  *different* model families (self-supervised visual vs. contrastive vision-language) — this spike
  has to weigh that trade explicitly.

## Candidate models

candle support was checked against the `candle-transformers` model set and the `candle-examples`
examples on `main` (2026-07-06): **implemented** = CLIP (`clip`), SigLIP v1 (`siglip`), DINOv2 +
DINOv2-reg (`dinov2`, `dinov2reg4`), EVA-02 (`eva2`), Chinese-CLIP. **Not implemented** = SigLIP2
(open request [candle#2799](https://github.com/huggingface/candle/issues/2799)), MetaCLIP,
jina-clip, nomic-embed-vision.

| Model | Text→img? | Dim | Params / size | Licence | candle today? | ONNX? | Notes for 3DAM |
|-------|:--------:|----:|---------------|---------|:-------------:|:-----:|----------------|
| **SigLIP v1 base/16-224** (`google/siglip-base-patch16-224`) | ✅ | **768** | ~200M / ~0.8 GB f32 | **Apache-2.0** | **✅ example ships, loads this exact ckpt** | ✅ | Sigmoid-loss CLIP; stronger retrieval than same-size OpenAI CLIP; **candle-native + permissive** — the sweet spot |
| SigLIP2 base/16 | ✅ | 768 | ~86–200M | **Apache-2.0** | ❌ (req [#2799](https://github.com/huggingface/candle/issues/2799)) | ✅ [onnx-community](https://huggingface.co/onnx-community/siglip2-base-patch16-224-ONNX) | Best-in-class quality (below), but needs the **`ort` fallback** or a candle port today |
| OpenAI CLIP ViT-B/32 | ✅ | **512** | ~150M / ~0.6 GB | MIT | **✅ (the reliably-working candle CLIP path)** | ✅ | Cheapest, smallest dim; weakest retrieval of the set; candle CLIP has config-load issues on other variants ([candle#2520](https://github.com/huggingface/candle/issues/2520)) |
| OpenAI/OpenCLIP ViT-L/14 | ✅ | 768 | ~304M / ~1.7 GB | MIT / Apache-2.0 | ⚠️ arch present, **non-B/32 configs flaky** ([#2520](https://github.com/huggingface/candle/issues/2520)) | ✅ | Higher quality than B/32, ~2× the cost; candle loadability is the risk |
| **DINOv2 ViT-S/14** (`facebook/dinov2-small`) | ❌ visual only | **384** | ~22M / ~0.09 GB | **Apache-2.0** | **✅ example ships** | ✅ (community exports) | **Best pure-visual similarity / near-dup**; tiny + fast; **no text**. Ideal dedup signal |
| DINOv2 ViT-B/14 | ❌ visual only | 768 | ~86M / ~0.35 GB | Apache-2.0 | ✅ | ✅ | Stronger visual than S, 4× params; same "no text" caveat |
| EVA-02 CLIP | ✅ | 512–768 | large | MIT | ✅ (`eva2`) | partial | Strong benchmarks but heavier; less obviously better per-watt than SigLIP for our sizes |
| jina-clip-v2 / nomic-embed-vision | ✅ | 512–768 | ~0.9 GB | mixed (jina CC-BY-NC for weights ⚠️) | ❌ | ✅ | Good retrieval, but **no candle** and jina weights carry a **non-commercial** licence risk — disqualifying for shipping |
| MetaCLIP | ✅ | 512–768 | ViT-B..H | CC-BY-NC ⚠️ | ❌ | ✅ | Strong data-curation story but **non-commercial licence** and no candle |

Sizes/params are indicative (f32; f16/quantised roughly halves RAM). "candle today" reflects a
2026-07-06 read of `main`; treat as a point-in-time snapshot.

## Quality evidence (cited)

**Vision-language retrieval (text↔image), the "search + semantic similar" axis.**

- **SigLIP beats same-size OpenAI CLIP.** SigLIP's sigmoid loss gives higher zero-shot and
  retrieval numbers at matched model size; SigLIP2 then improves on SigLIP again by ~2–3 pts
  top-1 / recall@1 on ImageNet-family and lifts COCO **text→image R@1 from 47.4% → 53.2%** at
  ViT-B/16 — and its captioning + self-distillation + masked-patch objectives make the *visual*
  features carry more detail, closing much of the gap to pure-visual models
  ([SigLIP 2 paper, arXiv:2502.14786](https://arxiv.org/html/2502.14786v1);
  [HF SigLIP 2 blog](https://huggingface.co/blog/siglip2)).
- **CLIP scales with size but costs for it.** OpenAI CLIP ViT-B/32 ≈ 63.2% zero-shot ImageNet
  top-1 vs ViT-L/14 ≈ 75.5% — L/14 is clearly stronger but ~2× the compute and (in candle) the
  loadable path is really only B/32 today
  ([OpenCLIP results table](https://github.com/mlfoundations/open_clip);
  [candle#2520](https://github.com/huggingface/candle/issues/2520)).
- Independent 2025 write-ups put **SigLIP2 ahead of CLIP and (for retrieval robustness under
  image transforms) ahead of or level with DINOv2**, while **DINOv2 remains top for
  pure-visual** classification/retrieval
  ([Voxel51 embedding-model comparison](https://voxel51.com/blog/finding-the-best-embedding-model-for-image-classification);
  [SigLIP2 topic overview](https://www.emergentmind.com/topics/siglip2-model)).

**Pure visual similarity / near-duplicate, the "dedup + find-similar-look" axis.**

- **DINOv2 is purpose-built for this.** It reports large instance-retrieval gains — up to
  **+34% mAP on Oxford Hard** vs prior SSL — and Meta *used DINOv2 embeddings + nearest-neighbour
  as their own near-duplicate deduplication pipeline*, i.e. the model is validated *as a
  near-dup detector*, which is exactly 3DAM's [§4.2](../../docs/tech-spec/05-analysis-similarity-dedup.md)
  job ([DINOv2 paper, arXiv:2304.07193](https://arxiv.org/html/2304.07193v2);
  [DINOv2 overview](https://www.emergentmind.com/topics/dinov2)).
- Being label- and text-free, DINOv2 keys on **visual structure** rather than nameable concepts —
  a good fit for the **unusual, non-photographic** content 3DAM sees (tiling textures, normal
  maps, sprite sheets), where a photo-caption-trained CLIP has *no in-distribution training
  signal* and may cluster by irrelevant semantics.

**Domain caveat (be honest — evidence here is thin).** Every candidate above is trained (or
self-supervised) predominantly on **natural photographs**. 3DAM's textures/tiles/normal-maps/UI
are **out-of-distribution**; none of the cited benchmarks measure game-asset retrieval. We should
*expect* CLIP-family text→image to be weakest exactly on flat tiling textures and normal maps
(no obvious "caption"), and expect DINOv2's structural features to degrade more gracefully there —
but this is **reasoning, not measurement**, and is the top item to validate on real 3DAM images.

## Recommendation

**Primary (v1): SigLIP v1 base/patch16-224 — dim 768 — via candle.**

Rationale, grounded in the two hard constraints:

1. **It satisfies ADR 0006 with zero friction.** It is *already implemented in
   `candle-transformers`*, and the shipped example loads this exact checkpoint
   ([candle siglip example](https://github.com/huggingface/candle/tree/main/candle-examples/examples/siglip)) —
   no port, no ONNX, no native `ort` dependency, single binary intact. Nothing else that is both
   text-capable *and* best-in-class is candle-native today.
2. **It is the best text↔image option that is candle-native.** Sigmoid-loss SigLIP out-retrieves
   same-size OpenAI CLIP, giving 3DAM real **text→image search** and semantic "find similar" in
   one 768-d space — and 768-d matches the [05 §2.1](../../docs/tech-spec/05-analysis-similarity-dedup.md)
   indicative range and the [vector-index spike](../vector-index/README.md)'s 512-d test point
   closely (index memory ≈ N·768·4 B ≈ **~3.1 GB resident at 1M** f32, ~1.5× the spike's 512-d
   2.2 GB — the spike's f16/i8 quantisation follow-up becomes *more* important at 768; note it as
   a memory lever for the NAS host).
3. **Apache-2.0** — clean to ship.

**Pair it with DINOv2 ViT-S/14 (dim 384, candle-native, Apache-2.0) as the near-duplicate /
pure-visual signal.** This is the honest reading of the evidence: **no single model is best at
both jobs.** SigLIP gives text search + semantic similarity; DINOv2 is the validated near-dup /
"same-look" detector, tiny (~22M params) and cheap enough to also run at scale, and more robust on
3DAM's non-photographic content. This slots directly into the existing design — [05
§4.2](../../docs/tech-spec/05-analysis-similarity-dedup.md) already layers **pHash (coarse) → the
finer embedding**; DINOv2 becomes that finer near-dup embedding, while SigLIP is the primary
"similarity/search" space in [§2.1](../../docs/tech-spec/05-analysis-similarity-dedup.md). Both are
just two `EmbeddingSpace`s (§2.1) with distinct `space_id`s; the pipeline already supports
one-index-per-space ([§3.1](../../docs/tech-spec/05-analysis-similarity-dedup.md)).

**If v1 must ship exactly one image model:** ship **SigLIP** alone (text search is a headline
feature and SigLIP's visual side is respectable), and defer DINOv2 to a fast-follow — DINOv2 is
the cheaper add, so this ordering keeps text→image while leaving dedup on pHash + SigLIP-cosine
until DINOv2 lands.

**Fallback / upgrade path: SigLIP2 base/16 (dim 768) via the `ort` feature gate.** SigLIP2 is
measurably better (above) but is *not* in candle yet ([#2799](https://github.com/huggingface/candle/issues/2799)).
It has official ONNX exports
([onnx-community siglip2-base-patch16-224-ONNX](https://huggingface.co/onnx-community/siglip2-base-patch16-224-ONNX)),
so it drops onto ADR 0006's `ort` fallback **at the same 768-d**, or becomes a candle version-bump
([05 §7](../../docs/tech-spec/05-analysis-similarity-dedup.md)) the moment #2799 lands — a
`model_version` bump, not a rewrite, because we kept the dim at 768. Down-tier fallback for the
weakest hardware is **OpenAI CLIP ViT-B/32 (512-d)** — the one CLIP path candle loads reliably.

**Explicitly not recommended for v1:** jina-clip / nomic / MetaCLIP — no candle *and* licence risk
(jina/MetaCLIP weights carry non-commercial terms), which cuts against "permissive enough to ship".

## Remaining to validate (follow-up code spike)

Evidence above is benchmark- and architecture-level; none of it is measured on 3DAM data or the
candle runtime. A follow-up spike (mirroring the vector-index/cross-peer spikes: real checkpoints,
report a table) must measure, on a corpus of **real game textures, tiling materials, normal maps,
sprites, UI, and concept art**:

1. **Actual candle latency + RAM**, CPU-only *and* GPU, for **SigLIP base/16** and **DINOv2-S/14**
   — single-image embed time and batch throughput at ingest scale ([05 §1](../../docs/tech-spec/05-analysis-similarity-dedup.md)),
   and peak resident memory on a **modest NAS-class CPU** (the ADR 0006 serve-host case). This is
   the load/latency spike ADR 0006 flagged before locking dims.
2. **Retrieval quality on-domain**, since the cited benchmarks are all natural-photo: hand-label a
   small "these are similar" / "these are near-dupes" set of 3DAM assets and compare **SigLIP vs
   DINOv2 vs pHash** on recall/precision — *especially* on tiling textures and normal maps, the
   suspected CLIP weak spot. Confirm whether DINOv2 genuinely wins near-dup enough to justify the
   second model, or whether SigLIP-cosine + pHash suffices for v1.
3. **Text→image usefulness on assets** — does SigLIP return sensible textures for queries like
   "rusty metal", "cobblestone", "grass tile"? This is the whole case for CLIP-family over
   DINOv2; if it's poor on our content, reconsider making DINOv2 primary and treating text search
   as best-effort.
4. **Real-embedding HNSW recall at 768-d and 384-d** — re-run the [vector-index spike](../vector-index/README.md)
   on *actual* SigLIP/DINOv2 vectors (its numbers were synthetic clustered data) to confirm
   recall/latency hold on the true manifold, and measure the **f16/i8 quantisation** memory saving
   at 768-d that the NAS host needs.
5. **Validate the cross-peer `space_id` gate** ([cross-peer spike](../cross-peer-similarity/README.md)
   caveat) on two real builds of the chosen checkpoint before freezing its content-addressed id.

Honest bottom line: the model *choice* is well-supported by the candle constraint + published
benchmarks; the **game-asset domain performance is the genuinely unmeasured risk**, because the
literature only tests natural photos.
