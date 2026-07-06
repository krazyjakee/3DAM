//! Spike: cross-peer similarity — is an advertised `space_id` + exact-match gate enough
//! to decide when federated similarity hits can be cross-ranked?
//!
//! Settles the tech-spec 05 §3.4 / 07 §5 open question (PRODUCT_SPEC §10): ranking similarity
//! hits across federated peers only makes sense when the peers embed into the SAME vector space
//! (same model + version). The three candidate v1 mechanisms are:
//!   (a) ADVERTISE an embedding-space id in the API and GATE cross-peer ranking on exact match,
//!   (b) NEGOTIATE a shared space between peers,
//!   (c) fall back to PER-PEER-RANKED, GROUPED (not cross-ranked) results.
//!
//! This spike builds no models. It demonstrates the *space-compatibility logic* on
//! synthetic-but-realistic embeddings and asks: does a cheap `space_id` string-equality gate
//! cleanly separate "safe to cross-rank" from "garbage if cross-ranked", with no false positives?
//!
//! Usage: spike-cross-peer-similarity            (deterministic; fixed-seed PRNG)
//!
//! Method: we generate a shared latent concept per asset, then have each peer "embed" it under
//! its own EmbeddingSpace. A matching space is a linear map with small per-peer noise (the same
//! model+version on different boxes never produces bit-identical vectors, but the space is the
//! same). A mismatched space applies a random rotation, a basis permutation, or a different dim —
//! exactly what a different model/version does. We then measure whether cross-peer cosine
//! ranking still tracks the ground-truth concept ranking (Spearman rank correlation), and whether
//! the `space_id` gate predicts that outcome.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Deterministic PRNG — splitmix64. No `rand` dependency; identical across runs
// and machines. Matches the vector-index spike's convention (fixed seed only).
// ---------------------------------------------------------------------------
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Centred uniform in ~[-0.5, 0.5].
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
    /// Approx standard normal via central-limit sum of 6 uniforms.
    fn next_normal(&mut self) -> f32 {
        let mut s = 0.0;
        for _ in 0..6 {
            s += self.next_f32();
        }
        s // mean 0, variance ~0.5; scale is irrelevant post-normalisation
    }
}

const SEED: u64 = 0x3DA3_C0FF_EE00_0007;
const LATENT_DIM: usize = 64; // the shared "concept" dimensionality
const N_ASSETS: usize = 500; // vectors each peer holds
const N_QUERIES: usize = 60; // query assets we rank the corpus against

// ---------------------------------------------------------------------------
// EmbeddingSpace — the identity advertised in the federation API (tech-spec 05
// §2.1, 07 §5). Compatibility is EXACT string equality on `space_id()`.
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq)]
struct EmbeddingSpace {
    model_id: &'static str,
    model_version: u32,
    dim: usize,
    metric: &'static str,        // "cosine"
    normalization: &'static str, // "l2"
}
impl EmbeddingSpace {
    /// The advertised id. Two peers are cross-rankable iff these strings are equal.
    fn space_id(&self) -> String {
        format!(
            "{}@{}/d{}/{}/{}",
            self.model_id, self.model_version, self.dim, self.metric, self.normalization
        )
    }
}

// ---------------------------------------------------------------------------
// Vector maths (cosine on l2-normalised vectors == dot product).
// ---------------------------------------------------------------------------
fn normalize(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in v.iter_mut() {
        *x /= n;
    }
}
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // vectors are pre-normalised, but guard against dim mismatch defensively.
    let len = a.len().min(b.len());
    (0..len).map(|i| a[i] * b[i]).sum()
}

// ---------------------------------------------------------------------------
// A projection = the linear map a model applies from the shared latent concept
// space into its own embedding space, plus a small per-peer noise level.
// - Same model+version  => same matrix + tiny noise (different box, same space).
// - Rotated / permuted  => a different basis (a different model of the same dim).
// - Different dim        => a structurally different space (a different model).
// ---------------------------------------------------------------------------
struct Projection {
    out_dim: usize,
    weights: Vec<f32>, // out_dim x LATENT_DIM row-major
    noise: f32,
}
impl Projection {
    fn random(rng: &mut Rng, out_dim: usize) -> Self {
        let mut weights = vec![0.0f32; out_dim * LATENT_DIM];
        for w in weights.iter_mut() {
            *w = rng.next_normal();
        }
        Projection { out_dim, weights, noise: 0.0 }
    }
    /// Copy the same matrix (same space) but attach a per-peer measurement noise.
    fn with_noise(&self, noise: f32) -> Self {
        Projection { out_dim: self.out_dim, weights: self.weights.clone(), noise }
    }
    /// A random rotation of an existing same-dim projection: same information,
    /// incompatible basis — a stand-in for "different model, same dim".
    fn rotated(&self, rng: &mut Rng) -> Self {
        // Build a random orthogonal-ish mixing over output dims and apply it.
        let d = self.out_dim;
        let mut rot = vec![0.0f32; d * d];
        for r in rot.iter_mut() {
            *r = rng.next_normal();
        }
        // Gram-Schmidt to make it a proper rotation (orthonormal rows).
        for i in 0..d {
            for j in 0..i {
                let dot: f32 = (0..d).map(|k| rot[i * d + k] * rot[j * d + k]).sum();
                for k in 0..d {
                    rot[i * d + k] -= dot * rot[j * d + k];
                }
            }
            let n: f32 = (0..d).map(|k| rot[i * d + k].powi(2)).sum::<f32>().sqrt().max(1e-9);
            for k in 0..d {
                rot[i * d + k] /= n;
            }
        }
        let mut weights = vec![0.0f32; d * LATENT_DIM];
        for o in 0..d {
            for l in 0..LATENT_DIM {
                let mut acc = 0.0;
                for k in 0..d {
                    acc += rot[o * d + k] * self.weights[k * LATENT_DIM + l];
                }
                weights[o * LATENT_DIM + l] = acc;
            }
        }
        Projection { out_dim: d, weights, noise: self.noise }
    }
    fn embed(&self, latent: &[f32], rng: &mut Rng) -> Vec<f32> {
        let mut out = vec![0.0f32; self.out_dim];
        for o in 0..self.out_dim {
            let mut acc = 0.0;
            for l in 0..LATENT_DIM {
                acc += self.weights[o * LATENT_DIM + l] * latent[l];
            }
            out[o] = acc + self.noise * rng.next_normal();
        }
        normalize(&mut out);
        out
    }
}

// ---------------------------------------------------------------------------
// A peer: an advertised space + the embedded corpus it holds.
// ---------------------------------------------------------------------------
struct Peer {
    name: &'static str,
    space: EmbeddingSpace,
    vectors: Vec<Vec<f32>>, // one per shared asset (aligned by index)
}

// ---------------------------------------------------------------------------
// Spearman rank correlation between two rankings (score lists) over the same
// items. 1.0 = identical order, ~0 = unrelated. We use it to ask: does peer B's
// cross-space cosine ranking of the corpus track the TRUE latent-concept ranking?
// ---------------------------------------------------------------------------
fn spearman(a: &[f32], b: &[f32]) -> f32 {
    let ra = ranks(a);
    let rb = ranks(b);
    let n = a.len() as f32;
    let mean = (n - 1.0) / 2.0;
    let (mut cov, mut va, mut vb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..a.len() {
        let (da, db) = (ra[i] - mean, rb[i] - mean);
        cov += da * db;
        va += da * da;
        vb += db * db;
    }
    if va < 1e-9 || vb < 1e-9 {
        return 0.0;
    }
    cov / (va.sqrt() * vb.sqrt())
}
fn ranks(v: &[f32]) -> Vec<f32> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&i, &j| v[i].partial_cmp(&v[j]).unwrap());
    let mut r = vec![0.0f32; v.len()];
    for (rank, &i) in idx.iter().enumerate() {
        r[i] = rank as f32;
    }
    r
}

fn main() {
    let mut rng = Rng(SEED);

    // 1. Generate a shared latent concept per asset. Clustered (a handful of
    //    "themes") so it looks like a real embedding manifold, not uniform noise.
    const N_THEMES: usize = 12;
    let mut themes = vec![vec![0.0f32; LATENT_DIM]; N_THEMES];
    for t in themes.iter_mut() {
        for x in t.iter_mut() {
            *x = rng.next_normal();
        }
        normalize(t);
    }
    let mut latents = vec![vec![0.0f32; LATENT_DIM]; N_ASSETS];
    for a in 0..N_ASSETS {
        let t = &themes[a % N_THEMES];
        for l in 0..LATENT_DIM {
            latents[a][l] = t[l] + 0.6 * rng.next_normal(); // theme + spread
        }
        normalize(&mut latents[a]);
    }

    // 2. Define the reference model's projection (dim 128). "Same space" peers
    //    reuse this exact matrix; incompatible peers get a different one.
    let base = Projection::random(&mut rng, 128);
    let rotated = base.rotated(&mut rng); // same dim, different basis
    let smaller = Projection::random(&mut rng, 96); // a different-dim model

    // 3. Build peers. LOCAL is our own engine; the rest are federated peers.
    let space_clip_v3 = |dim| EmbeddingSpace {
        model_id: "clip-vit-b32",
        model_version: 3,
        dim,
        metric: "cosine",
        normalization: "l2",
    };
    let mut peers: Vec<Peer> = Vec::new();

    // LOCAL — reference space, small measurement noise.
    peers.push(build_peer("LOCAL", space_clip_v3(128), &base.with_noise(0.02), &latents, &mut rng));
    // P1 — SAME model+version, different box (same space, independent noise).
    peers.push(build_peer("P1-same", space_clip_v3(128), &base.with_noise(0.05), &latents, &mut rng));
    // P2 — same model_id/dim but model_version 4 => DIFFERENT basis (rotated).
    let mut p2_space = space_clip_v3(128);
    p2_space.model_version = 4;
    peers.push(build_peer("P2-v4-rot", p2_space, &rotated.with_noise(0.05), &latents, &mut rng));
    // P3 — a wholly different model with a different dim.
    let p3_space = EmbeddingSpace {
        model_id: "openclip-vit-l14",
        model_version: 1,
        dim: 96,
        metric: "cosine",
        normalization: "l2",
    };
    peers.push(build_peer("P3-otherdim", p3_space, &smaller.with_noise(0.05), &latents, &mut rng));

    let local = &peers[0];
    let local_id = local.space.space_id();

    println!("cross-peer similarity spike — deterministic (seed {SEED:#018x})\n");
    println!("shared assets: {N_ASSETS}   queries: {N_QUERIES}   latent-dim: {LATENT_DIM}\n");
    println!("advertised embedding-space ids:");
    for p in &peers {
        println!("  {:<12} {}", p.name, p.space.space_id());
    }
    println!();

    // 4. Ground truth: for each query asset, rank the corpus by TRUE latent
    //    cosine (the real semantic ordering, independent of any model).
    let queries: Vec<usize> = (0..N_QUERIES).map(|q| (q * 7 + 3) % N_ASSETS).collect();
    let truth: HashMap<usize, Vec<f32>> = queries
        .iter()
        .map(|&q| {
            let scores: Vec<f32> = (0..N_ASSETS).map(|a| cosine(&latents[q], &latents[a])).collect();
            (q, scores)
        })
        .collect();

    // 5. For each federated peer, cross-rank: take the query vector AS THE LOCAL
    //    ENGINE PRODUCED IT, score it against the PEER's corpus vectors (this is
    //    exactly what unified cross-peer ranking does — transport the local query
    //    vector, cosine against the peer's index), and compare that ranking to
    //    the ground-truth latent ranking via Spearman.
    println!(
        "{:<12} {:<10} {:<10} {:>12} {:>14}",
        "peer", "space", "cross-rank", "rank-corr", "mean-sim-gap"
    );
    println!("{}", "-".repeat(62));

    let mut false_positives = 0usize;
    let mut false_negatives = 0usize;

    for p in &peers[1..] {
        let space_match = p.space.space_id() == local_id; // mechanism (a): the gate
        let mut corrs = Vec::new();
        let mut gaps = Vec::new();
        for &q in &queries {
            let qvec = &local.vectors[q]; // local engine's query vector
            let peer_scores: Vec<f32> =
                (0..N_ASSETS).map(|a| cosine(qvec, &p.vectors[a])).collect();
            corrs.push(spearman(&truth[&q], &peer_scores));
            // "mean-sim gap": how far the top-ranked peer hit's similarity sits
            // from what the SAME-space baseline (LOCAL vs itself) would report —
            // a large gap means the numbers aren't on a comparable scale.
            let local_scores: Vec<f32> =
                (0..N_ASSETS).map(|a| cosine(qvec, &local.vectors[a])).collect();
            let top_local = local_scores.iter().cloned().fold(f32::MIN, f32::max);
            let top_peer = peer_scores.iter().cloned().fold(f32::MIN, f32::max);
            gaps.push((top_local - top_peer).abs());
        }
        let corr = mean(&corrs);
        let gap = mean(&gaps);

        // A peer is genuinely cross-rankable if its cross-space ranking actually
        // tracks the true ordering. We call that "valid" at corr >= 0.5.
        let actually_valid = corr >= 0.5;
        let verdict = if space_match { "UNIFIED" } else { "grouped" };

        // Gate correctness bookkeeping (mechanism (a) evaluation):
        if space_match && !actually_valid {
            false_positives += 1; // gate said OK but ranking is garbage
        }
        if !space_match && actually_valid {
            false_negatives += 1; // gate excluded a peer that was actually fine
        }

        println!(
            "{:<12} {:<10} {:<10} {:>12.3} {:>14.3}",
            p.name,
            if space_match { "MATCH" } else { "differ" },
            verdict,
            corr,
            gap
        );
    }

    println!();
    println!("gate mechanism (a) — space_id string-equality:");
    println!("  false positives (gated UNIFIED but ranking invalid): {false_positives}");
    println!("  false negatives (gated grouped but ranking was fine): {false_negatives}");
    println!();

    // 6. Sanity anchor: same-space cross-peer ranking (LOCAL query vs P1's corpus)
    //    must itself be near-perfect, proving the "same space is comparable" claim.
    let p1 = &peers[1];
    let mut same_space_corr = Vec::new();
    for &q in &queries {
        let peer_scores: Vec<f32> =
            (0..N_ASSETS).map(|a| cosine(&local.vectors[q], &p1.vectors[a])).collect();
        same_space_corr.push(spearman(&truth[&q], &peer_scores));
    }
    println!("anchor — same-space cross-peer rank corr (LOCAL query vs P1 corpus): {:.3}", mean(&same_space_corr));
    println!("         (near 1.0 => same model+version IS directly cross-rankable)");

    if false_positives == 0 {
        println!("\nRESULT: the space_id gate admitted ZERO incomparable peers into unified ranking.");
    }
}

fn build_peer(
    name: &'static str,
    space: EmbeddingSpace,
    proj: &Projection,
    latents: &[Vec<f32>],
    rng: &mut Rng,
) -> Peer {
    let vectors = latents.iter().map(|l| proj.embed(l, rng)).collect();
    Peer { name, space, vectors }
}

fn mean(v: &[f32]) -> f32 {
    v.iter().sum::<f32>() / v.len().max(1) as f32
}
