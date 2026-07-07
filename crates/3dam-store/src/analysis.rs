//! Analysis targets, tags/suggestions, similarity and dedup — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    // ── analysis / automation (tech-spec 05, phase 3) ───────────────────────

    /// The assets an analysis pass should process: everything behind `current_version` (the incremental
    /// Plan gate, §1.2/§7.2), or `force`-all, or a specific `ids` set. Joins the source so the runner can
    /// resolve each file. Skips offline/federated sources (no bytes to decode).
    pub fn list_analysis_targets(
        &self,
        current_version: i64,
        force: bool,
        ids: &[AssetId],
    ) -> Result<Vec<AnalysisTarget>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT a.id, s.connection, a.path, a.media_type, a.format, a.content_hash
             FROM asset a JOIN source s ON s.id = a.source_id
             WHERE s.kind = 'local_fs'",
        );
        if !force {
            sql.push_str(&format!(" AND a.analysis_version < {current_version}"));
        }
        let mut binds: Vec<Value> = Vec::new();
        if !ids.is_empty() {
            let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND a.id IN ({ph})"));
            for id in ids {
                binds.push(Value::Blob(id.as_bytes().to_vec()));
            }
        }
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                let id = blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?);
                let connection: String = r.get(1)?;
                let path: String = r.get(2)?;
                let media_s: String = r.get(3)?;
                let format: String = r.get(4)?;
                let hash: Option<Vec<u8>> = r.get(5)?;
                // local_fs display URI is the (canonical) source root the analyzer joins onto.
                let source_uri = parse_connection(&connection)
                    .map(|c| c.display_uri())
                    .unwrap_or_default();
                Ok(AnalysisTarget {
                    id,
                    source_uri,
                    path,
                    media: MediaType::parse(&media_s).unwrap_or(MediaType::Image),
                    format,
                    content_hash: hash
                        .and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok())
                        .map(ContentHash),
                })
            })
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(internal)?);
        }
        Ok(out)
    }

    /// Persist the derived image signals (§5, §6) into the existing `image_attr` row. The row is created
    /// at scan (cheap tier), so this is an UPDATE; if absent (e.g. a directly-analysed asset), upsert.
    pub fn set_image_analysis(&self, id: &AssetId, a: &ImageAnalysis) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let key = id.as_bytes().to_vec();
        let phash_blob = a.phash.to_le_bytes().to_vec();
        let colors = serde_json::to_string(&a.dominant_colors).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT INTO image_attr (asset_id, phash, tileability, repeat_period, tile_class, dominant_colors, class)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(asset_id) DO UPDATE SET
                phash=excluded.phash, tileability=excluded.tileability,
                repeat_period=excluded.repeat_period, tile_class=excluded.tile_class,
                dominant_colors=excluded.dominant_colors, class=excluded.class",
            params![key, phash_blob, a.tileability as f64, a.repeat_period, a.tile_class, colors, a.class],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Persist an auto-category guess onto an audio/model attr row (§4, §5).
    pub fn set_media_class(
        &self,
        id: &AssetId,
        media: MediaType,
        class: &str,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let key = id.as_bytes().to_vec();
        // Column set is identical across the three attr tables; pick the table for the media type.
        let sql = match media {
            MediaType::Audio => "INSERT INTO audio_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Model => "INSERT INTO model_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Image => "INSERT INTO image_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
        };
        conn.execute(sql, params![key, class]).map_err(internal)?;
        Ok(())
    }

    /// Upsert an asset's embedding for one space (§2.1, §3.1). `vec` must already be L2-normalised.
    pub fn set_embedding(
        &self,
        id: &AssetId,
        space_id: &str,
        media: MediaType,
        vec: &[f32],
        extractor: &str,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let mut bytes = Vec::with_capacity(vec.len() * 4);
        for f in vec {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        conn.execute(
            "INSERT INTO embedding (asset_id, space_id, media_type, dim, vec, extractor, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(asset_id, space_id) DO UPDATE SET
                dim=excluded.dim, vec=excluded.vec, extractor=excluded.extractor, created_at=excluded.created_at",
            params![
                id.as_bytes().to_vec(),
                space_id,
                media.as_str(),
                vec.len() as i64,
                bytes,
                extractor,
                now_ms(),
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Record that an asset is now analysed at `version` (the Plan gate reads this, §7.2).
    pub fn mark_analysed(&self, id: &AssetId, version: i64) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE asset SET analysis_version = ?2, analysed_at = ?3, updated_at = ?3 WHERE id = ?1",
            params![id.as_bytes().to_vec(), version, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    // ── tags / suggestions (§1.4) ─────────────────────────────────────────────

    pub(crate) fn load_tags(conn: &Connection, id_blob: &[u8]) -> Vec<TagRef> {
        let mut stmt = match conn.prepare(
            "SELECT t.name, at.state, at.source, at.confidence
             FROM asset_tag at JOIN tag t ON t.id = at.tag_id
             WHERE at.asset_id = ?1 ORDER BY at.state, t.name",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![id_blob], |r| {
            Ok(TagRef {
                name: r.get(0)?,
                state: r.get(1)?,
                source: r.get(2)?,
                confidence: r.get::<_, Option<f64>>(3)?.map(|v| v as f32),
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Intern a tag name, returning its id (case-insensitive unique).
    fn intern_tag(conn: &Connection, name: &str) -> Result<Vec<u8>, LibError> {
        if let Some(id) = conn
            .query_row(
                "SELECT id FROM tag WHERE name = ?1 COLLATE NOCASE",
                params![name],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(internal)?
        {
            return Ok(id);
        }
        let id = Uuid::now_v7();
        conn.execute(
            "INSERT INTO tag (id, name) VALUES (?1, ?2)",
            params![id.as_bytes().to_vec(), name],
        )
        .map_err(internal)?;
        Ok(id.as_bytes().to_vec())
    }

    /// Add an auto-suggested tag (§1.4). No-op if the asset already carries this tag in *any* state —
    /// a prior reject stays rejected (re-analysis must not re-suggest), a confirmed stays confirmed.
    pub fn suggest_tag(
        &self,
        id: &AssetId,
        name: &str,
        confidence: f32,
        extractor: &str,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let tag_id = Self::intern_tag(&conn, name)?;
        conn.execute(
            "INSERT INTO asset_tag (asset_id, tag_id, state, source, confidence, extractor, created_at)
             VALUES (?1, ?2, 'suggested', 'auto', ?3, ?4, ?5)
             ON CONFLICT(asset_id, tag_id) DO NOTHING",
            params![id.as_bytes().to_vec(), tag_id, confidence as f64, extractor, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Accept (`confirmed`) or reject (`rejected`) a suggested tag by name (§1.4). Reversible.
    pub fn set_tag_state(&self, id: &AssetId, name: &str, state: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let tag_id = Self::intern_tag(&conn, name)?;
        let n = conn
            .execute(
                "UPDATE asset_tag SET state = ?3 WHERE asset_id = ?1 AND tag_id = ?2",
                params![id.as_bytes().to_vec(), tag_id, state],
            )
            .map_err(internal)?;
        if n == 0 {
            // No prior suggestion (e.g. a user confirming a tag directly): create it as user-sourced.
            conn.execute(
                "INSERT INTO asset_tag (asset_id, tag_id, state, source, created_at)
                 VALUES (?1, ?2, ?3, 'user', ?4)
                 ON CONFLICT(asset_id, tag_id) DO UPDATE SET state = excluded.state",
                params![id.as_bytes().to_vec(), tag_id, state, now_ms()],
            )
            .map_err(internal)?;
        }
        Ok(())
    }

    // ── similarity + dedup (§3, §4) ───────────────────────────────────────────

    /// Cosine-nearest neighbours of `id` within its media's embedding space (§3.2). Brute-force exact
    /// scan over the space (v1; HNSW is the scale follow-up, §3.1). Facet `filters` are post-applied
    /// (§3.3). Returns `(summary, score)` sorted by descending cosine, self dropped, capped at `k`.
    pub fn similar(
        &self,
        id: &AssetId,
        k: u32,
        filters: &[Filter],
    ) -> Result<Vec<(AssetSummary, f32)>, LibError> {
        let conn = self.conn.lock().unwrap();
        // Query vector + its space.
        let query: Option<(String, Vec<u8>)> = conn
            .query_row(
                "SELECT space_id, vec FROM embedding WHERE asset_id = ?1",
                params![id.as_bytes().to_vec()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        let Some((space_id, qbytes)) = query else {
            return Ok(Vec::new()); // not embedded yet (§1.3)
        };
        let qvec = bytes_to_f32(&qbytes);

        // Score every other vector in the same space.
        let mut stmt = conn
            .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
            .map_err(internal)?;
        let self_blob = id.as_bytes().to_vec();
        let rows = stmt
            .query_map(params![space_id], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(internal)?;
        let mut scored: Vec<(AssetId, f32)> = Vec::new();
        for r in rows {
            let (id_blob, vbytes) = r.map_err(internal)?;
            if id_blob == self_blob {
                continue;
            }
            let score = cosine(&qvec, &bytes_to_f32(&vbytes));
            scored.push((blob_to_asset_id(&id_blob), score));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Over-fetch, then post-filter against the facet predicate and fetch summaries (§3.3).
        let overfetch = (k as usize * 4).max(k as usize + 16);
        let candidate_ids: Vec<AssetId> = scored.iter().take(overfetch).map(|(a, _)| *a).collect();
        let summaries = Self::summaries_for_ids(&conn, &candidate_ids, filters)?;
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

    /// Duplicate groups for the review view (§4). `Exact` groups by content hash; `Near` groups by
    /// embedding cosine ≥ threshold within a media space (union-find over the pairwise relation, §4.3).
    pub fn duplicates(&self, req: &DupRequest) -> Result<Vec<DupGroup>, LibError> {
        const NEAR_COS: f32 = 0.92; // conservative "strong near-dup" band (§4.2; tuned later, §8)
        let conn = self.conn.lock().unwrap();
        let mut groups: Vec<DupGroup> = Vec::new();

        match req.kind {
            DupKind::Exact => {
                let mut media_pred = String::new();
                if let Some(m) = req.media {
                    media_pred = format!(" AND media_type = '{}'", m.as_str());
                }
                let sql = format!(
                    "SELECT lower(hex(content_hash)) h, group_concat(lower(hex(id))) ids, COUNT(*) n
                     FROM asset WHERE content_hash IS NOT NULL{media_pred}
                     GROUP BY content_hash HAVING n > 1 ORDER BY n DESC LIMIT {}",
                    req.limit
                );
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(1)?,)))
                    .map_err(internal)?;
                for r in rows {
                    let (ids_csv,) = r.map_err(internal)?;
                    let ids = parse_hex_ids(&ids_csv);
                    if let Some(g) = Self::build_dup_group(
                        &conn,
                        DupKind::Exact,
                        &ids,
                        "identical bytes (same content hash)",
                    )? {
                        groups.push(g);
                    }
                }
            }
            DupKind::Near => {
                // Load embeddings for the requested media (or all), union-find over cosine ≥ threshold.
                let mut sql = String::from(
                    "SELECT e.asset_id, e.vec FROM embedding e JOIN asset a ON a.id = e.asset_id",
                );
                if let Some(m) = req.media {
                    sql.push_str(&format!(" WHERE e.media_type = '{}'", m.as_str()));
                }
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                    })
                    .map_err(internal)?;
                let mut ids: Vec<AssetId> = Vec::new();
                let mut vecs: Vec<Vec<f32>> = Vec::new();
                for r in rows {
                    let (id_blob, vbytes) = r.map_err(internal)?;
                    ids.push(blob_to_asset_id(&id_blob));
                    vecs.push(bytes_to_f32(&vbytes));
                }
                let mut uf = UnionFind::new(ids.len());
                for i in 0..vecs.len() {
                    for j in (i + 1)..vecs.len() {
                        if cosine(&vecs[i], &vecs[j]) >= NEAR_COS {
                            uf.union(i, j);
                        }
                    }
                }
                for comp in uf.components() {
                    if comp.len() < 2 {
                        continue;
                    }
                    if groups.len() >= req.limit as usize {
                        break;
                    }
                    let member_ids: Vec<AssetId> = comp.iter().map(|&i| ids[i]).collect();
                    if let Some(g) = Self::build_dup_group(
                        &conn,
                        DupKind::Near,
                        &member_ids,
                        &format!("embedding cosine ≥ {NEAR_COS:.2}"),
                    )? {
                        groups.push(g);
                    }
                }
            }
        }
        Ok(groups)
    }

    /// Build a `DupGroup` from member ids: load summaries, pick the suggested keep (largest bytes,
    /// then highest pixel count for images). Skips groups that collapse to <2 resolvable members.
    fn build_dup_group(
        conn: &Connection,
        kind: DupKind,
        ids: &[AssetId],
        signal: &str,
    ) -> Result<Option<DupGroup>, LibError> {
        let map = Self::summaries_for_ids(conn, ids, &[])?;
        let mut members: Vec<AssetSummary> =
            ids.iter().filter_map(|i| map.get(i).cloned()).collect();
        if members.len() < 2 {
            return Ok(None);
        }
        // Suggested keep: the biggest file (a decent proxy for highest fidelity, §4.3).
        members.sort_by_key(|b| std::cmp::Reverse(b.size));
        let suggested_keep = members[0].id;
        let media = members[0].media;
        Ok(Some(DupGroup {
            kind,
            media,
            members,
            signal: signal.to_string(),
            suggested_keep,
        }))
    }

    /// Fetch summaries for a set of ids, applying the same faceted filters as text search (§3.3).
    /// Returns a map so callers can preserve their own ordering (similarity score / dup grouping).
    fn summaries_for_ids(
        conn: &Connection,
        ids: &[AssetId],
        filters: &[Filter],
    ) -> Result<std::collections::HashMap<AssetId, AssetSummary>, LibError> {
        let mut map = std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds: Vec<Value> = Vec::new();
        for f in filters {
            apply_filter(f, &mut where_sql, &mut binds)?;
        }
        let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        where_sql.push_str(&format!(" AND asset.id IN ({ph})"));
        for id in ids {
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        let sql = format!("{GRID_SELECT} FROM asset {ATTR_JOINS} {where_sql}");
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), row_to_summary)
            .map_err(internal)?;
        for r in rows {
            let s = r.map_err(internal)?;
            map.insert(s.id, s);
        }
        Ok(map)
    }
}

/// Decode a little-endian f32 blob (an embedding row's `vec`).
fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity. Vectors are stored L2-normalised, so this is a dot product; we still divide by
/// the norms defensively in case a legacy/zero vector slips in.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
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

/// Parse a `group_concat(lower(hex(id)))` CSV of 32-hex-char UUIDs back into ids.
fn parse_hex_ids(csv: &str) -> Vec<AssetId> {
    csv.split(',')
        .filter_map(|h| {
            let bytes = (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            (bytes.len() == 16).then(|| blob_to_asset_id(&bytes))
        })
        .collect()
}

/// Tiny union-find for near-dup connected components (§4.3).
struct UnionFind {
    parent: Vec<usize>,
}
impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
    fn components(&mut self) -> Vec<Vec<usize>> {
        let mut map: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for i in 0..self.parent.len() {
            let root = self.find(i);
            map.entry(root).or_default().push(i);
        }
        map.into_values().collect()
    }
}
