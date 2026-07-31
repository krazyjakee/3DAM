//! Faceted query, id enumeration, and library stats — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    /// Faceted query → a page of summaries. Cursor is an offset (slice-simple; keyset later). The
    /// caller always names a ceiling — there is deliberately no `Full`-forwarding convenience
    /// wrapper, because such a wrapper is exactly how a read path forgets to filter (issue #42).
    /// The engine's entry point is this, with `ctx.visibility`.
    ///
    /// The engine may also inject a pre-computed text-query embedding
    /// (`(space_id, vector)`) for the query string (semantic-search M4/M5). When present and the mode
    /// is Hybrid/Semantic, it seeds a true text→asset ranked list in the fusion — the model-backed
    /// path that finds assets with zero lexical overlap. `None` (the default, and any build without a
    /// semantic model) leaves the model-free lexical + neighbour behaviour unchanged.
    pub fn query_assets_semantic(
        &self,
        req: &QueryRequest,
        text_vec: Option<(String, Vec<f32>)>,
        vis: &Visibility,
    ) -> Result<Page<AssetSummary>, LibError> {
        let limit = req.page.clamped(QUERY_MAX_LIMIT);
        let offset = decode_offset(req.page.after.as_ref())?;

        // Semantic-search M5: a Hybrid/Semantic query with text widens the lexical hits with their
        // embedding neighbours. Falls back to the lexical path below when there's no text.
        if req.mode != SearchMode::Lexical && req.text.as_ref().is_some_and(|t| !t.is_empty()) {
            return self.query_hybrid(req, limit, offset, text_vec, vis);
        }

        let (where_sql, binds) = build_where(req, &self.synonyms, vis)?;

        let dir = match req.sort.dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };
        // ORDER BY, plus any binds it needs (only relevance, which references the search term). A
        // relevance sort without a text query has nothing to rank, so it degrades to name order.
        let rank_text = req.text.as_ref().filter(|t| !t.is_empty());
        let rank_match = rank_text.and_then(|t| crate::search::fts_match_expr(t, &self.synonyms));
        let (order_clause, order_binds): (String, Vec<Value>) = match req.sort.field {
            SortField::Relevance if rank_match.is_some() => (
                // FTS relevance (M1), two-level. First a categorical tier so a name/tag match
                // always beats a document-body-only match (`AUTHORED_TIER` explains why a bm25
                // weight can't do this); then bm25 within the tier (more negative = better). Rows
                // matched only by the LIKE fallback have no bm25 row and COALESCE to a large
                // sentinel so they sort last. Name length/name break ties.
                format!(
                    "{tier}, \
                     COALESCE((SELECT {rank} FROM asset_fts \
                        WHERE asset_fts.rowid = asset.rowid AND asset_fts MATCH ?), 1e9) ASC, \
                        LENGTH(filename) ASC, filename ASC",
                    tier = crate::search::AUTHORED_TIER,
                    rank = crate::search::FTS_RANK
                ),
                {
                    let m = rank_match.unwrap();
                    // Bind order follows the clause: the tier's authored-scoped MATCH, then bm25's.
                    vec![
                        Value::Text(crate::search::authored_scoped(&m)),
                        Value::Text(m),
                    ]
                },
            ),
            SortField::Relevance if rank_text.is_some() => (
                // Text present but unindexable (punctuation-only) — the old substring proxy.
                "INSTR(LOWER(filename), LOWER(?)) ASC, LENGTH(filename) ASC, filename ASC".into(),
                vec![Value::Text(rank_text.unwrap().clone())],
            ),
            SortField::Relevance | SortField::Name => {
                (format!("filename {dir}, asset.id ASC"), Vec::new())
            }
            SortField::Size => (
                // Sort on the whole-asset size (mesh + external companion files), matching what the
                // grid shows; COALESCE keeps non-models (no model_attr row) on their own size.
                format!(
                    "(size_bytes + COALESCE(model_attr.dependency_bytes, 0)) {dir}, asset.id ASC"
                ),
                Vec::new(),
            ),
            SortField::Scanned => (format!("scanned_at {dir}, asset.id ASC"), Vec::new()),
        };

        let conn = self.conn.lock().unwrap();

        // Total for this filter (best-effort; cheap enough at slice scale).
        let count_sql = format!("SELECT COUNT(*) FROM asset{where_sql}");
        let total: i64 = conn
            .query_row(&count_sql, rusqlite::params_from_iter(binds.iter()), |r| {
                r.get(0)
            })
            .map_err(internal)?;

        // LEFT JOIN the per-type attr tables so each grid row carries a couple of cheap key
        // attributes (dimensions / duration / triangles) without an N+1 fetch. Column names stay
        // unambiguous across the joined tables, so the bare-name filters above keep working.
        let sql = format!(
            "{GRID_SELECT} FROM asset {ATTR_JOINS} {where_sql} ORDER BY {order_clause} LIMIT ? OFFSET ?"
        );
        // Bind order is positional across the whole statement: WHERE binds, then the ORDER BY term,
        // then LIMIT/OFFSET.
        let mut page_binds = binds.clone();
        page_binds.extend(order_binds);
        page_binds.push(Value::Integer(limit as i64));
        page_binds.push(Value::Integer(offset as i64));

        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(page_binds.iter()),
                row_to_summary,
            )
            .map_err(internal)?;
        let items = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;

        let next = if (offset + items.len()) < total as usize {
            Some(Cursor((offset + items.len()).to_string()))
        } else {
            None
        };
        Ok(Page {
            items,
            cursor: next,
            total: Some(total as u64),
            partial: Default::default(),
        })
    }

    /// Hybrid/semantic text search (M5): reciprocal-rank fusion of the lexical (FTS) hit list with
    /// the embedding neighbourhood of its top hits. `Hybrid` weights both equally; `Semantic` leans
    /// on the embedding neighbours (lexical still lightly counts, so a strong name match never
    /// vanishes). Filters apply to the whole fused set, so a neighbour that fails a facet is dropped.
    /// With no embeddings present this returns exactly the lexical page.
    fn query_hybrid(
        &self,
        req: &QueryRequest,
        limit: u32,
        offset: usize,
        text_vec: Option<(String, Vec<f32>)>,
        vis: &Visibility,
    ) -> Result<Page<AssetSummary>, LibError> {
        use std::collections::HashMap;
        // Fusion knobs. RRF k=60 is the usual default; the caps bound the fusion work per query.
        const RRF_K: f64 = 60.0;
        const LEX_CAP: usize = 400; // lexical candidates gathered
        const SEM_SEEDS: usize = 24; // top lexical hits that seed the embedding expansion
        const SEM_CAP: usize = 400; // neighbours kept from the expansion

        let conn = self.conn.lock().unwrap();

        // 1. Lexical candidates, best-first by bm25 (the M1 ranking), bounded.
        let (where_sql, mut binds) = build_where(req, &self.synonyms, vis)?;
        let rank_match = req
            .text
            .as_ref()
            .and_then(|t| crate::search::fts_match_expr(t, &self.synonyms));
        let order = if let Some(m) = rank_match {
            // Same two-level ranking as the lexical page (tier, then weighted bm25) so the
            // candidate list the fusion starts from is ordered the same way the user would see it.
            binds.push(Value::Text(crate::search::authored_scoped(&m)));
            binds.push(Value::Text(m));
            format!(
                "{tier}, \
                 COALESCE((SELECT {rank} FROM asset_fts \
                    WHERE asset_fts.rowid = asset.rowid AND asset_fts MATCH ?), 1e9) ASC, \
                    LENGTH(filename) ASC, filename ASC",
                tier = crate::search::AUTHORED_TIER,
                rank = crate::search::FTS_RANK
            )
        } else {
            "filename ASC, asset.id ASC".to_string()
        };
        let sql = format!(
            "SELECT asset.id FROM asset {ATTR_JOINS} {where_sql} ORDER BY {order} LIMIT {LEX_CAP}"
        );
        let lex_ids: Vec<AssetId> = {
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    Ok(blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?))
                })
                .map_err(internal)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(internal)?
        };

        // 2. Expand: embedding neighbours of the top lexical seeds (best cosine per neighbour).
        let seeds: Vec<AssetId> = lex_ids.iter().take(SEM_SEEDS).copied().collect();
        let neighbours = Self::semantic_neighbours(&conn, &seeds, SEM_CAP)?;

        // 2b. Model-backed text→asset list (M4): assets nearest to the encoded *query text* in the
        //     model's shared space — the ones a pure name search would miss entirely. Empty unless a
        //     semantic model produced `text_vec`.
        let text_hits: Vec<(AssetId, f32)> = match &text_vec {
            Some((space, qv)) => self.nearest_in_space(&conn, space, qv, SEM_CAP)?,
            None => Vec::new(),
        };

        // 3. Reciprocal-rank fuse the ranked lists. `w_text` leads in Semantic mode when the model is
        //    present; the model-free seed-neighbours (`w_sem`) still contribute.
        let (w_lex, w_sem, w_text) = match req.mode {
            SearchMode::Semantic => (0.3_f64, 0.7_f64, 1.5_f64),
            _ => (1.0_f64, 1.0_f64, 1.0_f64), // Hybrid
        };
        let mut fused: HashMap<AssetId, f64> = HashMap::new();
        for (rank, id) in lex_ids.iter().enumerate() {
            *fused.entry(*id).or_default() += w_lex / (RRF_K + rank as f64 + 1.0);
        }
        for (rank, (id, _score)) in neighbours.iter().enumerate() {
            *fused.entry(*id).or_default() += w_sem / (RRF_K + rank as f64 + 1.0);
        }
        for (rank, (id, _score)) in text_hits.iter().enumerate() {
            *fused.entry(*id).or_default() += w_text / (RRF_K + rank as f64 + 1.0);
        }

        // 4. Apply facet filters + the visibility ceiling + fetch summaries for the whole candidate
        //    set (the embedding expansion is space-wide, so an unshared neighbour must drop here),
        //    then order by fused score (name breaks ties) and paginate the survivors.
        let candidate_ids: Vec<AssetId> = fused.keys().copied().collect();
        let summaries = Self::summaries_for_ids(&conn, &candidate_ids, &req.filters, vis)?;
        let mut ranked: Vec<(AssetId, f64)> = fused
            .into_iter()
            .filter(|(id, _)| summaries.contains_key(id))
            .collect();
        // Stable, deterministic order: fused score desc, then name asc for ties.
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let na = summaries.get(&a.0).map(|s| s.name.as_str()).unwrap_or("");
                    let nb = summaries.get(&b.0).map(|s| s.name.as_str()).unwrap_or("");
                    na.cmp(nb)
                })
        });

        let total = ranked.len();
        let items: Vec<AssetSummary> = ranked
            .into_iter()
            .skip(offset)
            .take(limit as usize)
            .filter_map(|(id, _)| summaries.get(&id).cloned())
            .collect();
        let next = if (offset + items.len()) < total {
            Some(Cursor((offset + items.len()).to_string()))
        } else {
            None
        };
        Ok(Page {
            items,
            cursor: next,
            total: Some(total as u64),
            partial: Default::default(),
        })
    }

    /// Best-cosine embedding neighbours of a set of seed assets, across each seed's own space,
    /// excluding the seeds themselves. Scans each involved space once; returns `(id, best_cosine)`
    /// sorted by descending cosine and capped at `cap`. The scale follow-up (M6) swaps this linear
    /// scan for an ANN index behind the same signature.
    fn semantic_neighbours(
        conn: &Connection,
        seeds: &[AssetId],
        cap: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        use std::collections::{HashMap, HashSet};
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        let seed_set: HashSet<AssetId> = seeds.iter().copied().collect();
        // Seed vectors grouped by embedding space.
        let mut seed_vecs: HashMap<String, Vec<Vec<f32>>> = HashMap::new();
        {
            let ph = seeds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("SELECT space_id, vec FROM embedding WHERE asset_id IN ({ph})");
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let binds: Vec<Value> = seeds
                .iter()
                .map(|s| Value::Blob(s.as_bytes().to_vec()))
                .collect();
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(internal)?;
            for r in rows {
                let (space, vb) = r.map_err(internal)?;
                seed_vecs
                    .entry(space)
                    .or_default()
                    .push(crate::analysis::bytes_to_f32(&vb));
            }
        }
        if seed_vecs.is_empty() {
            return Ok(Vec::new()); // seeds not embedded yet
        }
        // Score every vector in each involved space against the best-matching seed in that space.
        let mut best: HashMap<AssetId, f32> = HashMap::new();
        for (space, svecs) in &seed_vecs {
            let mut stmt = conn
                .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![space], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(internal)?;
            for r in rows {
                let (idb, vb) = r.map_err(internal)?;
                let id = blob_to_asset_id(&idb);
                if seed_set.contains(&id) {
                    continue;
                }
                let v = crate::analysis::bytes_to_f32(&vb);
                let score = svecs
                    .iter()
                    .map(|sv| crate::analysis::cosine(sv, &v))
                    .fold(f32::MIN, f32::max);
                let e = best.entry(id).or_insert(f32::MIN);
                if score > *e {
                    *e = score;
                }
            }
        }
        let mut out: Vec<(AssetId, f32)> = best.into_iter().collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(cap);
        Ok(out)
    }

    /// Every asset id matching a query's text + filters, ordered by name — the unbounded id set an
    /// export or smart-folder resolution walks (no pagination). Ignores `page`/`sort`/`facets`.
    pub fn query_asset_ids(
        &self,
        req: &QueryRequest,
        vis: &Visibility,
    ) -> Result<Vec<AssetId>, LibError> {
        let (where_sql, binds) = build_where(req, &self.synonyms, vis)?;
        let conn = self.conn.lock().unwrap();
        let sql =
            format!("SELECT asset.id FROM asset {ATTR_JOINS} {where_sql} ORDER BY filename ASC, asset.id ASC");
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                Ok(blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// All member ids of a collection (unbounded), newest-added first.
    pub fn collection_member_ids(&self, id: &CollectionId) -> Result<Vec<AssetId>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT asset_id FROM collection_member WHERE collection_id = ?1 ORDER BY added_at DESC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![id.as_bytes().to_vec()], |r| {
                Ok(blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// Library aggregates, optionally scoped to one source (`source = None` ⇒ whole library).
    /// The scoped form powers per-source sidebar counts; a federated source never reaches here —
    /// the engine proxies its stats to the peer instead.
    ///
    /// Every aggregate composes the visibility ceiling: totals, media/source breakdowns, and the tag
    /// vocabulary are all oracles for hidden content (issue #42 leak audit), so each counts only the
    /// caller-reachable assets, and `by_source` never names an unreachable source.
    pub fn stats(
        &self,
        source: Option<&SourceId>,
        vis: &Visibility,
    ) -> Result<LibraryStats, LibError> {
        let conn = self.conn.lock().unwrap();

        // The shared per-asset predicate: optional source scope + the visibility ceiling. Built once
        // per alias (the aggregates below reference the asset table as `asset` or `a`).
        let asset_pred = |alias: &str| -> (String, Vec<Value>) {
            let mut sql = String::new();
            let mut binds: Vec<Value> = Vec::new();
            if let Some(sid) = source {
                sql.push_str(&format!(" AND {alias}.source_id = ?"));
                binds.push(Value::Blob(sid.as_bytes().to_vec()));
            }
            push_visibility(vis, alias, &mut sql, &mut binds);
            (sql, binds)
        };

        let count_where = |extra: &str| -> Result<i64, LibError> {
            let (pred, binds) = asset_pred("asset");
            let sql = format!("SELECT COUNT(*) FROM asset WHERE 1=1{extra}{pred}");
            conn.query_row(&sql, rusqlite::params_from_iter(binds.iter()), |r| r.get(0))
                .map_err(internal)
        };
        let total = count_where("")?;
        let unanalyzed = count_where(" AND analysed_at IS NULL")?;

        // Source count: only sources the caller can reach at all. A collection-only grant does not
        // surface its members' sources here — the collection, not the source, is what was shared.
        let sources: i64 = match (source, vis.restricted()) {
            (Some(_), _) => 1,
            (None, None) => conn
                .query_row("SELECT COUNT(*) FROM source", [], |r| r.get(0))
                .map_err(internal)?,
            (None, Some(scope)) => scope.sources.len() as i64,
        };

        let mut by_media = CountMap::new();
        {
            let (pred, binds) = asset_pred("asset");
            let sql = format!(
                "SELECT media_type, COUNT(*) FROM asset WHERE 1=1{pred} GROUP BY media_type"
            );
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(internal)?;
            for r in rows {
                let (k, v) = r.map_err(internal)?;
                by_media.insert(k, v as u64);
            }
        }
        let mut by_source = CountMap::new();
        {
            // Restrict the *source rows themselves* — an unreachable source's name must not appear
            // (even with a zero count). The joined-asset predicate then counts reachable assets.
            let mut src_pred = String::new();
            let mut binds: Vec<Value> = Vec::new();
            if let Some(sid) = source {
                src_pred.push_str(" AND s.id = ?");
                binds.push(Value::Blob(sid.as_bytes().to_vec()));
            }
            if let Some(scope) = vis.restricted() {
                if scope.sources.is_empty() {
                    src_pred.push_str(" AND 0=1");
                } else {
                    let ph = scope
                        .sources
                        .iter()
                        .map(|_| "?")
                        .collect::<Vec<_>>()
                        .join(",");
                    src_pred.push_str(&format!(" AND s.id IN ({ph})"));
                    for sid in &scope.sources {
                        binds.push(Value::Blob(sid.as_bytes().to_vec()));
                    }
                }
            }
            let sql = format!(
                "SELECT s.name, COUNT(a.id) FROM source s
                 LEFT JOIN asset a ON a.source_id = s.id
                 WHERE 1=1{src_pred} GROUP BY s.id"
            );
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(internal)?;
            for r in rows {
                let (k, v) = r.map_err(internal)?;
                by_source.insert(k, v as u64);
            }
        }
        // The most-used confirmed tags — the vocabulary behind the Tags filter facet. Picked by
        // frequency (top N) so the sidebar shows the useful few, not the whole long tail; the
        // BTreeMap then presents them alphabetically. Suggested/rejected tags are excluded — this is
        // the curated set, mirroring what the inspector confirms. Visibility applies: a tag that
        // exists only on hidden assets must not appear (issue #42 leak audit).
        let mut tags = CountMap::new();
        {
            let (pred, binds) = asset_pred("a");
            let sql = format!(
                "SELECT t.name, COUNT(*) AS n FROM asset_tag at
                 JOIN tag t ON t.id = at.tag_id
                 JOIN asset a ON a.id = at.asset_id
                 WHERE at.state = 'confirmed'{pred}
                 GROUP BY at.tag_id ORDER BY n DESC, t.name ASC LIMIT 30"
            );
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(internal)?;
            for r in rows {
                let (k, v) = r.map_err(internal)?;
                tags.insert(k, v as u64);
            }
        }
        Ok(LibraryStats {
            total: total as u64,
            by_media,
            by_source,
            tags,
            unanalyzed: unanalyzed as u64,
            sources: sources as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_sources::SourceConnection;

    /// Unrestricted query — the test-local stand-in for the `Full`-forwarding wrapper that
    /// production code deliberately no longer has (every real caller names its ceiling).
    fn query_all(store: &Store, req: &QueryRequest) -> Result<Page<AssetSummary>, LibError> {
        store.query_assets_semantic(req, None, &Visibility::Full)
    }

    /// Insert one image asset with the given filename and return the live store.
    fn store_with(filename: &str) -> Store {
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        store
            .upsert_asset(&NewAsset {
                source_id: src,
                path: filename.to_string(),
                filename: filename.to_string(),
                content_hash: None,
                size_bytes: Some(1),
                source_modified_at: None,
                scanned_at: now_ms(),
                media_type: MediaType::Image,
                format: "fbx".into(),
            })
            .unwrap();
        store
    }

    /// The ranking risk the video/document epic (#79) called out: document body text joins the same
    /// FTS index as filenames, so without per-column bm25 weights a long document that merely
    /// *mentions* a word buries the file actually named after it. Asserts the weighting works.
    #[test]
    fn filename_outranks_document_body_text() {
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        let add = |filename: &str, media: MediaType, format: &str| {
            store
                .upsert_asset(&NewAsset {
                    source_id: src,
                    path: filename.to_string(),
                    filename: filename.to_string(),
                    content_hash: None,
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: media,
                    format: format.into(),
                })
                .unwrap()
                .0
        };
        let sound = add("kick.wav", MediaType::Audio, "wav");
        let doc = add("design-notes.md", MediaType::Document, "md");

        // The document says "kick" repeatedly; the audio file merely *is* kick.wav.
        let body = "kick ".repeat(200) + "and other percussion design notes";
        store.set_document_text(&doc, &body).unwrap();

        let page = query_all(
            &store,
            &QueryRequest {
                text: Some("kick".into()),
                sort: Sort {
                    field: SortField::Relevance,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        let ids: Vec<_> = page.items.iter().map(|a| a.id).collect();
        assert!(
            ids.contains(&doc),
            "the document must still be findable by its text — the tier reorders, it never filters"
        );
        assert_eq!(
            ids.first(),
            Some(&sound),
            "the file named `kick.wav` must outrank a document that merely mentions kick 200 times"
        );

        // The other half of the bargain: a word that appears *only* in a document's body is still
        // a hit. Full-text search over documents is the whole point of putting the text in the
        // index — demoting it below filenames must not become suppressing it.
        let page = query_all(
            &store,
            &QueryRequest {
                text: Some("percussion".into()),
                sort: Sort {
                    field: SortField::Relevance,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let ids: Vec<_> = page.items.iter().map(|a| a.id).collect();
        assert_eq!(
            ids,
            vec![doc],
            "a term only present in document body text must still match that document"
        );
    }

    /// Favourites (issue #63): the flag round-trips through the `flags` bitset, the `favorite` facet
    /// filters to just the starred assets, and un-starring clears it — all without a schema change.
    #[test]
    fn favorite_flag_round_trips_and_filters() {
        use dam_api::dto::{FacetField, Filter, FilterOp, FilterValue};
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        let mk = |name: &str| {
            store
                .upsert_asset(&NewAsset {
                    source_id: src,
                    path: name.to_string(),
                    filename: name.to_string(),
                    content_hash: None,
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: MediaType::Image,
                    format: "png".into(),
                })
                .unwrap()
                .0
        };
        let a = mk("keep.png");
        let _b = mk("other.png");

        // Fresh assets are not favourites.
        let all = query_all(&store, &QueryRequest::default()).unwrap().items;
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|s| !s.favorite));

        // Star one; the favourites-only filter returns exactly it, marked favourite.
        store.set_favorite(&a, true).unwrap();
        let fav_req = QueryRequest {
            filters: vec![Filter {
                field: FacetField::Favorite,
                op: FilterOp::Eq,
                value: FilterValue::Bool(true),
            }],
            ..Default::default()
        };
        let favs = query_all(&store, &fav_req).unwrap().items;
        assert_eq!(favs.len(), 1);
        assert_eq!(favs[0].name, "keep.png");
        assert!(favs[0].favorite);

        // Un-star; the filter is empty again.
        store.set_favorite(&a, false).unwrap();
        assert!(query_all(&store, &fav_req).unwrap().items.is_empty());
    }

    /// Folder navigation (issue #66): the on-the-fly tree derived from stored paths, and the
    /// path-prefix filter that scopes a query to a subtree.
    #[test]
    fn folder_tree_and_path_filter() {
        use dam_api::dto::{FacetField, Filter, FilterOp, FilterValue};
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        let mk = |path: &str| {
            store
                .upsert_asset(&NewAsset {
                    source_id: src,
                    path: path.to_string(),
                    filename: path.rsplit('/').next().unwrap().to_string(),
                    content_hash: None,
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: MediaType::Image,
                    format: "png".into(),
                })
                .unwrap();
        };
        mk("Environment/Rock/cliff.png");
        mk("Environment/Rock/boulder.png");
        mk("Environment/Tree/oak.png");
        mk("Characters/hero.png");
        mk("readme.png"); // a file at the root, not a folder

        let names = |v: Vec<dam_api::dto::FolderEntry>| {
            v.into_iter()
                .map(|f| (f.name, f.asset_count))
                .collect::<Vec<_>>()
        };
        // Root: two folders with whole-subtree counts; the root file is not a folder. NOCASE-sorted.
        assert_eq!(
            names(store.list_folders(&src, "").unwrap()),
            vec![("Characters".into(), 1), ("Environment".into(), 3)]
        );
        // One level down.
        assert_eq!(
            names(store.list_folders(&src, "Environment/").unwrap()),
            vec![("Rock".into(), 2), ("Tree".into(), 1)]
        );
        // A leaf folder has no subfolders.
        assert!(store
            .list_folders(&src, "Environment/Rock/")
            .unwrap()
            .is_empty());

        // The path-prefix filter scopes a query to a subtree; an empty prefix is a no-op.
        let scoped = |prefix: &str| {
            let req = QueryRequest {
                filters: vec![Filter {
                    field: FacetField::Path,
                    op: FilterOp::Eq,
                    value: FilterValue::Str(prefix.into()),
                }],
                ..Default::default()
            };
            query_all(&store, &req).unwrap().total.unwrap()
        };
        assert_eq!(scoped("Environment/"), 3);
        assert_eq!(scoped("Environment/Rock/"), 2);
        assert_eq!(scoped(""), 5);
    }

    fn search(store: &Store, text: &str) -> Vec<String> {
        search_mode(store, text, SearchMode::Lexical)
    }

    fn search_mode(store: &Store, text: &str, mode: SearchMode) -> Vec<String> {
        let req = QueryRequest {
            text: Some(text.into()),
            mode,
            ..Default::default()
        };
        query_all(store, &req)
            .unwrap()
            .items
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// M1: an FTS token match finds the asset by a whole filename word (not just a substring).
    #[test]
    fn fts_matches_filename_token() {
        let store = store_with("ak47_lowpoly.fbx");
        assert_eq!(search(&store, "lowpoly"), vec!["ak47_lowpoly.fbx"]);
        assert_eq!(search(&store, "ak47"), vec!["ak47_lowpoly.fbx"]);
    }

    /// M3: a synonym widens the query — searching "gun" finds a file named only "ak47".
    #[test]
    fn synonym_finds_related_asset() {
        let store = store_with("ak47_lowpoly.fbx");
        assert_eq!(search(&store, "gun"), vec!["ak47_lowpoly.fbx"]);
        assert_eq!(search(&store, "weapon"), vec!["ak47_lowpoly.fbx"]);
        // An unrelated term must not match.
        assert!(search(&store, "piano").is_empty());
    }

    /// V7: an auto-tag makes an asset findable by text even when the filename never mentions it, and
    /// rejecting the tag drops it back out of the index — the reject-only lifecycle, end to end.
    #[test]
    fn tag_name_is_searchable_and_reject_removes_it() {
        let store = store_with("clip_0001.wav");
        let id = query_all(&store, &QueryRequest::default()).unwrap().items[0].id;
        // A term that appears only as a tag, never in the filename.
        assert!(search(&store, "snare").is_empty());
        store.suggest_tag(&id, "snare", 0.9, "test@1").unwrap();
        assert_eq!(search(&store, "snare"), vec!["clip_0001.wav"]);
        // Rejecting hides it from search again; restoring (confirm) brings it back.
        store.set_tag_state(&id, "snare", "rejected").unwrap();
        assert!(search(&store, "snare").is_empty());
        store.set_tag_state(&id, "snare", "confirmed").unwrap();
        assert_eq!(search(&store, "snare"), vec!["clip_0001.wav"]);
    }

    /// Advanced Search: a typed structured-attribute filter round-trips against the stored columns —
    /// a numeric range (sample rate) and a boolean toggle (has_rig) each return exactly the matching
    /// asset. Proves the new `attr_num_filter`/`attr_bool_filter` arms filter, not just parse.
    #[test]
    fn structured_attribute_filters_select_matching_assets() {
        use dam_api::dto::{
            AudioAttributes, FacetField, Filter, FilterOp, FilterValue, MediaAttributes,
            ModelAttributes,
        };
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        let mk = |name: &str, media: MediaType| {
            store
                .upsert_asset(&NewAsset {
                    source_id: src,
                    path: name.to_string(),
                    filename: name.to_string(),
                    content_hash: None,
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: media,
                    format: "x".into(),
                })
                .unwrap()
                .0
        };
        // Two audio clips at different sample rates.
        let hi = mk("hi.wav", MediaType::Audio);
        let lo = mk("lo.wav", MediaType::Audio);
        store
            .set_media_attrs(
                &hi,
                &MediaAttributes::Audio(AudioAttributes {
                    sample_rate: Some(48_000),
                    ..Default::default()
                }),
            )
            .unwrap();
        store
            .set_media_attrs(
                &lo,
                &MediaAttributes::Audio(AudioAttributes {
                    sample_rate: Some(22_050),
                    ..Default::default()
                }),
            )
            .unwrap();
        // Two models, one rigged.
        let rigged = mk("rigged.glb", MediaType::Model);
        let _static_mesh = mk("static.glb", MediaType::Model);
        store
            .set_media_attrs(
                &rigged,
                &MediaAttributes::Model(ModelAttributes {
                    has_rig: Some(true),
                    ..Default::default()
                }),
            )
            .unwrap();

        let filtered = |field, op, value| {
            let req = QueryRequest {
                filters: vec![Filter { field, op, value }],
                ..Default::default()
            };
            let mut names = query_all(&store, &req)
                .unwrap()
                .items
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>();
            names.sort();
            names
        };

        // Sample-rate range 44.1k–96k catches only the 48k clip.
        assert_eq!(
            filtered(
                FacetField::SampleRate,
                FilterOp::Range,
                FilterValue::Range(44_100.0, 96_000.0)
            ),
            vec!["hi.wav"]
        );
        // has_rig = true catches only the rigged model.
        assert_eq!(
            filtered(FacetField::HasRig, FilterOp::Eq, FilterValue::Bool(true)),
            vec!["rigged.glb"]
        );
    }

    /// M2: the tokens column carries camelCase splits, so "poly" (a sub-token of "LowPoly") matches.
    #[test]
    fn tokens_column_carries_camelcase_splits() {
        let store = store_with("BrickWall_02.png");
        assert_eq!(search(&store, "brick"), vec!["BrickWall_02.png"]);
        assert_eq!(search(&store, "wall"), vec!["BrickWall_02.png"]);
    }

    /// M5: a Hybrid query pulls in the embedding neighbours of the lexical hit — a file that shares
    /// no query word but sits next to it in the embedding space. Lexical mode returns only the name
    /// match, proving the broadening is the hybrid path's doing.
    #[test]
    fn hybrid_broadens_via_embeddings() {
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        let mk = |name: &str| {
            store
                .upsert_asset(&NewAsset {
                    source_id: src,
                    path: name.to_string(),
                    filename: name.to_string(),
                    content_hash: None,
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: MediaType::Audio,
                    format: "wav".into(),
                })
                .unwrap()
                .0
        };
        let a = mk("kick_drum.wav");
        let b = mk("punchy_transient.wav"); // shares no word with "kick"
        store
            .set_embedding(
                &a,
                "audio-stats-v1",
                MediaType::Audio,
                &[1.0, 0.0, 0.0],
                "t@1",
            )
            .unwrap();
        store
            .set_embedding(
                &b,
                "audio-stats-v1",
                MediaType::Audio,
                &[0.98, 0.2, 0.0],
                "t@1",
            )
            .unwrap();

        let lex = search_mode(&store, "kick", SearchMode::Lexical);
        assert_eq!(lex, vec!["kick_drum.wav"], "lexical is name-only");

        let hy = search_mode(&store, "kick", SearchMode::Hybrid);
        assert!(
            hy.contains(&"kick_drum.wav".to_string()),
            "keeps lexical hit: {hy:?}"
        );
        assert!(
            hy.contains(&"punchy_transient.wav".to_string()),
            "hybrid pulled the embedding neighbour: {hy:?}"
        );
    }

    /// M4: the model-backed text→asset path. A Semantic query whose text matches *no* filename still
    /// finds the asset nearest to the injected query-text vector (what SigLIP's `encode_text` yields).
    /// Exercises the full fusion wiring without needing real model weights.
    #[test]
    fn semantic_text_vector_finds_asset_without_lexical_match() {
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        let mk = |name: &str| {
            store
                .upsert_asset(&NewAsset {
                    source_id: src,
                    path: name.to_string(),
                    filename: name.to_string(),
                    content_hash: None,
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: MediaType::Image,
                    format: "png".into(),
                })
                .unwrap()
                .0
        };
        let a = mk("alpha.png");
        let b = mk("bravo.png");
        let space = "siglip@1+image+3+cos";
        store
            .set_embedding(&a, space, MediaType::Image, &[1.0, 0.0, 0.0], "siglip@1")
            .unwrap();
        store
            .set_embedding(&b, space, MediaType::Image, &[0.0, 1.0, 0.0], "siglip@1")
            .unwrap();

        // Query text matches no filename; inject a text vector pointing at bravo's embedding.
        let req = QueryRequest {
            text: Some("nonexistent".into()),
            mode: SearchMode::Semantic,
            ..Default::default()
        };
        let page = store
            .query_assets_semantic(
                &req,
                Some((space.to_string(), vec![0.05, 0.98, 0.0])),
                &Visibility::Full,
            )
            .unwrap();
        let names: Vec<String> = page.items.iter().map(|s| s.name.clone()).collect();
        assert!(
            !names.is_empty(),
            "text-vector search found assets despite zero lexical overlap"
        );
        assert_eq!(
            names[0], "bravo.png",
            "nearest to the query vector ranks first: {names:?}"
        );
    }
}
