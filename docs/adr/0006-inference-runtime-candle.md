# ADR 0006 — On-device inference runtime: candle

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §2, §6.2, §7, §10,
[tech-spec 05 §2](../tech-spec/05-analysis-similarity-dedup.md),
[tech-spec 01](../tech-spec/01-architecture-and-crates.md)

## Context

3DAM's automation runs **on-device** embedding/inference per media type — CLIP-style image,
an audio encoder, and a multi-view/shape model — feeding similarity, auto-tag, and
auto-categorise ([tech-spec 05 §2](../tech-spec/05-analysis-similarity-dedup.md)). Privacy is a
hard rule: no cloud calls, no telemetry (PRODUCT_SPEC §2, DESIGN_GUIDELINES §1.5).

PRODUCT_SPEC §7 named two runtimes behind the `Embedder` trait: **`candle`** (pure-Rust tensor
lib) and **ONNX Runtime via `ort`** (C++ bindings). tech-spec 05 §2.2 leaned candle and left the
final pick to a spike-gated follow-up. The trade is single-binary packaging vs breadth of
ready-made models.

## Decision

**Use `candle` as the primary on-device inference runtime.** Keep `ort` as a **compile-time
feature-gated fallback** for any specific model with no viable candle port. The pipeline depends
only on the `Embedder` trait (tech-spec 05 §2.1), so a per-model runtime choice never leaks
upward and swapping is a version bump, not a rewrite.

Rationale:

- **Preserves the "one binary" promise (PRODUCT_SPEC §2).** candle is pure Rust with no native
  C++ dependency to ship and dynamically load per OS, so the single `3dam` binary across
  Linux/Windows/macOS stays intact — consistent with the packaging story in
  [tech-spec 15](../tech-spec/15-observability-config-testing-packaging.md) §15.5 and the
  one-binary-three-roles architecture.
- **Weights are `safetensors`,** loaded lazily and once (tech-spec 05 §2.3); models are optional
  artefacts fetched on first use (user-initiated, not bundled), which fits the no-unsolicited-
  network rule.
- **GPU optional, CPU default.** candle offers CUDA/Metal features where available and CPU
  otherwise — matching the "analysis is pluggable and optional" guideline and the headless serve
  host that may have no GPU.
- **`ort` stays available** precisely where candle's model coverage falls short, gated by the
  `ort` build feature (tech-spec 01), so we lose no reach — we just don't pay the native-dep cost
  by default.

## Consequences

**Positive**
- Single self-contained binary; no per-platform native runtime to package, sign, or load.
- Clean seam: `Embedder` trait + `EmbeddingSpace` id means the runtime is an implementation
  detail, versioned for reproducible re-analysis (tech-spec 05 §7).

**Negative / risks**
- **Smaller ready-made model zoo than ONNX.** Some models need porting/conversion to candle;
  where that is uneconomic, the `ort` fallback carries them (a heavier build for that config).
- candle is younger than ORT on exotic ops — mitigated by choosing well-supported architectures
  when picking concrete models (still open, below).

**Follow-ups**
- ~~**Concrete model selection per media type**~~ — **researched 2026-07-06**
  ([`spikes/embedding-models/`](../../spikes/embedding-models/README.md)): **SigLIP 768-d** (image,
  candle-native; + DINOv2 384-d dedup) and **multi-view→SigLIP 768-d** (3D) stay pure-candle, but
  **audio (LAION-CLAP 512-d) has no candle implementation and forces the `ort`/ONNX path** — so the
  fallback below is **load-bearing in v1, not hypothetical.** Dims are provisional pending the
  validation spike.
- Wire the `candle` (default) / `ort` (**required for audio**) feature gating in [01](../tech-spec/01-architecture-and-crates.md).
- A follow-up load/latency + on-domain-quality spike per chosen model (real game assets, real
  candle/`ort` CPU latency) before locking the dimensions in `EmbeddingSpace`.
