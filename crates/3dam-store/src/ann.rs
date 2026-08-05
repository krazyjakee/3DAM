//! HNSW approximate-nearest-neighbour index over an embedding space (semantic-search M6).
//!
//! The brute-force cosine scan in [`crate::similarity`] is exact and correct, but O(n·d) per query —
//! fine at v1 scale, linear at 100k+ embeddings. This wraps `instant-distance`'s HNSW so similarity
//! and hybrid search stay sub-linear once a space grows. `instant-distance` is the decided backend
//! per `docs/adr/0016-vector-index-backend.md` — one pure-Rust crate, no build script and no C++
//! toolchain; the spike's faster `usearch` was not taken and `sqlite-vec` was dropped outright.
//! It is built behind the `ann` Cargo feature and the store keeps it in a per-space cache
//! invalidated whenever an embedding is written; the brute-force path remains the fallback (and the
//! ground truth the parity test checks against).

use dam_api::id::AssetId;
use instant_distance::{Builder, HnswMap, Point, Search};

/// A stored embedding as an HNSW point. Vectors are persisted L2-normalised, so cosine distance is
/// `1 − dot`; the metric matches the exact path's `cosine`.
#[derive(Clone)]
struct Vec32(Vec<f32>);

impl Point for Vec32 {
    fn distance(&self, other: &Self) -> f32 {
        if self.0.len() != other.0.len() || self.0.is_empty() {
            return 2.0; // max cosine distance — mismatched/empty never ranks
        }
        let dot: f32 = self.0.iter().zip(&other.0).map(|(a, b)| a * b).sum();
        1.0 - dot
    }
}

/// An in-memory HNSW index mapping embedding vectors → asset ids for one space.
pub struct AnnIndex {
    map: HnswMap<Vec32, AssetId>,
}

impl AnnIndex {
    /// Build an index over `(id, vector)` pairs. `None` for an empty set (nothing to index).
    pub fn build(items: Vec<(AssetId, Vec<f32>)>) -> Option<Self> {
        if items.is_empty() {
            return None;
        }
        let mut points = Vec::with_capacity(items.len());
        let mut values = Vec::with_capacity(items.len());
        for (id, v) in items {
            values.push(id);
            points.push(Vec32(v));
        }
        Some(Self {
            map: Builder::default().build(points, values),
        })
    }

    /// Approximate top-`k` neighbours of `query`, returned as `(id, cosine)` nearest-first.
    pub fn nearest(&self, query: &[f32], k: usize) -> Vec<(AssetId, f32)> {
        let mut search = Search::default();
        let q = Vec32(query.to_vec());
        self.map
            .search(&q, &mut search)
            .take(k)
            .map(|item| (*item.value, 1.0 - item.distance))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::id::AssetId;

    /// The ANN top-1 agrees with an exact cosine scan on a small, well-separated set — the parity
    /// contract that lets the store swap in the index without changing results.
    #[test]
    fn ann_top1_matches_exact() {
        let ids: Vec<AssetId> = (0..5).map(|_| AssetId::new()).collect();
        let vecs = [
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
            vec![0.9, 0.1, 0.0],
            vec![0.1, 0.9, 0.0],
        ];
        let items: Vec<(AssetId, Vec<f32>)> =
            ids.iter().copied().zip(vecs.iter().cloned()).collect();
        let index = AnnIndex::build(items).unwrap();

        // Query near vec[0]: nearest should be id[0], runner-up id[3].
        let hits = index.nearest(&[0.99, 0.05, 0.0], 2);
        assert_eq!(hits[0].0, ids[0], "nearest is the aligned vector");
        assert_eq!(hits[1].0, ids[3], "runner-up is the next-closest");
    }
}
