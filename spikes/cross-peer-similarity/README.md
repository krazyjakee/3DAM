# Spike — cross-peer similarity: advertise a space_id and gate on exact match

Status: **Decided** · Date: 2026-07-06 · Settles the open question in
[tech-spec 05 §3.4](../../docs/tech-spec/05-analysis-similarity-dedup.md) /
[tech-spec 07 §5](../../docs/tech-spec/07-sources-and-federation.md) / PRODUCT_SPEC §10.

## Question

Ranking "find similar" hits **across federated peers** is only meaningful when the peers embed
into the **same vector space** — same model **and** version. A cosine distance from peer A's
index is comparable to peer B's only if both used the same embedder. The three candidate v1
mechanisms (PRODUCT_SPEC §10):

- **(a) advertise** an embedding-space id in the API and **gate** cross-peer ranking on exact match;
- **(b) negotiate** a shared space between peers;
- **(c) fall back** to per-peer-ranked, **grouped** (not cross-ranked) results.

Which is the right v1 default? The spike must produce evidence, not assertion: is a cheap
`space_id` string-equality gate actually sufficient to separate "safe to cross-rank" from
"garbage if cross-ranked", with zero false positives?

## What it does

[`src/main.rs`](src/main.rs) — ~330 lines, **zero dependencies** (fixed-seed splitmix64 PRNG,
matching the vector-index spike's convention). It builds no models; it demonstrates the
**space-compatibility logic** on synthetic-but-realistic embeddings.

- Defines `EmbeddingSpace { model_id, model_version, dim, metric, normalization }` and its
  advertised `space_id()` string — the exact identity the federation API would publish
  ([05 §2.1](../../docs/tech-spec/05-analysis-similarity-dedup.md),
  [07 §5](../../docs/tech-spec/07-sources-and-federation.md)). Compatibility = string equality.
- Generates a **shared latent concept** per asset (clustered into 12 themes, so it lives on a
  manifold like real embeddings). Each peer "embeds" the *same* assets under its own space:
  - **LOCAL / P1-same** — same model+version (same projection matrix), independent small
    per-peer measurement noise. Same box vs different box, **same space**.
  - **P2-v4-rot** — same `model_id`/dim but `model_version 4` ⇒ a **rotated basis** (the same
    information in an incompatible coordinate frame — what a different model version does).
  - **P3-otherdim** — a wholly **different model at a different dim** (128 → 96).
- **Ground truth** is the true latent-concept cosine ranking of the corpus (model-independent).
  For each peer it transports the *local* query vector, cosines it against **that peer's** corpus
  (exactly what unified cross-peer ranking does), and measures **Spearman rank correlation**
  against ground truth, plus a **mean-similarity gap** (how far the peer's top score sits from
  the same-space baseline's scale).
- Evaluates the **gate**: does `space_id` equality predict which peers are actually cross-rankable
  (corr ≥ 0.5), counting false positives (gated UNIFIED but ranking invalid) and false negatives.

Fully deterministic: single fixed seed (`0x3da3c0ffee000007`), no system time, no true RNG.

## Environment

Dev box: Rust 1.95, `--release`. No native deps, no GPU, no models — the spike is pure
space-compatibility logic and runs anywhere in <1 s.

## Results

Advertised space ids (from `space_id()`):

```
LOCAL        clip-vit-b32@3/d128/cosine/l2
P1-same      clip-vit-b32@3/d128/cosine/l2
P2-v4-rot    clip-vit-b32@4/d128/cosine/l2      <- model_version differs
P3-otherdim  openclip-vit-l14@1/d96/cosine/l2   <- model + dim differ
```

500 shared assets, 60 queries, latent-dim 64:

| peer pair (LOCAL query → …) | space match | cross-rank | rank corr vs truth | mean-sim gap |
|-----------------------------|:-----------:|:----------:|-------------------:|-------------:|
| **P1-same** (same model+version) | **MATCH** | **UNIFIED** | **0.817** | **0.001** |
| **P2-v4-rot** (v4, rotated basis) | differ | grouped | **−0.003** | 0.750 |
| **P3-otherdim** (other model, d96) | differ | grouped | **0.025** | 0.742 |

Gate mechanism (a) — `space_id` string-equality:

- false positives (gated UNIFIED but ranking invalid): **0**
- false negatives (gated grouped but ranking was fine): **0**

Anchor — same-space cross-peer rank corr (LOCAL query vs P1 corpus): **0.817**.

## Findings

1. **Same space is directly cross-rankable.** LOCAL-query-against-P1-corpus tracks the true
   semantic ordering at **corr 0.817** with a **0.001** similarity-scale gap — despite the two
   peers being *different boxes* with independent measurement noise. Same model+version means
   the vectors live in one coordinate frame; cosine across the two indexes is meaningful and
   rank-preserving. (It is 0.817, not 1.0, precisely because independent per-peer noise is
   modelled — an honest "strongly rank-meaningful", not a rigged 1.0.)
2. **A different space is garbage, and quantitatively so.** A mere `model_version` bump that
   rotates the basis (**P2**) collapses cross-ranking to **corr −0.003** — statistically
   *unrelated* to the true order, i.e. cross-ranking incompatible spaces is noise. A different
   model at a different dim (**P3**) is the same story: **corr 0.025**. Both also sit ~**0.75**
   off the same-space similarity scale, so the numbers aren't even on a comparable axis — you
   can't interleave them by score. This is the concrete evidence that unified ranking across
   mismatched spaces is meaningless.
3. **The cheap gate is exactly right.** A one-line `space_id` string-equality check separated the
   comparable peer from the two incomparable ones with **zero false positives and zero false
   negatives**. The advertised id — `(model_id, model_version, dim, metric, normalization)` —
   carries all the discriminating information; nothing subtler (probing vectors, distribution
   tests) is needed to make the v1 call.

## Verdict

**v1 default = mechanism (a): advertise a `space_id` and gate cross-peer ranking on exact match,
with per-peer-grouped fallback (c) when spaces differ. Negotiation (b) is deferred.**

- **(a) advertise + gate** is correct and nearly free: peers publish
  `EmbeddingSpace { model_id, model_version, dim, metric, normalization }` in `advertise()`
  ([07 §5](../../docs/tech-spec/07-sources-and-federation.md)); the querying engine unifies
  ranking only for exact-match peers. The spike shows this admits **only** genuinely comparable
  peers (0 false positives) and excludes **none** it shouldn't (0 false negatives).
- **(c) grouped fallback** is the right presentation for mismatched peers: their hits are shown
  as a separate "similar on `<peer>`" section, never interleaved into the unified ranking. The
  −0.003 / 0.025 correlations prove interleaving them would be actively misleading. This should
  be the **default over silently omitting** them (07 §5's option (a) "omit") — the results are
  still useful *grouped*, just not *cross-ranked*.
- **(b) negotiation is not worth it for v1.** There is nothing to negotiate cheaply: making two
  peers share a space means agreeing on a model+version and **re-embedding** one side's whole
  catalog (a [§7](../../docs/tech-spec/05-analysis-similarity-dedup.md) version-bump-scale job),
  not a handshake. The gate already extracts the full value; negotiation is a heavier,
  later-if-ever optimisation for tightly-coupled peer clusters.

Text/facet fan-out is unaffected and always merges (07 §5) — the gate is scoped to *similarity*.

## Caveats / follow-ups

- **Synthetic embeddings.** A linear projection + rotation is a stand-in for real model
  behaviour. The *direction* of the result (same space comparable, rotated/other-dim not) is
  robust and matches theory, but the absolute 0.817 same-space number will shift on real
  CLIP/audio/shape embeddings — validate the gate on two real checkpoints (same model two builds;
  a model version bump) before freezing the corr≥0.5 "valid" threshold used here.
- **String-id discipline.** The gate is only as good as the advertised id being **honest and
  complete**. If two genuinely different models ever collide on `space_id` (e.g. someone reuses a
  `model_id` without bumping `model_version`), the gate false-positives. Real impl should key the
  id on the **model artefact sha256** ([05 §2.3](../../docs/tech-spec/05-analysis-similarity-dedup.md)
  records `{model, model_version, sha256, dim}`), not just a human string, so the id is
  content-addressed and can't lie.
- **Untested:** partial results / timeouts when a compatible peer is slow (07 §6 mechanics),
  and whether a "possible near-dup" *review* band could ever surface cross-space groupings
  (no — same gate applies; noted for completeness).

## Run it

```sh
cargo run --release        # deterministic; prints the table above
```
