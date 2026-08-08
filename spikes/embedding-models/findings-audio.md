# Spike — audio embedding model for v1

Status: **Research** · Date: 2026-07-06 · Informs the open question in
[tech-spec 05 §2.1 / §8](../../docs/tech-spec/05-analysis-similarity-dedup.md) (concrete
embedding models per media type) and PRODUCT_SPEC §10, under the runtime constraint of
[ADR 0006](../../docs/adr/0006-inference-runtime-candle.md) (candle primary, `ort`/ONNX
feature-gated fallback).

## Question

Which concrete **audio embedding model(s)** should 3DAM ship in v1 to power on-device
**similarity of short game-audio samples** — SFX / one-shots, music loops, ambient beds, and
voice/dialogue (PRODUCT_SPEC §1, §5)? The dominant use case is **timbral / acoustic similarity
à la Sononym** ("drop a footstep, find similar footsteps across my SFX library",
[tech-spec 05 §3.2](../../docs/tech-spec/05-analysis-similarity-dedup.md)), with an optional
secondary of **text→sound search** ("find *metallic impact*"). The winner must:

- give good retrieval on **non-speech** audio (SFX + music + timbre), not just speech;
- collapse **variable-length** audio to one fixed-dim vector cleanly (§2.1 contract);
- be small/fast enough for CPU-default on-device inference (ADR 0006);
- ship under a permissive licence; and
- **run via candle, or export to ONNX for the `ort` fallback** (ADR 0006 hard constraint).

This is desk research (benchmarks + candle/ONNX availability), not a code spike — see
*Remaining to validate*.

## Candidates

| model | dim | size (audio tower) | licence | candle? | ONNX? | notes |
|-------|-----|--------------------|---------|:-------:|:-----:|-------|
| **LAION-CLAP** (HTSAT + RoBERTa, `clap-htsat-unfused`) | **512** | ~158M total (HTSAT-tiny audio enc.) | **Apache-2.0** | ✗ (no port) | ✅ (HF Optimum / Transformers.js; `Xenova/*clap*`) | text+audio; trained on music + environmental + speech; **built-in variable-length feature fusion**; strong zero-shot on SFX/music |
| **MS-CLAP** (Microsoft, CNN14 + BERT) | 1024 | ~80M (CNN14 audio enc.) | MIT | ✗ | ✅ (exportable) | text+audio; CNN14 audio tower; competitive on env-sound/retrieval |
| **PANNs CNN14** (audio tagging) | **2048** | ~80M | **MIT** (Apache in some repos) | ✗ | ✅ (community ONNX exports exist) | audio-only tagging embedding; AudioSet mAP 0.431; strong SFX/env baseline; no text arm |
| **OpenL3** | 512 / 6144 | ~4.7M–18M | MIT | ✗ | partial (TF/Kapre frontend awkward) | env- or music-trained variants; good self-supervised audio-audio similarity; small |
| **VGGish** | **128** | ~72M | Apache-2.0 | ✗ | ✅ (widely exported) | old (2017) AudioSet CNN; cheap, low-dim; weaker than CLAP/PANNs today |
| **BEATs / PaSST** | 768+ | ~90M | MIT / research | ✗ | partial | SOTA-ish audio tagging; heavier; thinner tooling |
| **EnCodec / DAC / Mimi / SNAC** | codec latents | small | MIT/Apache | **✅ (in candle)** | ✅ | **codecs, not semantic embedders** — RVQ latents optimise reconstruction, not timbral similarity; wrong tool for retrieval |
| **Whisper encoder** | 512–1280 | 39M–1.5B | MIT | **✅ (in candle)** | ✅ | speech-pretrained; near-random on non-speech tasks — **domain mismatch** for SFX/music |
| **Wav2Vec2 / HuBERT** | 768–1024 | 95M+ | MIT/Apache | ✗ | ✅ | speech SSL; same non-speech mismatch |
| **MFCC / log-mel + PCA** (classic DSP) | any (e.g. 128) | ~0 | n/a | **✅ (rustfft/realfft frontend)** | n/a | always-available cheap baseline; timbral but shallow; no semantics/text |

candle audio support (verified against `candle-transformers` model index): **EnCodec, DAC,
Mimi, SNAC, Whisper, MetaVoice, Parler-TTS, CSM** — i.e. **codecs + speech/TTS only**. There is
**no CLAP, no PANNs, no VGGish, no OpenL3** in candle today. Every candidate that is actually
good for *game-audio similarity* is therefore **not** candle-native and lands on the ONNX/`ort`
path (or a from-scratch port).

## Quality evidence

- **CLAP is the strongest general non-speech audio embedder available off-the-shelf.**
  LAION `clap-htsat-unfused` scores **89.5% zero-shot on ESC-50** (environmental-sound
  classification) and MS-CLAP reports **~90.1%**; ONE-PEACE tops both at 91.8%. CLAP is
  trained on paired audio-text across **music, environmental sound, and speech**
  (LAION-Audio-630K + AudioSet + FSD50K + Clotho + AudioCaps), which is exactly 3DAM's mixed
  SFX/music/ambient domain, and it is SOTA for **text→audio retrieval** — the secondary
  "search by words" use case comes for free from the same model.
- **MAEB (Massive Audio Embedding Benchmark, 2026; 53 encoders × 30 tasks)** confirms the
  domain split that matters here: **contrastive audio-text models (CLAP family) excel at
  environmental-sound and music tasks**, while **speech-pretrained models (Whisper, Wav2Vec2,
  HuBERT) score near-random on non-speech** — and vice-versa. Its headline conclusion is that
  "no universal audio model exists"; for 3DAM's *non-speech-dominant* asset mix, the CLAP
  branch is the right specialisation. (Caveat: MAEB's top overall slots go to 7B audio-LLMs
  like LCO-Embedding-Omni-7B / Qwen2-Audio — far too heavy for on-device v1; they are not
  candidates.)
- **PANNs CNN14** is a well-established audio-only baseline: **AudioSet mAP 0.431** and a
  2048-d embedding widely used as a similarity/teacher feature. It is a credible audio-audio
  fallback but has **no text arm** and a **4× larger vector** than CLAP (2048 vs 512), which
  matters at index scale (below).
- **Speech models are a documented mismatch.** Both candle-native audio encoders (Whisper) and
  the SSL speech models (Wav2Vec2/HuBERT) are trained for phonetic content and underperform on
  timbre/SFX/music per MAEB — so candle's *existing* audio coverage does not serve 3DAM's
  primary use case. This is the crux of the candle gap.
- **Codecs are not embedders.** EnCodec/DAC/Mimi/SNAC (the audio models candle *does* have)
  produce RVQ latents optimised for waveform reconstruction, not semantic/timbral similarity;
  using their latents for "find similar" is off-label and untested for retrieval.

**Variable-length → one vector.** CLAP solves this *inside the model*: its **feature-fusion**
design maps any-length audio to a single fixed 512-d vector in constant compute (10-s chunks;
short clips are repeat-padded, long clips fuse a downsampled global view with sampled local
views). So for CLAP the §2.1 "one fixed-dim vector per asset" contract needs **no external
pooling** — a real advantage over PANNs/VGGish, where the standard recipe is slice → per-chunk
embed → **mean-pool then L2-renormalise** (the same pooling pattern §2.4 already uses for 3D
multi-view).

**Index-scale implication (dim).** Per the [vector-index spike](../vector-index/README.md),
the shipped f16 `usearch` HNSW measured ~**1.21 GiB graph RSS delta at 1M×512**
([ADR 0016](../../docs/adr/0016-vector-index-backend.md)); dimensionality still scales the dominant
vector tape approximately linearly.
At **1M assets**:

- **512-d (CLAP):** ~**1.21 GiB** — the measured shipped graph RSS delta.
- **2048-d (PANNs CNN14):** ~**4.84 GiB** — 4× the RAM/disk for the audio index alone.
- **128-d (VGGish):** ~**0.30 GiB** — cheap but lowest quality.

512-d is the sweet spot: same footprint the vector-index spike already validated, and
scalar quantization (f16/i8, untested there) could halve/quarter it later.

## Recommendation

**Primary (v1): LAION-CLAP (`clap-htsat-unfused`), 512-d audio embedding, run via the `ort`
ONNX path.**

- **Dim = 512**, L2-normalised, cosine — fixes the audio row of the §2.1 table (currently
  "512–2048") at **512** and keeps the audio HNSW index at the ~2 GB/1M footprint the
  vector-index spike already measured.
- **Domain fit:** best off-the-shelf non-speech embedder for 3DAM's mixed SFX/music/ambient
  library (ESC-50 ~89.5%, MAEB "CLAP excels at env-sound + music"), and its **built-in
  feature fusion** gives the fixed-length vector with no bespoke pooling.
- **Two use cases, one model:** the audio tower powers Sononym-style **audio→audio** similarity
  (the primary), and the paired **text tower** delivers **text→sound search** (the secondary)
  from the *same* checkpoint and *same* 512-d space — high leverage. Both towers export to
  ONNX; ship the audio tower always, the text tower as an optional search add-on.
- **Licence:** **Apache-2.0** — cleanly shippable.

**Runtime reality (the honest part):** CLAP has **no candle implementation**, so this
recommendation **exercises the ADR 0006 `ort` fallback** rather than the preferred candle path.
This is acceptable and anticipated — ADR 0006 explicitly keeps `ort` "precisely where candle's
model coverage falls short" — but it means **audio is the media type that forces the native-dep
build**, unlike image (CLIP has candle support). CLAP exports to ONNX via HF Optimum and there
are ready Transformers.js exports (`Xenova/*clap*`) to start from; the mel/STFT frontend can be
done in Rust (`rustfft`/`realfft`, already in the audio stack, PRODUCT_SPEC §7) or inside the
ONNX graph.

**Fallback: PANNs CNN14 (2048-d, MIT) on the same `ort` path**, if a CLAP ONNX export proves
troublesome or too slow on CPU. It is audio-only (no text search), needs external
mean-pool+renorm for variable length, and quadruples the index footprint (~8 GB/1M) — so it is
a genuine fallback, not a co-primary. A **classic log-mel + PCA** DSP embedder (pure Rust,
`rustfft`) is the always-available *cheapest* tier: it needs no model download and no `ort`, so
it can back the "analysis is optional/fail-soft" path (§2.3) when no model is installed, at
clearly lower quality.

**Explicitly rejected for the primary role:** candle's *existing* audio models — Whisper
(speech mismatch) and the codecs EnCodec/DAC/Mimi/SNAC (reconstruction latents, not semantic
embeddings). Their being candle-native is not enough to overcome the domain mismatch; picking
one of them just to stay on candle would sacrifice the core Sononym use case. Also rejected:
**DCLAP** (a 7M-param distilled CLAP with ONNX exports and 5–6× speedup — attractive on size,
but **AGPL-3.0**, so not shippable in a permissively-licensed product).

## Remaining to validate (follow-up code spike)

1. **CLAP → ONNX → `ort` actually runs on-device.** Export `clap-htsat-unfused` (audio tower,
   and text tower for search), load in `ort`, and confirm end-to-end embedding of a real SFX +
   loop + one-shot set. This is the load-bearing unknown given the **no-candle** status.
2. **CPU inference latency** per clip (short one-shot vs multi-second loop) on the CPU-default
   target, and whether the mel/STFT frontend belongs in Rust or in the ONNX graph. Decide if
   CLAP's HTSAT is fast enough per-asset or needs batching in the worker pool (14).
3. **Retrieval quality on 3DAM's real domain** — build a small labelled fixture (footsteps,
   impacts, weapon SFX, loops, ambience, VO) and measure audio→audio precision@k and
   near-dup separability (feeds the §4.2 audio near-dup thresholds), plus a text→sound spot
   check. Public ESC-50/MAEB numbers are a proxy, not game-audio.
4. **Dim/quantization at scale** — validate the shipped f16 configuration and 8× candidate-recall
   floor on real audio embeddings; compare i8 only if memory pressure justifies its recall cost
   ([ADR 0016](../../docs/adr/0016-vector-index-backend.md)).
5. **space_id honesty** — record the CLAP artefact `{model_id, model_version, sha256, dim=512,
   metric=cosine}` so the cross-peer gate ([cross-peer spike](../cross-peer-similarity/README.md))
   is content-addressed; note that shipping the text tower separately must not fork the audio
   space_id.
6. **`ort` packaging cost** — since audio is the type that pulls in the native ONNX Runtime,
   measure the build/binary/packaging impact of the `ort` feature and confirm the fail-soft
   path (no model / no `ort` feature → DSP baseline or metadata-only) behaves per §2.3.

## Sources

- LAION-CLAP paper (feature fusion, variable-length) — https://arxiv.org/html/2211.06687v4
- `laion/clap-htsat-unfused` (512-d, Apache-2.0, ESC-50 89.5%) — https://huggingface.co/laion/clap-htsat-unfused
- CLAP overview / ESC-50 ~90% — https://medium.com/axinc-ai/clap-feature-extraction-model-for-searching-audio-from-text-dcfd4c93756e
- MS-CLAP (CNN14 + BERT) — https://huggingface.co/microsoft/msclap
- MAEB: Massive Audio Embedding Benchmark (2026) — https://arxiv.org/abs/2602.16008 · https://huggingface.co/blog/AdnanElAssadi/maeb
- PANNs / CNN14 (2048-d, AudioSet mAP 0.431) — https://arxiv.org/pdf/1912.10211 · https://github.com/qiuqiangkong/audioset_tagging_cnn
- candle-transformers model index (audio coverage: codecs + Whisper/TTS only) — https://docs.rs/candle-transformers/latest/candle_transformers/models/index.html
- CLAP in Transformers.js / ONNX export (`Xenova/larger_clap_music_and_speech`, HF Optimum) — https://huggingface.co/Xenova/larger_clap_music_and_speech · https://huggingface.co/docs/transformers.js/en/index
- DCLAP distilled CLAP (7M params, ONNX, **AGPL-3.0** — non-shippable) — https://github.com/NeptuneHub/AudioMuse-AI-DCLAP
- VGGish (128-d) / OpenL3 (512/6144-d) — https://zilliz.com/learn/top-10-most-used-embedding-models-for-audio-data · https://essentia.upf.edu/models.html

## Run it

Desk research — no code artefact. The validation items above are the follow-up code spike.
