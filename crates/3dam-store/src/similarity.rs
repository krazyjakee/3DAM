//! Vector similarity (tech-spec 05 §3): cosine/ANN nearest-neighbour search over an embedding
//! space, for a reference asset or an arbitrary query vector. Part of the `Store` impl, plus the
//! two vector primitives (`bytes_to_f32`, `cosine`) every reader of an `embedding.vec` blob shares.
use super::*;
use crate::helpers::*;

impl Store {
    /// Cosine-nearest neighbours of `id` within its media's embedding space (§3.2). Brute-force exact
    /// scan over the space (v1; HNSW is the scale follow-up, §3.1). Facet `filters` are post-applied
    /// (§3.3). Returns `(summary, score)` sorted by descending cosine, self dropped, capped at `k`.
    ///
    /// **Two snapshots, not one** (issue #137 step 5). Cosine-scoring a whole embedding space — or
    /// building its HNSW index — is CPU work, and a read guard pins a WAL snapshot for as long as it
    /// lives, so a slow scan here would hold the write-ahead log open against a concurrent scan. The
    /// vectors are therefore read under one guard, scored with none held, and the surviving
    /// summaries hydrated under a second. A neighbour deleted between the two simply drops out of
    /// the page: this was always a best-effort view of a moving catalog (the vectors themselves are
    /// written by a background analyse pass), so the extra window costs nothing the caller could
    /// have relied on.
    pub fn similar(
        &self,
        id: &AssetId,
        k: u32,
        filters: &[Filter],
        vis: &Visibility,
    ) -> Result<(String, Vec<(AssetSummary, f32)>), LibError> {
        // Query vector + its space. Scoped, so the connection is back in the pool before any scoring.
        let query: Option<(String, Vec<u8>)> = {
            let conn = self.read()?;
            conn.query_row(
                "SELECT space_id, vec FROM embedding WHERE asset_id = ?1",
                params![id.as_bytes().to_vec()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?
        };
        let Some((space_id, qbytes)) = query else {
            return Ok((String::new(), Vec::new())); // not embedded yet (§1.3)
        };
        let qvec = bytes_to_f32(&qbytes);
        // Over-fetch nearest neighbours (self excluded) so the facet post-filter still leaves ≥ k.
        let overfetch = (k as usize * 4).max(k as usize + 16);

        // Nearest neighbours in the space, descending cosine. The `ann` feature (M6) serves this from
        // a cached HNSW index; the default build does the exact brute-force scan (correct and the
        // ground truth the ANN parity test checks against). Neither may run under a guard — see the
        // doc comment — so both take their own, briefly, and give it back before they compute.
        #[cfg(feature = "ann")]
        let scored: Vec<(AssetId, f32)> = self.ann_scored(&space_id, &qvec, id, overfetch)?;
        #[cfg(not(feature = "ann"))]
        let scored: Vec<(AssetId, f32)> = {
            // Materialise the space first: `cosine` over every vector must not run on the connection.
            let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            {
                let conn = self.read()?;
                let mut stmt = conn
                    .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
                    .map_err(internal)?;
                let mapped = stmt
                    .query_map(params![space_id], |r| {
                        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                    })
                    .map_err(internal)?;
                for r in mapped {
                    rows.push(r.map_err(internal)?);
                }
            }
            let self_blob = id.as_bytes().to_vec();
            let mut scored: Vec<(AssetId, f32)> = Vec::new();
            for (id_blob, vbytes) in rows {
                if id_blob == self_blob {
                    continue;
                }
                let score = cosine(&qvec, &bytes_to_f32(&vbytes));
                scored.push((blob_to_asset_id(&id_blob), score));
            }
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored
        };

        // Post-filter against the facet predicate + the visibility ceiling and fetch summaries
        // (§3.3). The ceiling applies to the *candidates* — a neighbour from an unshared source
        // leaks both its existence and its content-likeness (issue #42 leak audit).
        let candidate_ids: Vec<AssetId> = scored.iter().take(overfetch).map(|(a, _)| *a).collect();
        let conn = self.read()?;
        let summaries = Self::summaries_for_ids(&conn, &candidate_ids, filters, vis)?;
        let mut out = Vec::new();
        for (aid, score) in scored {
            if out.len() >= k as usize {
                break;
            }
            if let Some(sum) = summaries.get(&aid) {
                out.push((sum.clone(), score));
            }
        }
        Ok((space_id, out))
    }

    /// The stored embedding for one asset — `(space_id, vector)`, or `None` when not yet analysed.
    /// The federated fan-out ships this vector to matched-space peers (phase 6, issue #40).
    pub fn embedding_for(&self, id: &AssetId) -> Result<Option<(String, Vec<f32>)>, LibError> {
        let conn = self.read()?;
        let row: Option<(String, Vec<u8>)> = conn
            .query_row(
                "SELECT space_id, vec FROM embedding WHERE asset_id = ?1",
                params![id.as_bytes().to_vec()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        Ok(row.map(|(s, v)| (s, bytes_to_f32(&v))))
    }

    /// Cosine-nearest neighbours of an arbitrary query vector in `space_id` — the serving side of
    /// cross-peer similarity (phase 6, issue #40). A space this catalog has no vectors in — or a
    /// dimension mismatch — is a `BadRequest`, not a silent empty page, so the caller's exact-match
    /// space gate stays honest. Same over-fetch + facet post-filter as [`Self::similar`]; no
    /// self-drop (the query vector has no local identity).
    ///
    /// Observes more than one snapshot, for the reason spelled out on [`Self::similar`]: the dim
    /// probe, the neighbour scan, and the summary hydration each take their own read guard so the
    /// cosine/HNSW work between them runs off the connection.
    pub fn similar_by_vector(
        &self,
        space_id: &str,
        qvec: &[f32],
        k: u32,
        filters: &[Filter],
        vis: &Visibility,
    ) -> Result<Vec<(AssetSummary, f32)>, LibError> {
        let dim: Option<i64> = {
            let conn = self.read()?;
            conn.query_row(
                "SELECT LENGTH(vec) / 4 FROM embedding WHERE space_id = ?1 LIMIT 1",
                params![space_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?
        };
        match dim {
            None => {
                return Err(LibError::BadRequest(format!(
                    "embedding space {space_id:?} is not served by this catalog"
                )))
            }
            Some(d) if d as usize != qvec.len() => {
                return Err(LibError::BadRequest(format!(
                    "query vector has {} dims; space {space_id:?} has {d}",
                    qvec.len()
                )))
            }
            Some(_) => {}
        }
        let overfetch = (k as usize * 4).max(k as usize + 16);
        let scored = self.nearest_in_space(space_id, qvec, overfetch)?;
        let candidate_ids: Vec<AssetId> = scored.iter().map(|(a, _)| *a).collect();
        let conn = self.read()?;
        let summaries = Self::summaries_for_ids(&conn, &candidate_ids, filters, vis)?;
        let mut out = Vec::new();
        for (aid, score) in scored {
            if out.len() >= k as usize {
                break;
            }
            if let Some(sum) = summaries.get(&aid) {
                out.push((sum.clone(), score));
            }
        }
        Ok(out)
    }

    /// Nearest neighbours of an arbitrary query vector in `space_id`, `(id, cosine)` desc, capped at
    /// `k`. This is the text→asset entry point (semantic-search M4/M5): the engine encodes a query
    /// string into the model's shared space, then this finds the closest assets — no reference asset
    /// needed. Uses the ANN index under the `ann` feature, else an exact scan. Empty space → empty.
    ///
    /// **Checks out its own read connection, and must not be called while one is held** — it is the
    /// scoring step that issue #137 step 5 moved off the connection, and the debug guard-depth check
    /// panics on a nested acquisition. It therefore observes its own snapshot, independent of
    /// whatever the caller reads before or after; see [`Self::similar`] for why that is acceptable.
    pub(crate) fn nearest_in_space(
        &self,
        space_id: &str,
        qvec: &[f32],
        k: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        #[cfg(feature = "ann")]
        {
            match self.ann_for_space(space_id) {
                Ok(index) => Ok(index.nearest(qvec, k)),
                Err(_) => Ok(Vec::new()), // empty/absent space
            }
        }
        #[cfg(not(feature = "ann"))]
        {
            // Read the space, hand the connection back, *then* score it.
            let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            {
                let conn = self.read()?;
                let mut stmt = conn
                    .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
                    .map_err(internal)?;
                let mapped = stmt
                    .query_map(params![space_id], |r| {
                        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                    })
                    .map_err(internal)?;
                for r in mapped {
                    rows.push(r.map_err(internal)?);
                }
            }
            let mut scored: Vec<(AssetId, f32)> = Vec::new();
            for (idb, vb) in rows {
                scored.push((blob_to_asset_id(&idb), cosine(qvec, &bytes_to_f32(&vb))));
            }
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(k);
            Ok(scored)
        }
    }

    /// ANN nearest neighbours of `qvec` in `space_id` (M6), self excluded, `(id, cosine)` desc.
    /// Like [`Self::ann_for_space`], call this with no read guard held.
    #[cfg(feature = "ann")]
    fn ann_scored(
        &self,
        space_id: &str,
        qvec: &[f32],
        self_id: &AssetId,
        k: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        let index = self.ann_for_space(space_id)?;
        let mut out: Vec<(AssetId, f32)> = index
            .nearest(qvec, k + 1)
            .into_iter()
            .filter(|(id, _)| id != self_id)
            .collect();
        out.truncate(k);
        Ok(out)
    }

    /// Get (or lazily build + cache) the HNSW index for a space (M6). Rebuilt when an embedding
    /// write has bumped `embed_gen` since the cached copy.
    ///
    /// **Checks out its own read connection, so no caller may hold one** (the debug guard-depth
    /// check panics otherwise, and on an in-memory store a nested acquisition would hang). Building
    /// an HNSW over a whole space is the single heaviest CPU step in the store, and a read guard
    /// pins a WAL snapshot for its whole lifetime — a cold build under the caller's guard would keep
    /// the write-ahead log growing for the duration (issue #137 step 5). So the SELECT runs under a
    /// guard of its own, that guard is dropped, and `AnnIndex::build` runs against the owned copy.
    /// The index therefore reflects the snapshot at SELECT time, not the caller's.
    #[cfg(feature = "ann")]
    fn ann_for_space(
        &self,
        space_id: &str,
    ) -> Result<std::sync::Arc<crate::ann::AnnIndex>, LibError> {
        use std::sync::atomic::Ordering;
        let generation = self.embed_gen.load(Ordering::Relaxed);
        if let Some((g, idx)) = self.ann_cache.lock().unwrap().get(space_id) {
            if *g == generation {
                return Ok(idx.clone());
            }
        }
        // (Re)build from the space's current vectors, off the connection.
        let mut items: Vec<(AssetId, Vec<f32>)> = Vec::new();
        {
            let conn = self.read()?;
            let mut stmt = conn
                .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![space_id], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(internal)?;
            for r in rows {
                let (idb, vb) = r.map_err(internal)?;
                items.push((blob_to_asset_id(&idb), bytes_to_f32(&vb)));
            }
        }
        let idx = std::sync::Arc::new(
            crate::ann::AnnIndex::build(items)
                .ok_or_else(|| LibError::Internal("empty embedding space".into()))?,
        );
        self.ann_cache
            .lock()
            .unwrap()
            .insert(space_id.to_string(), (generation, idx.clone()));
        Ok(idx)
    }
}

/// Decode a little-endian f32 blob (an embedding row's `vec`).
pub(crate) fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity. Vectors are stored L2-normalised, so this is a dot product; we still divide by
/// the norms defensively in case a legacy/zero vector slips in.
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}
