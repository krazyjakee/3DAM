# Spike — concrete embedding models per media type

Status: **Research complete** · Date: 2026-07-06 · Resolves the last open *spike* from
[PRODUCT_SPEC §10](../../docs/PRODUCT_SPEC.md) / [tech-spec 05 §8](../../docs/tech-spec/05-analysis-similarity-dedup.md),
under [ADR 0006](../../docs/adr/0006-inference-runtime-candle.md) (runtime = `candle`, `ort` as a
feature-gated ONNX fallback). Gates the vector **dims** in [05 §2.1](../../docs/tech-spec/05-analysis-similarity-dedup.md)
and the analysis-lane sizing in [14](../../docs/tech-spec/14-concurrency-performance-reliability.md).

Three parallel research passes, one per media type — full evidence and citations in the companion
files:

- [`findings-image.md`](findings-image.md)
- [`findings-audio.md`](findings-audio.md)
- [`findings-shape.md`](findings-shape.md)

## Question

Which concrete on-device models produce the fixed-dim embedding per media type that drives "find
similar", near-dup grouping, and (optionally) text→asset search — runnable through `candle`, or via
the `ort`/ONNX fallback only if forced?

## Recommended v1 picks

| Media | Primary model | Dim | Runtime | Licence | Fallback |
|-------|--------------|----:|---------|---------|----------|
| **Image** | **SigLIP** base/patch16-224 | **768** | **candle-native** | Apache-2.0 | SigLIP2 via `ort`; then CLIP ViT-B/32 (512-d) |
| **Image (dedup)** | **DINOv2** ViT-S/14 *(paired, optional v1)* | **384** | **candle-native** | Apache-2.0 | — (defer if shipping one) |
| **3D / shape** | **Multi-view render → SigLIP → mean-pool** (6 views, L2-renorm) | **768** | **candle-native** (reuses headless wgpu) | Apache-2.0 | more views / different image encoder |
| **Audio** | **LAION-CLAP** `clap-htsat-unfused` | **512** | **`ort` / ONNX** (no candle impl) | Apache-2.0 | PANNs CNN14 (2048-d, MIT) via `ort`; log-mel+PCA DSP baseline (pure Rust) |

Each media type gets its **own `EmbeddingSpace`** (distinct `space_id`, per the
[cross-peer spike](../cross-peer-similarity/README.md)) — even image and shape, which share SigLIP
weights: pooled multi-view 3D vectors must never cross-rank against single-image vectors.

## Cross-cutting findings

1. **Audio forces the `ort` dependency in v1.** candle has *no* CLAP/PANNs/VGGish/OpenL3 — its only
   audio models are codecs (EnCodec/DAC/Mimi) and speech (Whisper), all domain-mismatched for
   SFX/music similarity. So [ADR 0006](../../docs/adr/0006-inference-runtime-candle.md)'s "`ort` as
   feature-gated fallback" is **not hypothetical — it is the audio path for v1.** CLAP exports to
   ONNX cleanly (HF Optimum / existing `Xenova/*clap*` exports), so the path is viable but must be
   proven end-to-end. Image and shape stay pure-candle.
2. **One image encoder serves two media types.** SigLIP is the image embedder *and* the shape
   embedder (via multi-view render), so v1 ships **one** image model, not two — the render pipeline
   already exists and passed its [gate](../headless-render/README.md).
3. **SigLIP for search, DINOv2 for dedup.** No single image model wins both jobs: SigLIP's text tower
   gives text→image search; DINOv2's text-free structural features are more robust for near-duplicate
   detection on 3DAM's *non-photographic* textures/normal-maps. CLAP's text tower likewise covers
   text→sound search from the same 512-d audio space.
4. **All benchmarks are off-domain.** Every cited number is natural-photo / general-audio; **none**
   measures game-asset retrieval (flat tiling textures, normal maps, SFX one-shots). On-domain
   quality is the shared unknown across all three — the reason the dims below are *provisional*.

## Vector-index memory implication

An HNSW index was measured at **~2 GB / 1M vectors at 512-d** (usearch, in the
[vector-index spike](../vector-index/README.md); the shipped backend is `instant-distance` —
[ADR 0016](../../docs/adr/0016-vector-index-backend.md) — and the raw-vector bytes dominate either way).
Scaling by dim, per **separate per-media index**: image 768-d ≈ 3 GB/1M, shape 768-d ≈ 3 GB/1M,
audio 512-d ≈ 2 GB/1M, optional DINOv2 384-d ≈ 1.5 GB/1M. This is what makes the deferred
**f16/i8 quantization** follow-up (vector-index spike) matter at multi-million scale.

## Decision (provisional — pending on-domain validation)

Adopt the table above as the v1 target. Dims: **image 768, shape 768, audio 512** (+ optional
DINOv2 384). These are `model_version`-tagged in each `EmbeddingSpace`, so a later swap is a version
bump (05 §7), not a rewrite.

## Remaining — a follow-up **code** spike must measure, before dims freeze

1. **On-domain retrieval quality** on a real 3DAM fixture set (textures, normal maps, sprites,
   SFX/music one-shots, game meshes) — the only way to confirm SigLIP/CLAP/multi-view actually work
   on this domain vs the natural-image/general-audio benchmarks.
2. **Real candle CPU/NAS latency + RAM** at 768-d (image/shape) and **CLAP→ONNX→`ort` CPU latency**
   end-to-end (audio) — validates the on-device budget and the analysis-lane sizing ([14](../../docs/tech-spec/14-concurrency-performance-reliability.md)).
3. **Shape canonicalisation:** up-axis (glTF Y-up vs OBJ/FBX/STL Z-up) + bounding-sphere normalize
   before rendering the 6 views — multi-view is not rotation-invariant; validate on re-exported assets.
4. **DINOv2 in-or-out for v1** — decide once dedup quality is measured on real near-duplicate packs.
