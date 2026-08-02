//! Analysis targets, tags/suggestions, similarity and dedup — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    // ── analysis / automation (tech-spec 05, phase 3) ───────────────────────

    /// The assets an analysis pass should process: everything behind `current_version` (the incremental
    /// Plan gate, §1.2/§7.2), or `force`-all, or a specific `ids` set. Carries each asset's source
    /// *connection* so the runner can rebuild the backend and fetch bytes through it.
    ///
    /// Only **federated** sources are excluded: a peer yields catalog rows, not bytes, so there is
    /// nothing local to decode (its derived data belongs to the peer that owns it). SFTP/SMB assets
    /// are targets like any other and reach their bytes through `FileSource::fetch` — issue #48.
    /// Before that they were filtered out here by `s.kind = 'local_fs'`, which meant a remote asset
    /// was never even *planned* for analysis: no embedding, no derived signals, no auto-tags, and no
    /// error either, because nothing had gone wrong — it simply was not on the list.
    ///
    /// A row whose stored connection cannot be parsed is dropped rather than defaulted: an
    /// unparseable connection cannot be fetched from, and a target that can never succeed is worse
    /// than one that was never listed (it would fail every pass, forever, at whatever version gate).
    pub fn list_analysis_targets(
        &self,
        current_version: i64,
        force: bool,
        ids: &[AssetId],
    ) -> Result<Vec<AnalysisTarget>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT a.id, s.connection, a.path, a.media_type, a.format, a.content_hash, a.source_id,
                    s.auth_ref
             FROM asset a JOIN source s ON s.id = a.source_id
             WHERE s.kind <> 'federated'",
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
                let source_id = blob_to_source_id(&r.get::<_, Vec<u8>>(6)?);
                let auth_ref: Option<String> = r.get(7)?;
                Ok(parse_connection(&connection).ok().map(|mut connection| {
                    connection.set_credential_ref(auth_ref);
                    AnalysisTarget {
                        id,
                        source_id,
                        connection,
                        path,
                        media: MediaType::parse(&media_s).unwrap_or(MediaType::Image),
                        format,
                        content_hash: hash
                            .and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok())
                            .map(ContentHash),
                    }
                }))
            })
            .map_err(internal)?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?
            .into_iter()
            .flatten()
            .collect())
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
        // Every attr table carries the same `class` column; pick the table for the media type.
        let sql = match media {
            MediaType::Audio => "INSERT INTO audio_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Model => "INSERT INTO model_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Image => "INSERT INTO image_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Video => "INSERT INTO video_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Document => "INSERT INTO document_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
        };
        conn.execute(sql, params![key, class]).map_err(internal)?;
        Ok(())
    }

    /// Persist the continuous audio acoustic features from the analyze pass (issue #61). Upserts the
    /// `audio_attr` row (the cheap-tier metadata may not have created it yet).
    pub fn set_audio_features(
        &self,
        id: &AssetId,
        loudness_lufs: f32,
        brightness: f32,
        harmonicity: f32,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO audio_attr (asset_id, loudness_lufs, brightness, harmonicity)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(asset_id) DO UPDATE SET
                loudness_lufs=excluded.loudness_lufs, brightness=excluded.brightness,
                harmonicity=excluded.harmonicity",
            params![
                id.as_bytes().to_vec(),
                loudness_lufs,
                brightness,
                harmonicity
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Persist the server-computed inspector waveform peaks (issue #73) as a JSON `[f32,…]` string, so
    /// both clients draw the bars without decoding the audio. Upsert (the cheap-tier row exists from
    /// scan); a no-op-safe part of the analysis pass.
    pub fn set_audio_peaks(&self, id: &AssetId, peaks: &[f32]) -> Result<(), LibError> {
        let json = serde_json::to_string(peaks).map_err(internal)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO audio_attr (asset_id, waveform_peaks) VALUES (?1, ?2)
             ON CONFLICT(asset_id) DO UPDATE SET waveform_peaks=excluded.waveform_peaks",
            params![id.as_bytes().to_vec(), json],
        )
        .map_err(internal)?;
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
        // Invalidate any cached ANN index (M6): the space's vectors just changed.
        self.embed_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Drop an asset's embedding in one space. The counterpart to [`Self::set_embedding`] for the
    /// case where re-analysis produces *no* vector (a document whose new revision has no readable
    /// text): leaving the previous one indexed would keep ranking the asset on content it no longer
    /// has. A no-op when there was nothing there.
    pub fn clear_embedding(&self, id: &AssetId, space_id: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "DELETE FROM embedding WHERE asset_id = ?1 AND space_id = ?2",
                params![id.as_bytes().to_vec(), space_id],
            )
            .map_err(internal)?;
        if n > 0 {
            self.embed_gen
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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

    /// Rewrite an asset's `tags` FTS column to its current non-rejected tag names (schema V7), so tag
    /// text feeds full-text search and a rejected tag drops back out. Called after every tag mutation.
    /// Best-effort: an FTS hiccup must never sink the tag write that triggered it.
    fn reindex_asset_tags(conn: &Connection, id: &AssetId) {
        let _ = conn.execute(
            "UPDATE asset_fts SET tags = COALESCE((
                SELECT group_concat(t.name, ' ') FROM asset_tag at
                JOIN tag t ON t.id = at.tag_id
                WHERE at.asset_id = ?1 AND at.state <> 'rejected'), '')
             WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
            params![id.as_bytes().to_vec()],
        );
    }

    /// Write a document's extracted body text into the `text` FTS column (schema V10).
    ///
    /// The text is stored **only** in the index, never in a base-table column: it can be a megabyte
    /// per asset, nothing but search reads it, and keeping it out of `asset` keeps the row width
    /// (and every `SELECT *`-shaped query) unchanged. The cost of that choice is that the value has
    /// to be stashed and restored on any future FTS rebuild — exactly as `tokens` and `tags`
    /// already are, and as V10's own migration does.
    pub fn set_document_text(&self, id: &AssetId, text: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE asset_fts SET text = ?2
             WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
            params![id.as_bytes().to_vec(), text],
        )
        .map_err(internal)?;
        Ok(())
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
        Self::reindex_asset_tags(&conn, id);
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
        Self::reindex_asset_tags(&conn, id);
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
        vis: &Visibility,
    ) -> Result<(String, Vec<(AssetSummary, f32)>), LibError> {
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
            return Ok((String::new(), Vec::new())); // not embedded yet (§1.3)
        };
        let qvec = bytes_to_f32(&qbytes);
        // Over-fetch nearest neighbours (self excluded) so the facet post-filter still leaves ≥ k.
        let overfetch = (k as usize * 4).max(k as usize + 16);

        // Nearest neighbours in the space, descending cosine. The `ann` feature (M6) serves this from
        // a cached HNSW index; the default build does the exact brute-force scan (correct and the
        // ground truth the ANN parity test checks against).
        #[cfg(feature = "ann")]
        let scored: Vec<(AssetId, f32)> =
            self.ann_scored(&conn, &space_id, &qvec, id, overfetch)?;
        #[cfg(not(feature = "ann"))]
        let scored: Vec<(AssetId, f32)> = {
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
            scored
        };

        // Post-filter against the facet predicate + the visibility ceiling and fetch summaries
        // (§3.3). The ceiling applies to the *candidates* — a neighbour from an unshared source
        // leaks both its existence and its content-likeness (issue #42 leak audit).
        let candidate_ids: Vec<AssetId> = scored.iter().take(overfetch).map(|(a, _)| *a).collect();
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
        let conn = self.conn.lock().unwrap();
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
    pub fn similar_by_vector(
        &self,
        space_id: &str,
        qvec: &[f32],
        k: u32,
        filters: &[Filter],
        vis: &Visibility,
    ) -> Result<Vec<(AssetSummary, f32)>, LibError> {
        let conn = self.conn.lock().unwrap();
        let dim: Option<i64> = conn
            .query_row(
                "SELECT LENGTH(vec) / 4 FROM embedding WHERE space_id = ?1 LIMIT 1",
                params![space_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
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
        let scored = self.nearest_in_space(&conn, space_id, qvec, overfetch)?;
        let candidate_ids: Vec<AssetId> = scored.iter().map(|(a, _)| *a).collect();
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
    pub(crate) fn nearest_in_space(
        &self,
        conn: &Connection,
        space_id: &str,
        qvec: &[f32],
        k: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        #[cfg(feature = "ann")]
        {
            match self.ann_for_space(conn, space_id) {
                Ok(index) => Ok(index.nearest(qvec, k)),
                Err(_) => Ok(Vec::new()), // empty/absent space
            }
        }
        #[cfg(not(feature = "ann"))]
        {
            let mut stmt = conn
                .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![space_id], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(internal)?;
            let mut scored: Vec<(AssetId, f32)> = Vec::new();
            for r in rows {
                let (idb, vb) = r.map_err(internal)?;
                scored.push((blob_to_asset_id(&idb), cosine(qvec, &bytes_to_f32(&vb))));
            }
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(k);
            Ok(scored)
        }
    }

    /// ANN nearest neighbours of `qvec` in `space_id` (M6), self excluded, `(id, cosine)` desc.
    #[cfg(feature = "ann")]
    fn ann_scored(
        &self,
        conn: &Connection,
        space_id: &str,
        qvec: &[f32],
        self_id: &AssetId,
        k: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        let index = self.ann_for_space(conn, space_id)?;
        let mut out: Vec<(AssetId, f32)> = index
            .nearest(qvec, k + 1)
            .into_iter()
            .filter(|(id, _)| id != self_id)
            .collect();
        out.truncate(k);
        Ok(out)
    }

    /// Get (or lazily build + cache) the HNSW index for a space (M6). Rebuilt when an embedding
    /// write has bumped `embed_gen` since the cached copy. `conn` is the already-held lock.
    #[cfg(feature = "ann")]
    fn ann_for_space(
        &self,
        conn: &Connection,
        space_id: &str,
    ) -> Result<std::sync::Arc<crate::ann::AnnIndex>, LibError> {
        use std::sync::atomic::Ordering;
        let generation = self.embed_gen.load(Ordering::Relaxed);
        if let Some((g, idx)) = self.ann_cache.lock().unwrap().get(space_id) {
            if *g == generation {
                return Ok(idx.clone());
            }
        }
        // (Re)build from the space's current vectors.
        let mut stmt = conn
            .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![space_id], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(internal)?;
        let mut items: Vec<(AssetId, Vec<f32>)> = Vec::new();
        for r in rows {
            let (idb, vb) = r.map_err(internal)?;
            items.push((blob_to_asset_id(&idb), bytes_to_f32(&vb)));
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

    /// Duplicate groups for the review view (§4). `Exact` groups by content hash; `Near` groups by
    /// embedding cosine ≥ threshold within a media space (union-find over the pairwise relation, §4.3).
    pub fn duplicates(
        &self,
        req: &DupRequest,
        vis: &Visibility,
    ) -> Result<Page<DupGroup>, LibError> {
        const NEAR_COS: f32 = 0.92; // conservative "strong near-dup" band (§4.2; tuned later, §8)
        const NEAR_CANDIDATE_MAX: usize = 2_000;
        let conn = self.conn.lock().unwrap();
        let mut groups: Vec<DupGroup> = Vec::new();
        let limit = req.limit.clamp(1, DUP_GROUP_PAGE_MAX) as usize;
        let has_more;
        let mut next_cursor = None;
        let mut partial = dam_api::PartialStatus::default();

        match req.kind {
            DupKind::Exact => {
                let after = decode_exact_dup_cursor(req.after.as_ref())?;
                let mut where_sql = String::from(" WHERE content_hash IS NOT NULL");
                let mut binds: Vec<Value> = Vec::new();
                if let Some(m) = req.media {
                    where_sql.push_str(" AND media_type = ?");
                    binds.push(Value::Text(m.as_str().to_string()));
                }
                push_visibility(vis, "asset", &mut where_sql, &mut binds);
                let mut having = String::from(" HAVING n > 1");
                if let Some((count, hash)) = after {
                    having.push_str(" AND (n < ? OR (n = ? AND content_hash > ?))");
                    binds.push(Value::Integer(count as i64));
                    binds.push(Value::Integer(count as i64));
                    binds.push(Value::Blob(hash));
                }
                let sql = format!(
                    "SELECT content_hash, COUNT(*) n FROM asset {where_sql}
                     GROUP BY content_hash {having}
                     ORDER BY n DESC, content_hash ASC LIMIT ?"
                );
                binds.push(Value::Integer((limit + 1) as i64));
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u32>(1)?))
                    })
                    .map_err(internal)?;
                let mut exact: Vec<(Vec<u8>, u32)> = rows
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(internal)?;
                has_more = exact.len() > limit;
                exact.truncate(limit);
                if has_more {
                    next_cursor = exact
                        .last()
                        .map(|(hash, count)| Cursor(format!("exact:{count}:{}", encode_hex(hash))));
                }

                if !exact.is_empty() {
                    // Rank within every selected hash in SQL, then cap before summary hydration.
                    // The whole review page is hydrated by the single summaries query below.
                    let mut ranked_where = String::from(" WHERE asset.content_hash IS NOT NULL");
                    let mut ranked_binds: Vec<Value> = Vec::new();
                    push_visibility(vis, "asset", &mut ranked_where, &mut ranked_binds);
                    let hash_ph = exact.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                    let member_sql = format!(
                        "WITH ranked AS (
                           SELECT asset.id, asset.content_hash,
                                  COALESCE(asset.size_bytes, 0) member_size,
                                  ROW_NUMBER() OVER (
                                    PARTITION BY asset.content_hash
                                    ORDER BY COALESCE(asset.size_bytes, 0) DESC, asset.id ASC
                                  ) member_rank
                           FROM asset {ranked_where}
                         )
                         SELECT id, content_hash, member_size FROM ranked
                         WHERE member_rank <= ? AND content_hash IN ({hash_ph})
                         ORDER BY content_hash ASC, member_rank ASC"
                    );
                    ranked_binds.push(Value::Integer(DUP_GROUP_MEMBER_MAX as i64));
                    for (hash, _) in &exact {
                        ranked_binds.push(Value::Blob(hash.clone()));
                    }
                    let mut member_stmt = conn.prepare(&member_sql).map_err(internal)?;
                    let member_rows = member_stmt
                        .query_map(rusqlite::params_from_iter(ranked_binds.iter()), |r| {
                            Ok((
                                r.get::<_, Vec<u8>>(1)?,
                                blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?),
                                r.get::<_, i64>(2)?.max(0) as u64,
                            ))
                        })
                        .map_err(internal)?;
                    let mut ids_by_hash: std::collections::HashMap<Vec<u8>, Vec<AssetId>> =
                        std::collections::HashMap::new();
                    let mut size_by_id = std::collections::HashMap::new();
                    let mut all_ids = Vec::new();
                    for row in member_rows {
                        let (hash, id, size) = row.map_err(internal)?;
                        ids_by_hash.entry(hash).or_default().push(id);
                        size_by_id.insert(id, size);
                        all_ids.push(id);
                    }
                    let summaries = Self::summaries_for_ids(&conn, &all_ids, &[], vis)?;
                    for (hash, total_members) in exact {
                        let ids = ids_by_hash.remove(&hash).unwrap_or_default();
                        let group_key = encode_hex(&hash);
                        let members_cursor = (ids.len() < total_members as usize)
                            .then(|| member_cursor(&ids, &size_by_id))
                            .flatten();
                        if let Some(group) = Self::build_dup_group_from_summaries(
                            DupKind::Exact,
                            &ids,
                            total_members,
                            Some(group_key),
                            members_cursor,
                            "identical bytes (same content hash)",
                            &summaries,
                        ) {
                            groups.push(group);
                        }
                    }
                }
            }
            DupKind::Near => {
                let offset = decode_near_dup_cursor(req.after.as_ref(), NEAR_CANDIDATE_MAX)?;
                // Load embeddings for the requested media (or all), union-find over cosine ≥ threshold.
                let mut sql = String::from(
                    "SELECT e.asset_id, e.vec FROM embedding e JOIN asset a ON a.id = e.asset_id",
                );
                let mut where_sql = String::from(" WHERE 1=1");
                let mut binds: Vec<Value> = Vec::new();
                if let Some(m) = req.media {
                    where_sql.push_str(" AND e.media_type = ?");
                    binds.push(Value::Text(m.as_str().to_string()));
                }
                push_visibility(vis, "a", &mut where_sql, &mut binds);
                sql.push_str(&where_sql);
                sql.push_str(" ORDER BY e.asset_id LIMIT ?");
                binds.push(Value::Integer((NEAR_CANDIDATE_MAX + 1) as i64));
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
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
                if ids.len() > NEAR_CANDIDATE_MAX {
                    ids.truncate(NEAR_CANDIDATE_MAX);
                    vecs.truncate(NEAR_CANDIDATE_MAX);
                    partial.complete = false;
                    partial.warnings.push(dam_api::ItemWarning {
                        subject: "near-duplicates".into(),
                        code: "duplicate_candidates_capped".into(),
                        message: format!(
                            "near-duplicate analysis is capped at {NEAR_CANDIDATE_MAX} candidates"
                        ),
                    });
                }
                let mut uf = UnionFind::new(ids.len());
                for i in 0..vecs.len() {
                    for j in (i + 1)..vecs.len() {
                        if cosine(&vecs[i], &vecs[j]) >= NEAR_COS {
                            uf.union(i, j);
                        }
                    }
                }
                let components: Vec<Vec<usize>> = uf
                    .components()
                    .into_iter()
                    .filter(|comp| comp.len() >= 2)
                    .skip(offset)
                    .take(limit + 1)
                    .collect();
                has_more = components.len() > limit;
                let components = &components[..components.len().min(limit)];
                let all_ids: Vec<AssetId> = components
                    .iter()
                    .flat_map(|comp| {
                        comp.iter()
                            .take(DUP_GROUP_MEMBER_MAX)
                            .map(|&index| ids[index])
                    })
                    .collect();
                let summaries = Self::summaries_for_ids(&conn, &all_ids, &[], vis)?;
                for comp in components {
                    if comp.len() < 2 {
                        continue;
                    }
                    let member_ids: Vec<AssetId> = comp
                        .iter()
                        .take(DUP_GROUP_MEMBER_MAX)
                        .map(|&i| ids[i])
                        .collect();
                    if let Some(g) = Self::build_dup_group_from_summaries(
                        DupKind::Near,
                        &member_ids,
                        comp.len() as u32,
                        None,
                        None,
                        &format!("embedding cosine ≥ {NEAR_COS:.2}"),
                        &summaries,
                    ) {
                        groups.push(g);
                    }
                }
                if has_more {
                    next_cursor = Some(Cursor(format!("near:{}", offset + groups.len())));
                }
            }
        }
        let mut page = Page::new(groups, next_cursor);
        page.partial = partial;
        Ok(page)
    }

    /// Set-based exact-duplicate membership for the currently retained browse rows. The request is
    /// bounded by the service before reaching SQLite and this query returns no asset summaries.
    pub fn duplicate_membership(
        &self,
        ids: &[AssetId],
        vis: &Visibility,
    ) -> Result<Vec<DupMembership>, LibError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let requested_ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut requested_where =
            format!(" WHERE asset.content_hash IS NOT NULL AND asset.id IN ({requested_ph})");
        let mut requested_binds: Vec<Value> = ids
            .iter()
            .map(|id| Value::Blob(id.as_bytes().to_vec()))
            .collect();
        push_visibility(vis, "asset", &mut requested_where, &mut requested_binds);
        let mut count_where = String::from(" WHERE asset.content_hash IS NOT NULL");
        let mut count_binds = Vec::new();
        push_visibility(vis, "asset", &mut count_where, &mut count_binds);
        requested_binds.extend(count_binds);
        let sql = format!(
            "WITH requested AS (
               SELECT asset.id, asset.content_hash FROM asset {requested_where}
             ), counts AS (
               SELECT asset.content_hash, COUNT(*) n FROM asset
               JOIN (SELECT DISTINCT content_hash FROM requested) wanted
                 ON wanted.content_hash = asset.content_hash
               {count_where} GROUP BY asset.content_hash
             )
             SELECT requested.id, lower(hex(requested.content_hash)), counts.n
             FROM requested JOIN counts USING (content_hash) WHERE counts.n > 1"
        );
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(requested_binds.iter()), |row| {
                Ok(DupMembership {
                    asset: blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                    group: row.get(1)?,
                    count: row.get(2)?,
                })
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// One exact group for Inspector. Only one group is hydrated and its summaries are hard-capped.
    pub fn duplicate_group(
        &self,
        id: &AssetId,
        vis: &Visibility,
    ) -> Result<Option<DupGroup>, LibError> {
        let Some(membership) = self.duplicate_membership(&[*id], vis)?.into_iter().next() else {
            return Ok(None);
        };
        let hash = decode_hash_hex(&membership.group)?;
        let conn = self.conn.lock().unwrap();
        let mut where_sql = String::from(" WHERE asset.content_hash = ?");
        let mut binds = vec![Value::Blob(hash)];
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let sql = format!(
            "SELECT asset.id, COALESCE(asset.size_bytes, 0) FROM asset {where_sql}
             ORDER BY COALESCE(asset.size_bytes, 0) DESC, asset.id ASC LIMIT ?"
        );
        binds.push(Value::Integer(DUP_GROUP_MEMBER_MAX as i64));
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok((
                    blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                    row.get::<_, i64>(1)?.max(0) as u64,
                ))
            })
            .map_err(internal)?;
        let ordered = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;
        let ids: Vec<AssetId> = ordered.iter().map(|(id, _)| *id).collect();
        let members_cursor = (ids.len() < membership.count as usize)
            .then(|| {
                ordered
                    .last()
                    .map(|(id, size)| Cursor(format!("members:{size}:{id}")))
            })
            .flatten();
        let summaries = Self::summaries_for_ids(&conn, &ids, &[], vis)?;
        Ok(Self::build_dup_group_from_summaries(
            DupKind::Exact,
            &ids,
            membership.count,
            Some(membership.group),
            members_cursor,
            "identical bytes (same content hash)",
            &summaries,
        ))
    }

    /// Continue one exact group's member summaries with a stable `(size, id)` keyset.
    pub fn duplicate_group_members(
        &self,
        req: &DupGroupMembersRequest,
        vis: &Visibility,
    ) -> Result<Page<AssetSummary>, LibError> {
        let hash = decode_hash_hex(&req.group)?;
        let after = decode_dup_member_cursor(req.after.as_ref())?;
        let limit = req.limit.clamp(1, DUP_GROUP_PAGE_MAX) as usize;
        let conn = self.conn.lock().unwrap();
        let mut where_sql = String::from(" WHERE asset.content_hash = ?");
        let mut binds = vec![Value::Blob(hash)];
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        if let Some((size, id)) = after {
            where_sql.push_str(
                " AND (COALESCE(asset.size_bytes, 0) < ? OR
                       (COALESCE(asset.size_bytes, 0) = ? AND asset.id > ?))",
            );
            binds.push(Value::Integer(size as i64));
            binds.push(Value::Integer(size as i64));
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        let sql = format!(
            "SELECT asset.id, COALESCE(asset.size_bytes, 0) FROM asset {where_sql}
             ORDER BY COALESCE(asset.size_bytes, 0) DESC, asset.id ASC LIMIT ?"
        );
        binds.push(Value::Integer((limit + 1) as i64));
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok((
                    blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                    row.get::<_, i64>(1)?.max(0) as u64,
                ))
            })
            .map_err(internal)?;
        let mut ordered = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;
        let has_more = ordered.len() > limit;
        ordered.truncate(limit);
        let ids: Vec<AssetId> = ordered.iter().map(|(id, _)| *id).collect();
        let summaries = Self::summaries_for_ids(&conn, &ids, &[], vis)?;
        let items: Vec<AssetSummary> = ids
            .iter()
            .filter_map(|id| summaries.get(id).cloned())
            .collect();
        let cursor = has_more
            .then(|| {
                ordered
                    .last()
                    .map(|(id, size)| Cursor(format!("members:{size}:{id}")))
            })
            .flatten();
        Ok(Page::new(items, cursor))
    }

    /// Build a `DupGroup` from member ids: load summaries, pick the suggested keep (largest bytes,
    /// then highest pixel count for images). Skips groups that collapse to <2 resolvable members —
    /// which also re-forms visibility-filtered groups (issue #42 leak audit): a duplicate pair
    /// spanning a shared and an unshared source collapses to one visible member, and a group of one
    /// is not a duplicate, so the hidden file's existence never shows.
    fn build_dup_group_from_summaries(
        kind: DupKind,
        ids: &[AssetId],
        total_members: u32,
        group: Option<String>,
        members_cursor: Option<Cursor>,
        signal: &str,
        map: &std::collections::HashMap<AssetId, AssetSummary>,
    ) -> Option<DupGroup> {
        let mut members: Vec<AssetSummary> =
            ids.iter().filter_map(|i| map.get(i).cloned()).collect();
        if members.len() < 2 {
            return None;
        }
        // Suggested keep: the biggest file (a decent proxy for highest fidelity, §4.3).
        if kind == DupKind::Near {
            members.sort_by_key(|member| (std::cmp::Reverse(member.size), member.id));
        }
        let suggested_keep = members
            .iter()
            .max_by_key(|member| member.size)
            .expect("duplicate group has members")
            .id;
        let media = members[0].media;
        Some(DupGroup {
            kind,
            media,
            group,
            members,
            total_members,
            members_cursor,
            signal: signal.to_string(),
            suggested_keep,
        })
    }

    /// Fetch summaries for a set of ids, applying the same faceted filters as text search (§3.3)
    /// plus the visibility ceiling — the shared choke point of the similarity, hybrid-search, and
    /// dedup candidate paths, so an unreachable asset drops out of all of them in one place.
    /// Returns a map so callers can preserve their own ordering (similarity score / dup grouping).
    pub(crate) fn summaries_for_ids(
        conn: &Connection,
        ids: &[AssetId],
        filters: &[Filter],
        vis: &Visibility,
    ) -> Result<std::collections::HashMap<AssetId, AssetSummary>, LibError> {
        let mut map = std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds: Vec<Value> = Vec::new();
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
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
pub(crate) fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn decode_exact_dup_cursor(cursor: Option<&Cursor>) -> Result<Option<(u32, Vec<u8>)>, LibError> {
    let Some(Cursor(raw)) = cursor else {
        return Ok(None);
    };
    let mut parts = raw.split(':');
    let valid_prefix = parts.next() == Some("exact");
    let count = parts.next().and_then(|part| part.parse::<u32>().ok());
    let hash = parts.next().and_then(|part| decode_hash_hex(part).ok());
    if !valid_prefix
        || count.is_none()
        || hash.as_ref().is_none_or(|hash| hash.len() != 32)
        || parts.next().is_some()
    {
        return Err(LibError::BadRequest(
            "invalid exact-duplicate cursor".into(),
        ));
    }
    Ok(Some((count.unwrap(), hash.unwrap())))
}

fn decode_hash_hex(raw: &str) -> Result<Vec<u8>, LibError> {
    if raw.len() != 64 || !raw.is_ascii() {
        return Err(LibError::BadRequest("invalid duplicate hash".into()));
    }
    (0..raw.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&raw[index..index + 2], 16)
                .map_err(|_| LibError::BadRequest("invalid duplicate hash".into()))
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn member_cursor(
    ids: &[AssetId],
    sizes: &std::collections::HashMap<AssetId, u64>,
) -> Option<Cursor> {
    let id = *ids.last()?;
    let size = sizes.get(&id)?;
    Some(Cursor(format!("members:{size}:{id}")))
}

fn decode_near_dup_cursor(cursor: Option<&Cursor>, max: usize) -> Result<usize, LibError> {
    let Some(Cursor(raw)) = cursor else {
        return Ok(0);
    };
    let Some(offset) = raw
        .strip_prefix("near:")
        .and_then(|part| part.parse::<usize>().ok())
        .filter(|offset| *offset <= max)
    else {
        return Err(LibError::BadRequest("invalid near-duplicate cursor".into()));
    };
    Ok(offset)
}

fn decode_dup_member_cursor(cursor: Option<&Cursor>) -> Result<Option<(u64, AssetId)>, LibError> {
    let Some(Cursor(raw)) = cursor else {
        return Ok(None);
    };
    let mut parts = raw.split(':');
    let valid_prefix = parts.next() == Some("members");
    let size = parts
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .filter(|size| *size <= i64::MAX as u64);
    let id = parts.next().and_then(|part| part.parse::<AssetId>().ok());
    if !valid_prefix || size.is_none() || id.is_none() || parts.next().is_some() {
        return Err(LibError::BadRequest(
            "invalid duplicate-member cursor".into(),
        ));
    }
    Ok(Some((size.unwrap(), id.unwrap())))
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
        let mut components: Vec<Vec<usize>> = map.into_values().collect();
        // `HashMap` iteration is intentionally random; a cursor page needs a repeatable component
        // order for the same candidate snapshot.
        components.sort_by_key(|component| component.first().copied().unwrap_or(usize::MAX));
        components
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_sources::{FederatedConfig, SftpConfig, SourceConnection};

    fn duplicate_store(groups: usize, members_per_group: usize) -> Store {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/duplicate-test".into(),
                },
                "duplicates",
                false,
            )
            .unwrap();
        for group in 0..groups {
            let mut hash = [0_u8; 32];
            hash[..8].copy_from_slice(&(group as u64).to_be_bytes());
            for member in 0..members_per_group {
                store
                    .upsert_asset(&NewAsset {
                        source_id: source,
                        path: format!("{group}/{member}.png"),
                        filename: format!("{member}.png"),
                        content_hash: Some(ContentHash(hash)),
                        size_bytes: Some(member as i64 + 1),
                        source_modified_at: None,
                        scanned_at: now_ms(),
                        media_type: MediaType::Image,
                        format: "png".into(),
                    })
                    .unwrap();
            }
        }
        store
    }

    #[test]
    fn exact_duplicate_pages_use_a_bounded_keyset_cursor() {
        let store = duplicate_store(DUP_GROUP_PAGE_MAX as usize + 5, 2);
        let first = store
            .duplicates(
                &DupRequest {
                    kind: DupKind::Exact,
                    media: None,
                    limit: u32::MAX,
                    after: None,
                },
                &Visibility::Full,
            )
            .unwrap();
        assert_eq!(first.items.len(), DUP_GROUP_PAGE_MAX as usize);
        assert!(first.items.iter().all(|group| group.members.len() == 2));
        let cursor = first.cursor.expect("five groups remain");
        assert!(cursor.0.starts_with("exact:2:"));

        let second = store
            .duplicates(
                &DupRequest {
                    kind: DupKind::Exact,
                    media: None,
                    limit: u32::MAX,
                    after: Some(cursor),
                },
                &Visibility::Full,
            )
            .unwrap();
        assert_eq!(second.items.len(), 5);
        assert!(second.cursor.is_none());

        let invalid = store.duplicates(
            &DupRequest {
                kind: DupKind::Exact,
                media: None,
                limit: 10,
                after: Some(Cursor("999999999999999999999".into())),
            },
            &Visibility::Full,
        );
        assert!(matches!(invalid, Err(LibError::BadRequest(_))));
    }

    #[test]
    fn duplicate_group_hydration_has_a_hard_member_cap() {
        let store = duplicate_store(1, DUP_GROUP_MEMBER_MAX + 7);
        let id = {
            let conn = store.conn.lock().unwrap();
            conn.query_row("SELECT id FROM asset LIMIT 1", [], |row| {
                Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
            })
            .unwrap()
        };
        let membership = store
            .duplicate_membership(&[id], &Visibility::Full)
            .unwrap();
        let group = store
            .duplicate_group(&membership[0].asset, &Visibility::Full)
            .unwrap()
            .unwrap();
        assert_eq!(group.total_members as usize, DUP_GROUP_MEMBER_MAX + 7);
        assert_eq!(group.members.len(), DUP_GROUP_MEMBER_MAX);
        let group_key = group.group.clone().unwrap();
        let mut seen: Vec<AssetId> = group.members.iter().map(|member| member.id).collect();
        let mut cursor = group.members_cursor;
        while let Some(after) = cursor {
            let page = store
                .duplicate_group_members(
                    &DupGroupMembersRequest {
                        group: group_key.clone(),
                        after: Some(after),
                        limit: 17,
                    },
                    &Visibility::Full,
                )
                .unwrap();
            seen.extend(page.items.iter().map(|member| member.id));
            cursor = page.cursor;
        }
        let expected: Vec<AssetId> = {
            let conn = store.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM asset
                     ORDER BY COALESCE(size_bytes, 0) DESC, id ASC",
                )
                .unwrap();
            stmt.query_map([], |row| Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            seen, expected,
            "member pages must have no gaps or duplicates"
        );
        let overflowing_cursor = store.duplicate_group_members(
            &DupGroupMembersRequest {
                group: group_key,
                after: Some(Cursor(format!("members:{}:{id}", u64::MAX))),
                limit: 10,
            },
            &Visibility::Full,
        );
        assert!(matches!(overflowing_cursor, Err(LibError::BadRequest(_))));
    }

    /// Manual scale benchmark for issue #142. Even with ten thousand groups the returned page and
    /// hydrated summaries remain fixed-size; run with `cargo test -p dam-store -- --ignored`.
    #[test]
    #[ignore = "large-catalog duplicate benchmark"]
    fn duplicate_page_large_catalog_benchmark() {
        let store = duplicate_store(10_000, 2);
        let started = std::time::Instant::now();
        let page = store
            .duplicates(&DupRequest::default(), &Visibility::Full)
            .unwrap();
        eprintln!("10k duplicate groups: {:?}", started.elapsed());
        assert_eq!(page.items.len(), 24);
        assert!(page.cursor.is_some());
        assert!(
            page.items
                .iter()
                .map(|group| group.members.len())
                .sum::<usize>()
                <= 24 * DUP_GROUP_MEMBER_MAX
        );
    }

    fn sftp_conn() -> SourceConnection {
        SourceConnection::Sftp(SftpConfig {
            host: "example.invalid".into(),
            port: 22,
            username: "u".into(),
            base_path: "/assets".into(),
            password: Some("p".into()),
            private_key: None,
            passphrase: None,
            credential_ref: None,
        })
    }

    /// Add one source and one asset on it; return the store and the asset id.
    fn store_with_asset_on(conn: &SourceConnection, kind_name: &str) -> (Store, AssetId) {
        let store = Store::open_in_memory().unwrap();
        let mut stored = conn.clone();
        let _ = stored.take_credentials();
        let src = store.add_source(&stored, kind_name, false).unwrap();
        let (id, _) = store
            .upsert_asset(&NewAsset {
                source_id: src,
                path: "textures/brick.png".into(),
                filename: "brick.png".into(),
                content_hash: None,
                size_bytes: Some(1),
                source_modified_at: None,
                scanned_at: now_ms(),
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        (store, id)
    }

    /// The bug behind issue #48: `WHERE s.kind = 'local_fs'` meant an SFTP/SMB asset was never even
    /// *planned* for analysis. It produced no error, because nothing failed — the asset simply was
    /// not on the list, so it sat at `analysis_version = 0` forever with no embedding, no derived
    /// signals, and no auto-tags, while the job it should have been part of reported success.
    #[test]
    fn remote_assets_are_planned_for_analysis() {
        let (store, id) = store_with_asset_on(&sftp_conn(), "remote");
        let targets = store.list_analysis_targets(1, false, &[]).unwrap();
        assert_eq!(targets.len(), 1, "an SFTP asset must be an analysis target");
        assert_eq!(targets[0].id, id);
        assert!(
            matches!(targets[0].connection, SourceConnection::Sftp(_)),
            "the target carries the connection the runner fetches through"
        );
    }

    /// The one exclusion that must survive: a federated peer yields catalog rows, not bytes. There
    /// is nothing local to decode, and its derived data belongs to the peer that owns it — so
    /// widening the filter for SFTP/SMB must not accidentally sweep peers back in.
    #[test]
    fn federated_assets_are_never_analysis_targets() {
        let (store, _) = store_with_asset_on(
            &SourceConnection::Federated(FederatedConfig {
                endpoint: "http://peer.invalid:7878".into(),
                token: None,
                credential_ref: None,
            }),
            "peer",
        );
        assert!(
            store
                .list_analysis_targets(1, false, &[])
                .unwrap()
                .is_empty(),
            "a federated asset has no local bytes and must not be planned"
        );
    }

    /// A row whose stored connection blob cannot be parsed is dropped, not defaulted. A target that
    /// can never be fetched would fail every pass forever; leaving it off the list is the honest
    /// outcome, and keeps the warning count meaningful.
    #[test]
    fn an_unparseable_connection_drops_the_target() {
        let (store, _) = store_with_asset_on(&sftp_conn(), "remote");
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("UPDATE source SET connection = 'not json'", [])
                .unwrap();
        }
        assert!(
            store
                .list_analysis_targets(1, false, &[])
                .unwrap()
                .is_empty(),
            "an unfetchable target must not be planned"
        );
    }
}
