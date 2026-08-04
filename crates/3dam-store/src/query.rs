//! Faceted query, id enumeration, and library stats — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    /// Faceted query → a page of summaries. Continuations are opaque keyset cursors bound to the
    /// active sort and deterministic asset-id tie-breaker; no page performs offset work.
    /// The caller always names a ceiling — there is deliberately no `Full`-forwarding convenience
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

        // Semantic-search M5: a Hybrid/Semantic query with text widens the lexical hits with their
        // embedding neighbours. Falls back to the lexical path below when there's no text.
        if req.mode != SearchMode::Lexical && req.text.as_ref().is_some_and(|t| !t.is_empty()) {
            return self.query_hybrid(req, limit, text_vec, vis);
        }

        let (count_where, count_binds) = build_where(req, &self.synonyms, vis)?;
        let text = req.text.as_ref().filter(|text| !text.is_empty());
        if req.sort.field == SortField::Relevance && text.is_some() && req.sort.dir == SortDir::Desc
        {
            return Err(LibError::BadRequest(
                "descending relevance order is not supported".into(),
            ));
        }

        let dir = match req.sort.dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };
        // Text matches are materialized once and carry their tier + bm25 rank into ORDER BY. A
        // relevance sort without text has nothing to rank, so it degrades to name order.
        let (order_clause, key_select) = match req.sort.field {
            SortField::Relevance if text.is_some() => (
                "text_matches.tier ASC, text_matches.rank ASC, LENGTH(filename) ASC, \
                     filename ASC, asset.id ASC"
                    .into(),
                ", text_matches.tier, text_matches.rank, LENGTH(filename), filename",
            ),
            SortField::Relevance | SortField::Name => {
                (format!("filename {dir}, asset.id {dir}"), ", filename")
            }
            SortField::Size => (
                format!("browse_size_bytes {dir}, asset.id {dir}"),
                ", browse_size_bytes",
            ),
            SortField::Scanned => (format!("scanned_at {dir}, asset.id {dir}"), ", scanned_at"),
        };

        let conn = self.read()?;

        // The first page establishes an exact selection count. Infinite-scroll continuations omit
        // it unless explicitly requested, avoiding the old full filtered recount on every page.
        let total = if req.include_total.unwrap_or(req.page.after.is_none()) {
            let count_sql = format!("SELECT COUNT(*) FROM asset{count_where}");
            Some(
                conn.query_row(
                    &count_sql,
                    rusqlite::params_from_iter(count_binds.iter()),
                    |row| row.get::<_, i64>(0),
                )
                .map_err(internal)? as u64,
            )
        } else {
            None
        };

        // LEFT JOIN the per-type attr tables so each grid row carries a couple of cheap key
        // attributes (dimensions / duration / triangles) without an N+1 fetch. Column names stay
        // unambiguous across the joined tables, so the bare-name filters above keep working.
        let (cte, from, mut page_where, mut page_binds) = if let Some(text) = text {
            let ranked = ranked_text_matches(text, &self.synonyms);
            let (where_sql, filter_binds) = build_where_without_text(req, vis)?;
            let mut binds = ranked.binds;
            binds.extend(filter_binds);
            (
                ranked.cte,
                "text_matches JOIN asset ON asset.rowid = text_matches.rowid",
                where_sql,
                binds,
            )
        } else {
            (String::new(), "asset", count_where, count_binds)
        };

        let cursor = decode_browse_cursor(
            req.page.after.as_ref(),
            BrowseOrder::field(req.sort.field, req.sort.dir),
        )?;
        if let Some((key, id)) = cursor {
            match (req.sort.field, text.is_some(), key) {
                (
                    SortField::Relevance,
                    true,
                    BrowseKey::Relevance {
                        tier,
                        rank,
                        length,
                        name,
                    },
                ) => {
                    push_relevance_keyset(
                        &mut page_where,
                        &mut page_binds,
                        RelevancePosition {
                            tier,
                            rank,
                            length,
                            name: &name,
                            id,
                        },
                        SortDir::Asc,
                    );
                }
                (SortField::Relevance | SortField::Name, false, BrowseKey::Text(key))
                | (SortField::Name, true, BrowseKey::Text(key)) => push_keyset_predicate(
                    &mut page_where,
                    &mut page_binds,
                    "filename",
                    Some(Value::Text(key)),
                    id,
                    req.sort.dir,
                    false,
                ),
                (SortField::Size, _, BrowseKey::Integer(key)) => push_keyset_predicate(
                    &mut page_where,
                    &mut page_binds,
                    "browse_size_bytes",
                    key.map(Value::Integer),
                    id,
                    req.sort.dir,
                    true,
                ),
                (SortField::Scanned, _, BrowseKey::Integer(key)) => push_keyset_predicate(
                    &mut page_where,
                    &mut page_binds,
                    "scanned_at",
                    key.map(Value::Integer),
                    id,
                    req.sort.dir,
                    false,
                ),
                _ => {
                    return Err(LibError::BadRequest(
                        "browse cursor key does not match the active sort".into(),
                    ));
                }
            }
        }
        let sql = format!(
            "{cte} {GRID_SELECT}{key_select} FROM {from} {ATTR_JOINS} {page_where} \
             ORDER BY {order_clause} LIMIT ?"
        );
        page_binds.push(Value::Integer(limit as i64 + 1));

        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(page_binds.iter()), |row| {
                let summary = row_to_summary(row)?;
                let key = match (req.sort.field, text.is_some()) {
                    (SortField::Relevance, true) => BrowseKey::Relevance {
                        tier: row.get(16)?,
                        rank: row.get(17)?,
                        length: row.get(18)?,
                        name: row.get(19)?,
                    },
                    (SortField::Relevance | SortField::Name, _) => BrowseKey::Text(row.get(16)?),
                    (SortField::Size, _) | (SortField::Scanned, _) => {
                        BrowseKey::Integer(row.get(16)?)
                    }
                };
                Ok((summary, key))
            })
            .map_err(internal)?;
        let mut keyed_items = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;

        let has_more = keyed_items.len() > limit as usize;
        if has_more {
            keyed_items.pop();
        }
        let next = if has_more {
            keyed_items
                .last()
                .map(|(item, key)| {
                    encode_browse_cursor(
                        BrowseOrder::field(req.sort.field, req.sort.dir),
                        key.clone(),
                        item.id,
                    )
                })
                .transpose()?
        } else {
            None
        };
        let items = keyed_items.into_iter().map(|(item, _)| item).collect();
        Ok(Page {
            items,
            cursor: next,
            total,
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
        text_vec: Option<(String, Vec<f32>)>,
        vis: &Visibility,
    ) -> Result<Page<AssetSummary>, LibError> {
        use std::collections::HashMap;
        // Fusion knobs. RRF k=60 is the usual default; the caps bound the fusion work per query.
        const RRF_K: f64 = 60.0;
        const LEX_CAP: usize = 400; // lexical candidates gathered
        const SEM_SEEDS: usize = 24; // top lexical hits that seed the embedding expansion
        const SEM_CAP: usize = 400; // neighbours kept from the expansion

        let conn = self.read()?;

        // 1. Lexical candidates, best-first by bm25 (the M1 ranking), bounded.
        let text = req
            .text
            .as_deref()
            .expect("hybrid path requires query text");
        let ranked = ranked_text_matches(text, &self.synonyms);
        let (where_sql, filter_binds) = build_where_without_text(req, vis)?;
        let mut binds = ranked.binds;
        binds.extend(filter_binds);
        let sql = format!(
            "{cte} SELECT asset.id FROM text_matches \
             JOIN asset ON asset.rowid = text_matches.rowid {ATTR_JOINS} {where_sql} \
             ORDER BY text_matches.tier ASC, text_matches.rank ASC, \
                      LENGTH(filename) ASC, filename ASC LIMIT {LEX_CAP}",
            cte = ranked.cte,
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
                .then_with(|| a.0.cmp(&b.0))
        });

        let total = req
            .include_total
            .unwrap_or(req.page.after.is_none())
            .then_some(ranked.len() as u64);
        if let Some((key, cursor_id)) =
            decode_browse_cursor(req.page.after.as_ref(), BrowseOrder::Hybrid)?
        {
            let BrowseKey::Hybrid { score, name } = key else {
                return Err(LibError::BadRequest(
                    "browse cursor key does not match hybrid ranking".into(),
                ));
            };
            if !score.is_finite() {
                return Err(LibError::BadRequest(
                    "hybrid browse cursor score is not finite".into(),
                ));
            }
            ranked.retain(|(id, candidate_score)| {
                let candidate_name = summaries.get(id).map(|s| s.name.as_str()).unwrap_or("");
                *candidate_score < score
                    || (*candidate_score == score
                        && (candidate_name > name.as_str()
                            || (candidate_name == name && *id > cursor_id)))
            });
        }
        ranked.truncate(limit as usize + 1);
        let has_more = ranked.len() > limit as usize;
        if has_more {
            ranked.pop();
        }
        let next = if has_more {
            ranked
                .last()
                .map(|(id, score)| {
                    let name = summaries
                        .get(id)
                        .map(|summary| summary.name.clone())
                        .unwrap_or_default();
                    encode_browse_cursor(
                        BrowseOrder::Hybrid,
                        BrowseKey::Hybrid {
                            score: *score,
                            name,
                        },
                        *id,
                    )
                })
                .transpose()?
        } else {
            None
        };
        let items = ranked
            .into_iter()
            .filter_map(|(id, _)| summaries.get(&id).cloned())
            .collect();
        Ok(Page {
            items,
            cursor: next,
            total,
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
        let conn = self.read()?;
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
        let conn = self.read()?;
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
        let conn = self.read()?;

        // Source grants compose as a union of whole sources, which the source×facet aggregates can
        // sum exactly. A collection grant is an arbitrary asset subset and may overlap a granted
        // source, so it deliberately stays on the predicate path below: summing both would double
        // count the intersection and using only source rows would hide collection-only members.
        if vis
            .restricted()
            .is_none_or(|scope| scope.collections.is_empty())
        {
            return Self::stats_from_aggregates(&conn, source, vis);
        }

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

    fn stats_from_aggregates(
        conn: &Connection,
        source: Option<&SourceId>,
        vis: &Visibility,
    ) -> Result<LibraryStats, LibError> {
        let selected_sources: Option<Vec<Vec<u8>>> = match (source, vis.restricted()) {
            (Some(id), None) => Some(vec![id.as_bytes().to_vec()]),
            (Some(id), Some(scope)) if scope.sources.contains(id) => {
                Some(vec![id.as_bytes().to_vec()])
            }
            (Some(_), Some(_)) => Some(Vec::new()),
            (None, None) => None,
            (None, Some(scope)) => Some(
                scope
                    .sources
                    .iter()
                    .map(|id| id.as_bytes().to_vec())
                    .collect(),
            ),
        };
        let filter = |column: &str| -> (String, Vec<Value>) {
            match &selected_sources {
                None => (String::new(), Vec::new()),
                Some(ids) if ids.is_empty() => (" AND 0=1".into(), Vec::new()),
                Some(ids) => (
                    format!(
                        " AND {column} IN ({})",
                        ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
                    ),
                    ids.iter().cloned().map(Value::Blob).collect(),
                ),
            }
        };

        let (total, unanalyzed) = if selected_sources.is_none() {
            conn.query_row(
                "SELECT asset_count, unanalyzed_count FROM library_stat WHERE singleton = 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(internal)?
        } else {
            let (pred, binds) = filter("source_id");
            conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM(asset_count), 0), COALESCE(SUM(unanalyzed_count), 0)
                       FROM source_stat WHERE 1=1{pred}"
                ),
                rusqlite::params_from_iter(binds.iter()),
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(internal)?
        };
        let sources = match (source, vis.restricted()) {
            (Some(_), _) => 1,
            (None, None) => conn
                .query_row(
                    "SELECT source_count FROM library_stat WHERE singleton = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(internal)?,
            (None, Some(scope)) => scope.sources.len() as i64,
        };

        let mut by_media = CountMap::new();
        if selected_sources.is_none() {
            let mut stmt = conn
                .prepare("SELECT media_type, asset_count FROM media_stat")
                .map_err(internal)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(internal)?;
            for row in rows {
                let (name, count) = row.map_err(internal)?;
                by_media.insert(name, count.max(0) as u64);
            }
        } else {
            let (pred, binds) = filter("source_id");
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT media_type, SUM(asset_count) FROM source_media_stat
                      WHERE 1=1{pred} GROUP BY media_type"
                ))
                .map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(internal)?;
            for row in rows {
                let (name, count) = row.map_err(internal)?;
                by_media.insert(name, count.max(0) as u64);
            }
        }

        let mut by_source = CountMap::new();
        {
            let (pred, binds) = filter("s.id");
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT s.name, ss.asset_count FROM source s
                       JOIN source_stat ss ON ss.source_id = s.id WHERE 1=1{pred}"
                ))
                .map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(internal)?;
            for row in rows {
                let (name, count) = row.map_err(internal)?;
                by_source.insert(name, count.max(0) as u64);
            }
        }

        let mut tags = CountMap::new();
        if selected_sources.is_none() {
            let mut stmt = conn
                .prepare(
                    "SELECT t.name, ts.asset_count FROM tag_stat ts JOIN tag t ON t.id = ts.tag_id
                      WHERE ts.asset_count > 0 ORDER BY ts.asset_count DESC, t.name ASC LIMIT 30",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(internal)?;
            for row in rows {
                let (name, count) = row.map_err(internal)?;
                tags.insert(name, count.max(0) as u64);
            }
        } else {
            let (pred, binds) = filter("sts.source_id");
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT t.name, SUM(sts.asset_count) AS n FROM source_tag_stat sts
                       JOIN tag t ON t.id = sts.tag_id WHERE 1=1{pred}
                       GROUP BY sts.tag_id HAVING n > 0 ORDER BY n DESC, t.name ASC LIMIT 30"
                ))
                .map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(internal)?;
            for row in rows {
                let (name, count) = row.map_err(internal)?;
                tags.insert(name, count.max(0) as u64);
            }
        }

        Ok(LibraryStats {
            total: total.max(0) as u64,
            by_media,
            by_source,
            tags,
            unanalyzed: unanalyzed.max(0) as u64,
            sources: sources.max(0) as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::page::PageParams;
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

    #[test]
    fn maintained_stats_preserve_source_and_collection_visibility_unions() {
        use dam_api::service::VisibilityScope;

        let store = Store::open_in_memory().unwrap();
        let source_one = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/one".into(),
                },
                "one",
                false,
            )
            .unwrap();
        let source_two = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/two".into(),
                },
                "two",
                false,
            )
            .unwrap();
        let insert = |source_id, name: &str, media_type| {
            store
                .upsert_asset(&NewAsset {
                    source_id,
                    path: name.into(),
                    filename: name.into(),
                    content_hash: None,
                    size_bytes: None,
                    source_modified_at: None,
                    scanned_at: 0,
                    media_type,
                    format: name.rsplit_once('.').unwrap().1.into(),
                })
                .unwrap()
                .0
        };
        let one_image = insert(source_one, "one.png", MediaType::Image);
        let one_audio = insert(source_one, "one.wav", MediaType::Audio);
        let two_model = insert(source_two, "two.glb", MediaType::Model);
        store.mark_analysed(&one_audio, 1).unwrap();
        store
            .edit_manual_tags(&[one_image, two_model], &["shared".into()], &[], false)
            .unwrap();

        let collection = store
            .create_collection("mixed", CollectionKind::Manual, None)
            .unwrap();
        // The collection overlaps the granted source at one_image and adds two_model. The fallback
        // visibility join must count that union as 3, not sum it as 2 + 2.
        store
            .modify_collection_members(&collection, &[one_image, two_model], &[])
            .unwrap();

        let source_only = Visibility::Restricted(VisibilityScope {
            sources: [source_one].into_iter().collect(),
            ..VisibilityScope::default()
        });
        let stats = store.stats(None, &source_only).unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.unanalyzed, 1);
        assert_eq!(stats.by_media.get("image"), Some(&1));
        assert_eq!(stats.by_media.get("audio"), Some(&1));
        assert_eq!(stats.by_media.get("model"), None);
        assert_eq!(stats.by_source.keys().collect::<Vec<_>>(), vec!["one"]);
        assert_eq!(stats.tags.get("shared"), Some(&1));

        let union = Visibility::Restricted(VisibilityScope {
            sources: [source_one].into_iter().collect(),
            collections: [collection].into_iter().collect(),
            ..VisibilityScope::default()
        });
        let union_stats = store.stats(None, &union).unwrap();
        assert_eq!(
            union_stats.total, 3,
            "overlapping visibility union double-counted"
        );
        assert_eq!(union_stats.tags.get("shared"), Some(&2));
        assert_eq!(
            union_stats.by_source.keys().collect::<Vec<_>>(),
            vec!["one"]
        );

        let scoped = store.stats(Some(&source_two), &Visibility::Full).unwrap();
        assert_eq!(scoped.total, 1);
        assert_eq!(scoped.by_media.get("model"), Some(&1));
        assert_eq!(scoped.tags.get("shared"), Some(&1));

        let listed = store.list_sources().unwrap();
        assert_eq!(listed[0].stats.asset_count, 2);
        assert_eq!(listed[1].stats.asset_count, 1);
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

        // `Folder` is the same scope minus its subtrees — "what is filed *here*" (issue #66). The
        // pair is the point: `Environment/` holds three assets in total but nothing at its own
        // level, which is exactly the distinction the subfolder toggle exposes.
        let here = |prefix: &str| {
            let req = QueryRequest {
                filters: vec![Filter {
                    field: FacetField::Folder,
                    op: FilterOp::Eq,
                    value: FilterValue::Str(prefix.into()),
                }],
                ..Default::default()
            };
            query_all(&store, &req).unwrap().total.unwrap()
        };
        assert_eq!(
            here("Environment/"),
            0,
            "no files sit directly in Environment/"
        );
        assert_eq!(here("Environment/Rock/"), 2);
        // Empty prefix means the source root, *not* "everything" — the one place `Folder` and
        // `Path` deliberately disagree.
        assert_eq!(here(""), 1, "only the loose file at the top level");
    }

    #[test]
    fn folder_hierarchy_keeps_source_and_segment_boundaries() {
        use dam_sources::{SftpConfig, SmbConfig};

        let store = Store::open_in_memory().unwrap();
        let sources = [
            SourceConnection::LocalFs {
                root: "/catalog".into(),
            },
            SourceConnection::Sftp(SftpConfig {
                host: "example.test".into(),
                port: 22,
                username: "artist".into(),
                base_path: "/remote/catalog".into(),
                password: None,
                private_key: None,
                passphrase: None,
                credential_ref: None,
            }),
            SourceConnection::Smb(SmbConfig {
                host: "files.example.test".into(),
                port: 445,
                share: "assets".into(),
                base_path: "catalog".into(),
                username: String::new(),
                password: None,
                domain: None,
                credential_ref: None,
            }),
        ];

        for (index, connection) in sources.iter().enumerate() {
            let source = store
                .add_source(connection, &format!("source-{index}"), false)
                .unwrap();
            for path in [
                "Art/loose.png",
                "Art/Sub/inside.png",
                "Artist/not-art.png",
                "日本語/深い/item.png",
                "root.png",
            ] {
                store
                    .upsert_asset(&NewAsset {
                        source_id: source,
                        path: path.into(),
                        filename: path.rsplit('/').next().unwrap().into(),
                        content_hash: None,
                        size_bytes: Some(1),
                        source_modified_at: None,
                        scanned_at: now_ms(),
                        media_type: MediaType::Image,
                        format: "png".into(),
                    })
                    .unwrap();
            }

            let entries = |prefix: &str| {
                store
                    .list_folders(&source, prefix)
                    .unwrap()
                    .into_iter()
                    .map(|entry| (entry.name, entry.asset_count))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                entries(""),
                vec![
                    ("Art".into(), 2),
                    ("Artist".into(), 1),
                    ("日本語".into(), 1)
                ],
                "source {index} lost a root or segment boundary"
            );
            assert_eq!(entries("Art/"), vec![("Sub".into(), 1)]);
            assert_eq!(entries("Artist/"), Vec::<(String, u64)>::new());
            assert_eq!(entries("日本語/"), vec![("深い".into(), 1)]);
        }
    }

    #[test]
    fn folder_counts_follow_reconciliation_removal_and_source_deletion() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/catalog".into(),
                },
                "catalog",
                false,
            )
            .unwrap();
        let asset = |path: &str, size_bytes: i64| NewAsset {
            source_id: source,
            path: path.into(),
            filename: path.rsplit('/').next().unwrap().into(),
            content_hash: None,
            size_bytes: Some(size_bytes),
            source_modified_at: None,
            scanned_at: now_ms(),
            media_type: MediaType::Image,
            format: "png".into(),
        };
        let (first, _) = store.upsert_asset(&asset("A/one.png", 1)).unwrap();
        store.upsert_asset(&asset("B/two.png", 1)).unwrap();

        // Same-path reconciliation updates the asset row but must not count it a second time.
        store.upsert_asset(&asset("A/one.png", 2)).unwrap();
        store
            .mark_paths_missing(&source, &["A/one.png".into()])
            .unwrap();
        store.upsert_asset(&asset("A/one.png", 2)).unwrap();
        let entries = store.list_folders(&source, "").unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.asset_count))
                .collect::<Vec<_>>(),
            vec![("A", 1), ("B", 1)]
        );

        store.remove_asset(&first, false).unwrap();
        assert_eq!(
            store
                .list_folders(&source, "")
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>(),
            vec!["B".to_string()],
            "asset removal retained an empty branch"
        );

        store.remove_source(&source, false).unwrap();
        let hierarchy_rows: i64 = store
            .read()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM folder WHERE source_id = ?1",
                params![source.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hierarchy_rows, 0);
    }

    #[test]
    fn folder_expansion_plan_uses_the_parent_index_without_reading_assets() {
        let store = store_with("Tree/Branch/leaf.png");
        let source = store.list_sources().unwrap()[0].id;
        let conn = store.read().unwrap();
        let mut statement = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT name, descendant_asset_count FROM folder
                  WHERE source_id = ?1 AND parent_path = ?2 AND path <> ''
                  ORDER BY name COLLATE NOCASE",
            )
            .unwrap();
        let details = statement
            .query_map(params![source.as_bytes().to_vec(), ""], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let plan = details.join("\n");
        assert!(
            plan.contains("SEARCH folder USING INDEX idx_folder_parent")
                && plan.contains("source_id=? AND parent_path=?"),
            "folder expansion missed its direct-child index: {plan}"
        );
        assert!(
            !plan.contains("asset"),
            "folder expansion reached back into catalog rows: {plan}"
        );
    }

    fn folder_scale_fixture(rows: usize, width: usize, depth: usize) -> (Store, SourceId) {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/folder-benchmark".into(),
                },
                "folder-benchmark",
                false,
            )
            .unwrap();
        {
            let mut connection = store.write();
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO asset(
                        id, source_id, path, filename, size_bytes, scanned_at,
                        media_type, format, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, 1, 0, 'image', 'png', 0, 0)",
                )
                .unwrap();
            for index in 0..rows {
                let branch = index % width;
                let mut directory = format!("branch_{branch:05}/");
                for level in 0..depth {
                    directory.push_str(&format!("level_{level:02}/"));
                }
                let filename = format!("asset_{index:07}.png");
                let path = format!("{directory}{filename}");
                insert
                    .execute(params![
                        AssetId::new().as_bytes().to_vec(),
                        source.as_bytes().to_vec(),
                        path,
                        filename,
                    ])
                    .unwrap();
            }
            drop(insert);
            transaction.commit().unwrap();
        }
        (store, source)
    }

    fn folder_expansion_p95(
        rows: usize,
        width: usize,
        depth: usize,
        samples: usize,
    ) -> (std::time::Duration, std::time::Duration) {
        let (store, source) = folder_scale_fixture(rows, width, depth);
        assert_eq!(
            store.list_folders(&source, "").unwrap().len(),
            width.min(rows)
        );
        assert_eq!(
            store.list_folders(&source, "branch_00000/").unwrap().len(),
            usize::from(depth > 0)
        );

        let p95 = |prefix: &str| {
            let mut timings = Vec::with_capacity(samples);
            for _ in 0..samples {
                let started = std::time::Instant::now();
                std::hint::black_box(store.list_folders(&source, prefix).unwrap());
                timings.push(started.elapsed());
            }
            timings.sort_unstable();
            timings[(samples * 95 / 100).min(samples - 1)]
        };
        (p95(""), p95("branch_00000/"))
    }

    #[test]
    fn scaled_deep_and_wide_folder_expansion_p95_stays_bounded() {
        let (root_p95, nested_p95) = folder_expansion_p95(20_000, 512, 12, 40);
        eprintln!("20k folder expansion p95: root={root_p95:?}, nested={nested_p95:?}");
        assert!(
            root_p95 < std::time::Duration::from_secs(2)
                && nested_p95 < std::time::Duration::from_secs(2),
            "indexed folder expansion regressed: root={root_p95:?}, nested={nested_p95:?}"
        );
    }

    /// Reproducible product-scale hierarchy benchmark. Run explicitly with:
    /// `cargo test -p dam-store million_asset_folder_expansion_p95 -- --ignored --nocapture`.
    #[test]
    #[ignore = "builds the explicit deep/wide 1M-asset folder fixture"]
    fn million_asset_folder_expansion_p95() {
        let (root_p95, nested_p95) = folder_expansion_p95(1_000_000, 10_000, 20, 50);
        eprintln!("1M folder expansion p95: root={root_p95:?}, nested={nested_p95:?}");
    }

    /// Issue #66's discovery half: a folder name is meaning the catalog should be able to find, not
    /// just navigate to. Before this, `Cliffs/` was reachable only by walking the tree — typing
    /// "cliffs" matched nothing, because no column held the directory a file sits in.
    #[test]
    fn folder_names_are_searchable() {
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
        mk("Environment/Rock/Cliffs/a.png");
        mk("Environment/Rock/Cliffs/b.png");
        mk("Weapons_AK47/rifle.png");
        mk("loose.png");

        let mut hits = search(&store, "cliffs");
        hits.sort();
        assert_eq!(
            hits,
            vec!["a.png", "b.png"],
            "a folder name finds its files"
        );

        // Segments tokenise like filenames do, so a packed name is reachable by its parts.
        assert_eq!(search(&store, "weapons"), vec!["rifle.png"]);
        assert_eq!(search(&store, "ak47"), vec!["rifle.png"]);
        assert_eq!(search(&store, "47"), vec!["rifle.png"]);

        // The filename is *not* folded into the folder column — it has its own, and duplicating it
        // would double-count a name match and quietly distort the ranking it is meant to lead.
        assert!(
            search(&store, "loose").contains(&"loose.png".to_string()),
            "a root-level file is still findable by name"
        );
    }

    /// A folder match is authored, so it outranks body prose — but never the file actually named
    /// after the query. Both halves matter: the first is why folder joins the authored tier at all,
    /// the second is why its weight sits below `filename`.
    #[test]
    fn a_filename_outranks_a_folder_match() {
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
        mk("Cliffs/texture.png");
        mk("Misc/cliffs.png");

        assert_eq!(
            search(&store, "cliffs"),
            vec!["cliffs.png", "texture.png"],
            "the file named after the query beats the one merely filed under it"
        );
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

    #[test]
    fn filename_substring_and_punctuation_fallbacks_preserve_recall() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        for filename in ["AK47_LowPoly.fbx", "impact...final.wav", "ordinary.png"] {
            store
                .upsert_asset(&NewAsset {
                    source_id: source,
                    path: filename.into(),
                    filename: filename.into(),
                    content_hash: None,
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: MediaType::Image,
                    format: "bin".into(),
                })
                .unwrap();
        }

        assert_eq!(search(&store, "k47"), vec!["AK47_LowPoly.fbx"]);
        assert_eq!(search(&store, "..."), vec!["impact...final.wav"]);
        assert_eq!(
            search(&store, ".."),
            vec!["impact...final.wav"],
            "sub-trigram punctuation stays available through the bounded candidate stage"
        );
    }

    fn explain_ranked_text(store: &Store, text: &str) -> (String, Vec<String>) {
        let req = QueryRequest {
            text: Some(text.into()),
            ..Default::default()
        };
        let ranked = ranked_text_matches(text, &store.synonyms);
        let (where_sql, filter_binds) = build_where_without_text(&req, &Visibility::Full).unwrap();
        let mut binds = ranked.binds;
        binds.extend(filter_binds);
        let sql = format!(
            "{} SELECT asset.id FROM text_matches \
             JOIN asset ON asset.rowid = text_matches.rowid {where_sql}",
            ranked.cte
        );
        let conn = store.read().unwrap();
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let details = statement
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap();
        (sql, details)
    }

    #[test]
    fn token_and_substring_plan_stays_on_virtual_indexes_and_ranks_once() {
        let store = store_with("AK47_LowPoly.fbx");
        let (sql, details) = explain_ranked_text(&store, "k47");
        let plan = details.join("\n");
        assert!(
            plan.contains("asset_fts") && plan.contains("VIRTUAL TABLE INDEX"),
            "primary token search did not use FTS: {plan}"
        );
        assert!(
            plan.contains("asset_filename_trigram") && plan.contains("VIRTUAL TABLE INDEX"),
            "substring fallback did not use trigram FTS: {plan}"
        );
        assert!(
            !details.iter().any(|detail| detail == "SCAN asset"),
            "text search regressed to an unbounded catalog scan: {plan}"
        );
        assert!(
            !plan.contains("CORRELATED"),
            "rank/search probes became per-row correlated subqueries: {plan}"
        );
        assert_eq!(
            sql.matches("bm25(").count(),
            1,
            "bm25 must be computed once in the materialized FTS matched set"
        );
    }

    #[test]
    fn short_punctuation_plan_is_a_bounded_rowid_range_not_a_catalog_scan() {
        let store = store_with("impact...final.wav");
        let (_sql, details) = explain_ranked_text(&store, "..");
        let plan = details.join("\n");
        assert!(
            details.iter().any(|detail| {
                detail.contains("SEARCH short_asset USING INTEGER PRIMARY KEY (rowid>?)")
            }),
            "short fallback is not bounded by its indexed rowid window: {plan}"
        );
        assert!(
            !details
                .iter()
                .any(|detail| detail == "SCAN asset" || detail == "SCAN short_asset"),
            "short fallback regressed to an unbounded catalog scan: {plan}"
        );
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

    /// Pending automation never powers discovery; accepting makes it searchable, and undo removes
    /// it again while retaining the suggestion for review.
    #[test]
    fn pending_tag_is_excluded_until_accept_and_undo_removes_it_again() {
        let store = store_with("clip_0001.wav");
        let id = query_all(&store, &QueryRequest::default()).unwrap().items[0].id;
        // A term that appears only as a tag, never in the filename.
        assert!(search(&store, "snare").is_empty());
        store
            .suggest_tag(&id, "snare", 0.9, "test@1", "waveform matched a snare")
            .unwrap();
        assert!(search(&store, "snare").is_empty());
        store
            .review_suggestion(&id, "snare", ReviewAction::Accept)
            .unwrap();
        assert_eq!(search(&store, "snare"), vec!["clip_0001.wav"]);
        store
            .review_suggestion(&id, "snare", ReviewAction::Undo)
            .unwrap();
        assert!(search(&store, "snare").is_empty());
    }

    #[test]
    fn automated_class_filter_requires_the_matching_accepted_suggestion() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "classes",
                false,
            )
            .unwrap();
        let id = store
            .upsert_asset(&NewAsset {
                source_id: source,
                path: "impact.wav".into(),
                filename: "impact.wav".into(),
                content_hash: None,
                size_bytes: Some(1),
                source_modified_at: None,
                scanned_at: now_ms(),
                media_type: MediaType::Audio,
                format: "wav".into(),
            })
            .unwrap()
            .0;
        store
            .set_media_class(&id, MediaType::Audio, "one_shot")
            .unwrap();
        store
            .suggest_tag(
                &id,
                "one_shot",
                0.8,
                "audio@1",
                "measured transient envelope",
            )
            .unwrap();
        let request = QueryRequest {
            filters: vec![Filter {
                field: FacetField::AudioClass,
                op: FilterOp::Eq,
                value: FilterValue::Str("one_shot".into()),
            }],
            ..QueryRequest::default()
        };
        assert!(query_all(&store, &request).unwrap().items.is_empty());
        store
            .review_suggestion(&id, "one_shot", ReviewAction::Accept)
            .unwrap();
        assert_eq!(query_all(&store, &request).unwrap().items[0].id, id);
        store
            .review_suggestion(&id, "one_shot", ReviewAction::Undo)
            .unwrap();
        assert!(query_all(&store, &request).unwrap().items.is_empty());

        store
            .review_suggestion(&id, "one_shot", ReviewAction::Reject)
            .unwrap();
        store
            .edit_manual_tags(&[id], &["music".into()], &[], false)
            .unwrap();
        let corrected = QueryRequest {
            filters: vec![Filter {
                field: FacetField::AudioClass,
                op: FilterOp::Eq,
                value: FilterValue::Str("music".into()),
            }],
            ..QueryRequest::default()
        };
        assert_eq!(query_all(&store, &corrected).unwrap().items[0].id, id);
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

    fn large_search_fixture(rows: usize) -> Store {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/benchmark".into(),
                },
                "benchmark",
                false,
            )
            .unwrap();
        {
            let mut connection = store.write();
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            {
                let mut insert = transaction
                    .prepare_cached(
                        "INSERT INTO asset(
                            id, source_id, path, filename, size_bytes, scanned_at,
                            media_type, format, created_at, updated_at
                         ) VALUES (?1, ?2, ?3, ?3, 1, 0, 'image', 'png', 0, 0)",
                    )
                    .unwrap();
                for index in 0..rows {
                    let filename = if index % 997 == 0 {
                        format!("needle_target_{index:07}.png")
                    } else {
                        format!("catalog_asset_{index:07}.png")
                    };
                    insert
                        .execute(rusqlite::params![
                            AssetId::new().as_bytes().to_vec(),
                            source.as_bytes().to_vec(),
                            filename
                        ])
                        .unwrap();
                }
            }
            transaction.commit().unwrap();
        }
        store
    }

    fn first_page_p95(rows: usize, samples: usize) -> std::time::Duration {
        let store = large_search_fixture(rows);
        let request = QueryRequest {
            text: Some("needle".into()),
            sort: Sort {
                field: SortField::Relevance,
                ..Default::default()
            },
            page: PageParams {
                after: None,
                limit: 24,
            },
            ..Default::default()
        };
        let page = query_all(&store, &request).unwrap();
        assert_eq!(page.items.len(), rows.div_ceil(997).min(24));

        let mut timings = Vec::with_capacity(samples);
        for _ in 0..samples {
            let started = std::time::Instant::now();
            std::hint::black_box(query_all(&store, &request).unwrap());
            timings.push(started.elapsed());
        }
        timings.sort_unstable();
        timings[(samples * 95 / 100).min(samples - 1)]
    }

    #[test]
    fn scaled_first_page_search_p95_stays_bounded() {
        let p95 = first_page_p95(20_000, 20);
        eprintln!("20k indexed first-page search p95: {p95:?}");
        assert!(
            p95 < std::time::Duration::from_secs(2),
            "20k first-page p95 regressed to {p95:?}"
        );
    }

    /// Reproducible release benchmark for the product-scale catalog. Run explicitly with:
    /// `cargo test -p dam-store million_asset_first_page_search_p95 -- --ignored --nocapture`.
    #[test]
    #[ignore = "builds the explicit 1M-asset search fixture"]
    fn million_asset_first_page_search_p95() {
        let p95 = first_page_p95(1_000_000, 40);
        eprintln!("1M indexed first-page search p95: {p95:?}");
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

    fn page_names(store: &Store, mut req: QueryRequest) -> (Vec<String>, Vec<Option<u64>>) {
        let mut names = Vec::new();
        let mut totals = Vec::new();
        loop {
            let page = query_all(store, &req).unwrap();
            names.extend(page.items.into_iter().map(|item| item.name));
            totals.push(page.total);
            let Some(after) = page.cursor else { break };
            req.page.after = Some(after);
        }
        (names, totals)
    }

    fn browse_fixture(specs: &[(&str, Option<i64>, i64)]) -> Store {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/browse".into(),
                },
                "browse",
                false,
            )
            .unwrap();
        for (name, size, scanned_at) in specs {
            store
                .upsert_asset(&NewAsset {
                    source_id: source,
                    path: (*name).into(),
                    filename: (*name).into(),
                    content_hash: None,
                    size_bytes: *size,
                    source_modified_at: None,
                    scanned_at: *scanned_at,
                    media_type: MediaType::Image,
                    format: "png".into(),
                })
                .unwrap();
        }
        store
    }

    #[test]
    fn keyset_pages_cross_null_and_tied_keys_in_both_directions() {
        let specs = [
            ("null-a.png", None, 10),
            ("null-b.png", None, 10),
            ("one.png", Some(1), 20),
            ("two-a.png", Some(2), 20),
            ("two-b.png", Some(2), 20),
        ];
        let store = browse_fixture(&specs);
        for field in [SortField::Size, SortField::Scanned] {
            for dir in [SortDir::Asc, SortDir::Desc] {
                let request = QueryRequest {
                    sort: Sort { field, dir },
                    page: PageParams {
                        after: None,
                        limit: 2,
                    },
                    ..Default::default()
                };
                let (paged, totals) = page_names(&store, request.clone());
                let expected = query_all(
                    &store,
                    &QueryRequest {
                        page: PageParams {
                            after: None,
                            limit: 100,
                        },
                        ..request
                    },
                )
                .unwrap()
                .items
                .into_iter()
                .map(|item| item.name)
                .collect::<Vec<_>>();
                assert_eq!(
                    paged, expected,
                    "{field:?}/{dir:?} skipped or repeated a row"
                );
                assert_eq!(totals[0], Some(specs.len() as u64));
                assert!(totals.iter().skip(1).all(Option::is_none));
            }
        }

        let suppressed = query_all(
            &store,
            &QueryRequest {
                include_total: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            suppressed.total, None,
            "explicit opt-out recounted page one"
        );
        let first = query_all(
            &store,
            &QueryRequest {
                page: PageParams {
                    after: None,
                    limit: 2,
                },
                ..Default::default()
            },
        )
        .unwrap();
        let forced = query_all(
            &store,
            &QueryRequest {
                include_total: Some(true),
                page: PageParams {
                    after: first.cursor,
                    limit: 2,
                },
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(forced.total, Some(specs.len() as u64));
    }

    #[test]
    fn inserts_and_deletes_before_a_cursor_do_not_shift_the_next_page() {
        for dir in [SortDir::Asc, SortDir::Desc] {
            let store = browse_fixture(&[
                ("a.png", Some(1), 1),
                ("b.png", Some(1), 1),
                ("c.png", Some(1), 1),
                ("d.png", Some(1), 1),
            ]);
            let mut request = QueryRequest {
                sort: Sort {
                    field: SortField::Name,
                    dir,
                },
                page: PageParams {
                    after: None,
                    limit: 2,
                },
                ..Default::default()
            };
            let first = query_all(&store, &request).unwrap();
            request.page.after = first.cursor;

            // Add a row on the already-consumed side and delete a previously returned row. OFFSET
            // would shift here; the keyset boundary must remain fixed.
            let inserted = if dir == SortDir::Asc {
                "aa.png"
            } else {
                "z.png"
            };
            let source = store.list_sources().unwrap()[0].id;
            store
                .upsert_asset(&NewAsset {
                    source_id: source,
                    path: inserted.into(),
                    filename: inserted.into(),
                    content_hash: None,
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: 1,
                    media_type: MediaType::Image,
                    format: "png".into(),
                })
                .unwrap();
            store
                .write()
                .execute(
                    "DELETE FROM asset WHERE id = ?1",
                    [first.items[0].id.as_bytes().as_slice()],
                )
                .unwrap();

            let second = query_all(&store, &request).unwrap();
            let names = second
                .items
                .into_iter()
                .map(|item| item.name)
                .collect::<Vec<_>>();
            let expected = if dir == SortDir::Asc {
                vec!["c.png", "d.png"]
            } else {
                vec!["b.png", "a.png"]
            };
            assert_eq!(names, expected, "mutation shifted {dir:?} continuation");
        }
    }

    #[test]
    fn relevance_and_default_hybrid_continue_with_their_exact_effective_order() {
        let store = browse_fixture(&[
            ("kick_a.png", Some(1), 1),
            ("kick_b.png", Some(1), 1),
            ("kick_c.png", Some(1), 1),
        ]);
        for mode in [SearchMode::Lexical, SearchMode::Hybrid] {
            let sort = if mode == SearchMode::Lexical {
                Sort {
                    field: SortField::Relevance,
                    dir: SortDir::Asc,
                }
            } else {
                Sort::default() // regression: hybrid ranking is independent of this Name/Asc sort
            };
            let (names, _) = page_names(
                &store,
                QueryRequest {
                    text: Some("kick".into()),
                    mode,
                    sort,
                    page: PageParams {
                        after: None,
                        limit: 1,
                    },
                    ..Default::default()
                },
            );
            assert_eq!(names.len(), 3, "{mode:?} page 2 failed");
            let unique = names.iter().collect::<std::collections::HashSet<_>>();
            assert_eq!(unique.len(), 3, "{mode:?} repeated a continuation row");
        }

        let descending = query_all(
            &store,
            &QueryRequest {
                text: Some("kick".into()),
                sort: Sort {
                    field: SortField::Relevance,
                    dir: SortDir::Desc,
                },
                ..Default::default()
            },
        );
        assert!(
            descending.is_err(),
            "mixed-direction relevance must be rejected"
        );
    }

    #[test]
    fn browse_cursor_rejects_hostile_mismatched_and_noncanonical_payloads() {
        use base64::Engine as _;

        let id = AssetId::new();
        let cursor = encode_browse_cursor(
            BrowseOrder::field(SortField::Name, SortDir::Asc),
            BrowseKey::Text("a.png".into()),
            id,
        )
        .unwrap();
        assert!(decode_browse_cursor(
            Some(&cursor),
            BrowseOrder::field(SortField::Name, SortDir::Desc)
        )
        .is_err());
        assert!(decode_browse_cursor(
            Some(&Cursor(format!("b1.{}", "a".repeat(4_097)))),
            BrowseOrder::field(SortField::Name, SortDir::Asc)
        )
        .is_err());

        let Cursor(raw) = cursor;
        let encoded = raw.strip_prefix("b1.").unwrap();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .unwrap();
        let mut payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        payload["id"] = serde_json::Value::String(id.to_string().to_uppercase());
        let hostile = Cursor(format!(
            "b1.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap())
        ));
        assert!(decode_browse_cursor(
            Some(&hostile),
            BrowseOrder::field(SortField::Name, SortDir::Asc)
        )
        .is_err());

        let wrong_key = encode_browse_cursor(
            BrowseOrder::field(SortField::Name, SortDir::Asc),
            BrowseKey::Integer(Some(1)),
            id,
        )
        .unwrap();
        assert!(decode_browse_cursor(
            Some(&wrong_key),
            BrowseOrder::field(SortField::Name, SortDir::Asc)
        )
        .is_err());

        payload["version"] = serde_json::json!(9);
        let bad_version = Cursor(format!(
            "b1.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap())
        ));
        assert!(decode_browse_cursor(
            Some(&bad_version),
            BrowseOrder::field(SortField::Name, SortDir::Asc)
        )
        .is_err());

        let non_finite_json = format!(
            r#"{{"version":1,"order":{{"kind":"hybrid"}},"key":{{"kind":"hybrid","value":{{"score":1e400,"name":"a.png"}}}},"id":"{id}"}}"#
        );
        let non_finite = Cursor(format!(
            "b1.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(non_finite_json)
        ));
        assert!(decode_browse_cursor(Some(&non_finite), BrowseOrder::Hybrid).is_err());
    }

    #[test]
    fn browse_sort_plans_use_composite_indexes_without_temp_ordering() {
        let store = browse_fixture(&[("a.png", Some(1), 1)]);
        let conn = store.read().unwrap();
        for (order, index) in [
            ("filename ASC, id ASC", "idx_asset_browse_name"),
            ("filename DESC, id DESC", "idx_asset_browse_name"),
            ("scanned_at ASC, id ASC", "idx_asset_browse_scanned"),
            ("scanned_at DESC, id DESC", "idx_asset_browse_scanned"),
            ("browse_size_bytes ASC, id ASC", "idx_asset_browse_size"),
            ("browse_size_bytes DESC, id DESC", "idx_asset_browse_size"),
        ] {
            let sql = format!(
                "EXPLAIN QUERY PLAN {GRID_SELECT} FROM asset {ATTR_JOINS} ORDER BY {order} LIMIT 25"
            );
            let plan = conn
                .prepare(&sql)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("\n");
            assert!(plan.contains(index), "missing {index}: {plan}");
            assert!(!plan.contains("TEMP B-TREE"), "temporary ordering: {plan}");
        }
    }

    fn browse_edge_p95(rows: usize, samples: usize) -> (std::time::Duration, std::time::Duration) {
        let store = large_search_fixture(rows);
        let mut target_index = rows.saturating_sub(50);
        while target_index.is_multiple_of(997) {
            target_index = target_index.saturating_sub(1);
        }
        let target_name = format!("catalog_asset_{target_index:07}.png");
        let (id, filename): (Vec<u8>, String) = store
            .read()
            .unwrap()
            .query_row(
                "SELECT id, filename FROM asset WHERE filename = ?1",
                [target_name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let after = encode_browse_cursor(
            BrowseOrder::field(SortField::Name, SortDir::Asc),
            BrowseKey::Text(filename),
            blob_to_asset_id(&id),
        )
        .unwrap();
        let late_request = QueryRequest {
            include_total: Some(false),
            page: PageParams {
                after: Some(after),
                limit: 24,
            },
            ..Default::default()
        };
        let first_request = QueryRequest {
            include_total: Some(false),
            page: PageParams {
                after: None,
                limit: 24,
            },
            ..Default::default()
        };
        let p95 = |request: &QueryRequest| {
            let mut timings = Vec::with_capacity(samples);
            for _ in 0..samples {
                let started = std::time::Instant::now();
                std::hint::black_box(query_all(&store, request).unwrap());
                timings.push(started.elapsed());
            }
            timings.sort_unstable();
            timings[(samples * 95 / 100).min(samples - 1)]
        };
        (p95(&first_request), p95(&late_request))
    }

    fn assert_flat_browse_edges(first: std::time::Duration, late: std::time::Duration) {
        // Both probes execute the same indexed query shape. Fixed slack absorbs scheduler and timer
        // noise when the actual work is measured in microseconds.
        let ceiling = first.saturating_mul(5) + std::time::Duration::from_millis(5);
        assert!(
            late <= ceiling,
            "late page {late:?} is not flat vs first {first:?} (ceiling {ceiling:?})"
        );
    }

    #[test]
    fn scaled_20k_late_keyset_page_p95_stays_bounded() {
        let (first, late) = browse_edge_p95(20_000, 20);
        eprintln!("20k keyset browse p95: first={first:?}, late={late:?}");
        assert_flat_browse_edges(first, late);
        assert!(
            late < std::time::Duration::from_secs(2),
            "late p95: {late:?}"
        );
    }

    #[test]
    #[ignore = "builds the explicit 1M-asset browse fixture"]
    fn million_asset_late_keyset_page_p95() {
        let (first, late) = browse_edge_p95(1_000_000, 40);
        eprintln!("1M keyset browse p95: first={first:?}, late={late:?}");
        assert_flat_browse_edges(first, late);
    }
}
