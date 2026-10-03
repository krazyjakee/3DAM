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
            if let Some(ids) = self.ann_candidate_ids(space_id, qvec, k)? {
                return self.exact_rank_candidates(space_id, qvec, &ids, k);
            }
            // Startup/corruption recovery has no published base yet. Exact scan is a correctness
            // fallback only; the lifecycle worker is already rebuilding outside this request.
            self.exact_nearest_in_space(space_id, qvec, k)
        }
        #[cfg(not(feature = "ann"))]
        {
            self.exact_nearest_in_space(space_id, qvec, k)
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
        let mut out: Vec<(AssetId, f32)> = self
            .nearest_in_space(space_id, qvec, k + 1)?
            .into_iter()
            .filter(|(id, _)| id != self_id)
            .collect();
        out.truncate(k);
        Ok(out)
    }

    /// Bounded approximate candidates from the published base plus durable incremental upserts.
    /// `None` means there is no safe base yet (startup/recovery/oversized overlay), so the caller
    /// uses exact fallback. No index is ever built on this path.
    #[cfg(feature = "ann")]
    pub(crate) fn ann_candidate_ids(
        &self,
        space_id: &str,
        qvec: &[f32],
        requested: usize,
    ) -> Result<Option<Vec<AssetId>>, LibError> {
        let conn = self.read()?;
        self.ann_candidate_ids_in(&conn, space_id, qvec, requested)
    }

    #[cfg(feature = "ann")]
    pub(crate) fn ann_candidate_ids_in(
        &self,
        conn: &Connection,
        space_id: &str,
        qvec: &[f32],
        requested: usize,
    ) -> Result<Option<Vec<AssetId>>, LibError> {
        const ANN_CANDIDATE_MAX: usize = 8_192;
        let Some(manager) = &self.ann else {
            return Ok(None);
        };
        let Some(base) = manager.cached(space_id) else {
            manager.kick();
            return Ok(None);
        };
        let mut overlay = Vec::new();
        let mut indexed_generation = None;
        // Publication commits the new base generation and removes its journal before swapping the
        // process cache. The state row and bounded journal are deliberately read by one statement:
        // two autocommit SELECTs on the same connection would still observe separate SQLite
        // snapshots and could combine an old cached base with an already-cleared new journal.
        let mut stmt = conn
            .prepare(
                "SELECT state.indexed_generation, delta.asset_id, delta.operation
                   FROM (SELECT indexed_generation FROM ann_space_state WHERE space_id=?1) state
                   LEFT JOIN (
                     SELECT asset_id, operation FROM ann_delta
                      WHERE space_id=?1 AND generation>?2
                      ORDER BY generation DESC LIMIT ?3
                   ) delta ON TRUE",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(
                params![
                    space_id,
                    base.generation,
                    (crate::ann::OVERLAY_CANDIDATE_MAX + 1) as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .map_err(internal)?;
        for row in rows {
            let (generation, id, operation) = row.map_err(internal)?;
            indexed_generation = Some(generation);
            if let (Some(id), Some(operation)) = (id, operation) {
                overlay.push((blob_to_asset_id(&id), operation));
            }
        }
        if indexed_generation != Some(base.generation) {
            manager.kick();
            return Ok(None);
        }
        if overlay.len() > crate::ann::OVERLAY_CANDIDATE_MAX {
            manager.kick();
            return Ok(None);
        }
        // Every changed base point can consume one approximate slot at its stale location (updates
        // and tombstones alike). Search past all of them, then append live upserts explicitly.
        let budget = requested
            .saturating_mul(8)
            .max(64)
            .saturating_add(overlay.len())
            .min(ANN_CANDIDATE_MAX);
        let mut ids = match base.index.as_ref() {
            Some(index) => match index.candidates(qvec, budget) {
                Ok(ids) => ids,
                Err(error) => {
                    tracing::warn!(space_id, %error, "ANN lookup failed; evicting base for exact fallback");
                    manager.evict(space_id);
                    return Ok(None);
                }
            },
            None => Vec::new(),
        };
        ids.extend(
            overlay
                .into_iter()
                .filter_map(|(id, operation)| (operation == "upsert").then_some(id)),
        );
        ids.sort_unstable();
        ids.dedup();
        Ok(Some(ids))
    }

    fn exact_nearest_in_space(
        &self,
        space_id: &str,
        qvec: &[f32],
        k: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let conn = self.read()?;
            let mut stmt = conn
                .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
                .map_err(internal)?;
            let mapped = stmt
                .query_map(params![space_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(internal)?;
            for row in mapped {
                rows.push(row.map_err(internal)?);
            }
        }
        let mut scored: Vec<_> = rows
            .into_iter()
            .map(|(id, vector)| (blob_to_asset_id(&id), cosine(qvec, &bytes_to_f32(&vector))))
            .collect();
        sort_scored(&mut scored);
        scored.truncate(k);
        Ok(scored)
    }

    #[cfg(feature = "ann")]
    fn exact_rank_candidates(
        &self,
        space_id: &str,
        qvec: &[f32],
        ids: &[AssetId],
        k: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        let mut vectors = Vec::new();
        for chunk in ids.chunks(400) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT asset_id, vec FROM embedding WHERE space_id=? AND asset_id IN ({placeholders})"
            );
            let mut values = vec![Value::Text(space_id.to_string())];
            values.extend(chunk.iter().map(|id| Value::Blob(id.as_bytes().to_vec())));
            let conn = self.read()?;
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(internal)?;
            for row in rows {
                vectors.push(row.map_err(internal)?);
            }
        }
        let mut scored: Vec<_> = vectors
            .into_iter()
            .map(|(id, vector)| (blob_to_asset_id(&id), cosine(qvec, &bytes_to_f32(&vector))))
            .collect();
        sort_scored(&mut scored);
        scored.truncate(k);
        Ok(scored)
    }
}

fn sort_scored(scored: &mut [(AssetId, f32)]) {
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
}

/// Decode a little-endian f32 blob (an embedding row's `vec`).
pub(crate) fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
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
