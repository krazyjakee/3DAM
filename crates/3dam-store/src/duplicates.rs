//! Duplicate detection and review (tech-spec 05 §4): exact content-hash groups, near-duplicate
//! embedding components, the cursors that page both, and the durable review decisions applied on
//! top. Part of the `Store` impl.
use super::*;
use crate::helpers::*;
use crate::similarity::{bytes_to_f32, cosine};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const NEAR_DUP_COSINE: f32 = 0.92;
// ANN seeds are deliberately much wider than one response page: HNSW supplies the bounded
// candidate edges, so this does not turn into an O(n^2) request. The same established bound keeps
// exact fallback complete for ordinary catalogs without materialising a scale catalog or emitting
// overlapping fragments that pretend to be global connected components.
const NEAR_DUP_SEED_PAGE: usize = 2_000;
#[cfg(feature = "ann")]
const NEAR_DUP_EXPANSION_MAX: usize = 8_192;
const NEAR_DUP_EXPANSION_CURSOR_MAX: usize = 8_192;
const NEAR_DUP_CURSOR_PREFIX: &str = "n3.";
type NearCandidate = (AssetId, String, String, Vec<f32>);
#[cfg(feature = "ann")]
type NearNeighbour = (AssetId, Vec<f32>);
type NearComponent = (String, String, Vec<AssetId>);
type NearPartitions = BTreeMap<(String, String, usize), Vec<(AssetId, Vec<f32>)>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NearDupMode {
    Ann,
    Exact,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
struct NearDupKey {
    media: String,
    space: String,
    id: String,
}

impl NearDupKey {
    fn from_candidate(candidate: &NearCandidate) -> Self {
        Self {
            media: candidate.2.clone(),
            space: candidate.1.clone(),
            id: candidate.0.to_string(),
        }
    }

    fn asset_id(&self) -> Result<AssetId, LibError> {
        self.id
            .parse()
            .map_err(|_| LibError::BadRequest("invalid near-duplicate cursor id".into()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NearDupCursor {
    version: u8,
    scan_id: String,
    media: Option<String>,
    mode: Option<NearDupMode>,
    after: Option<NearDupKey>,
    right_after: Option<NearDupKey>,
    component_offset: usize,
}

struct NearDupWindow {
    components: Vec<NearComponent>,
    generations: BTreeMap<String, i64>,
    current: NearDupCursor,
    next: Option<NearDupCursor>,
    suppress_existing_overlap: bool,
}

#[derive(Clone)]
struct NearDupBlock {
    candidates: Vec<NearCandidate>,
    generations: BTreeMap<String, i64>,
    has_next: bool,
    end: Option<NearDupKey>,
}

struct DupGroupSeed<'a> {
    kind: DupKind,
    ids: &'a [AssetId],
    total_members: u32,
    group: Option<String>,
    members_cursor: Option<Cursor>,
    signal: &'a str,
    review: &'a str,
}

impl Store {
    /// Duplicate groups for the review view (§4). `Exact` groups by content hash; `Near` groups by
    /// embedding cosine ≥ threshold within a media space (union-find over the pairwise relation, §4.3).
    pub fn duplicates(
        &self,
        req: &DupRequest,
        vis: &Visibility,
    ) -> Result<Page<DupGroup>, LibError> {
        let mut groups: Vec<DupGroup> = Vec::new();
        let limit = req.limit.clamp(1, DUP_GROUP_PAGE_MAX) as usize;
        let mut next_cursor = None;
        let mut partial = dam_api::PartialStatus::default();

        match req.kind {
            DupKind::Exact => {
                let conn = self.read()?;
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
                let has_more = exact.len() > limit;
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
                    let member_details = Self::duplicate_members_for_ids(&conn, &all_ids, vis)?;
                    for (hash, total_members) in exact {
                        let ids = ids_by_hash.remove(&hash).unwrap_or_default();
                        let group_key = encode_hex(&hash);
                        let members_cursor = (ids.len() < total_members as usize)
                            .then(|| member_cursor(&ids, &size_by_id))
                            .flatten();
                        if let Some(group) = Self::build_dup_group_from_summaries(
                            DupGroupSeed {
                                kind: DupKind::Exact,
                                ids: &ids,
                                total_members,
                                group: Some(group_key),
                                members_cursor,
                                signal: "identical bytes (same content hash)",
                                review: &format!("exact:{}", encode_hex(&hash)),
                            },
                            &member_details,
                        ) {
                            if let Some(group) =
                                Self::apply_duplicate_review(&conn, group, req.review)?
                            {
                                groups.push(group);
                            }
                        }
                    }
                }
            }
            DupKind::Near => {
                let cursor = decode_near_dup_cursor(req.after.as_ref(), req.media)?;
                let window = self.near_duplicate_window(cursor, req.media, vis, &mut partial)?;
                let offset = window.current.component_offset;
                if offset > window.components.len() {
                    return Err(LibError::BadRequest(
                        "near-duplicate cursor is outside its comparison window".into(),
                    ));
                }
                let remaining = &window.components[offset..];
                let all_ids: Vec<AssetId> = remaining
                    .iter()
                    .flat_map(|(_, _, ids)| ids.iter().take(DUP_GROUP_MEMBER_MAX).copied())
                    .collect();
                let conn = self.read()?;
                let member_details = Self::duplicate_members_for_ids(&conn, &all_ids, vis)?;
                let mut consumed = 0usize;
                let mut remembered = Vec::new();
                for (_media, space, ids) in remaining {
                    consumed += 1;
                    let member_ids: Vec<AssetId> =
                        ids.iter().take(DUP_GROUP_MEMBER_MAX).copied().collect();
                    let review = near_review_id(space, ids);
                    let generation = window.generations.get(space).copied().ok_or_else(|| {
                        LibError::Internal(format!("missing seed generation for {space:?}"))
                    })?;
                    remembered.push((
                        review.clone(),
                        space.clone(),
                        generation,
                        member_ids.clone(),
                    ));
                    if let Some(g) = Self::build_dup_group_from_summaries(
                        DupGroupSeed {
                            kind: DupKind::Near,
                            ids: &member_ids,
                            total_members: ids.len() as u32,
                            group: None,
                            members_cursor: None,
                            signal: &format!("{space} embedding cosine ≥ {NEAR_DUP_COSINE:.2}"),
                            review: &review,
                        },
                        &member_details,
                    ) {
                        if let Some(group) = Self::apply_duplicate_review(&conn, g, req.review)? {
                            groups.push(group);
                            if groups.len() == limit {
                                break;
                            }
                        }
                    }
                }
                drop(conn);
                let saved_reviews = self.remember_near_duplicate_groups(
                    &remembered,
                    &window.current.scan_id,
                    window.suppress_existing_overlap,
                )?;
                groups.retain(|group| saved_reviews.contains(&group.review));

                let next_offset = offset.saturating_add(consumed);
                if next_offset < window.components.len() {
                    let mut current = window.current;
                    current.component_offset = next_offset;
                    next_cursor = Some(encode_near_dup_cursor(current)?);
                } else if let Some(next) = window.next {
                    next_cursor = Some(encode_near_dup_cursor(next)?);
                }
            }
        }
        let mut page = Page::new(groups, next_cursor);
        page.partial = partial;
        Ok(page)
    }

    /// Persist review metadata and any requested catalog-only removals in one SQLite transaction.
    /// Blocking remains content-addressed: it removes every catalog row with the same hash and
    /// records that hash for future scans, but never opens or deletes a source file.
    pub fn review_duplicate(
        &self,
        req: &DupReviewRequest,
    ) -> Result<DuplicateReviewOutcome, LibError> {
        if req.review.len() > 96
            || !(req.review.starts_with("exact:") || req.review.starts_with("near:"))
        {
            return Err(LibError::BadRequest("invalid duplicate review id".into()));
        }
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;

        let group_members = duplicate_review_members(&tx, &req.review)?;

        if let Some(keep) = req.keep {
            if !group_members.contains(&keep) {
                return Err(LibError::BadRequest(format!(
                    "keep asset {keep} is not a member of {}",
                    req.review
                )));
            }
        }

        let mut removed = BTreeMap::<AssetId, SourceId>::new();
        for removal in &req.removals {
            if !group_members.contains(&removal.asset) {
                return Err(LibError::BadRequest(format!(
                    "removal asset {} is not a member of {}",
                    removal.asset, req.review
                )));
            }
            let row: Option<(Option<Vec<u8>>, String, Vec<u8>)> = tx
                .query_row(
                    "SELECT content_hash, filename, source_id FROM asset WHERE id = ?1",
                    params![removal.asset.as_bytes().to_vec()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(internal)?;
            let (hash, filename, source_blob) =
                row.ok_or_else(|| LibError::NotFound(format!("asset {}", removal.asset)))?;

            if removal.block {
                if let Some(hash) = hash {
                    let mut stmt = tx
                        .prepare("SELECT id, source_id FROM asset WHERE content_hash = ?1")
                        .map_err(internal)?;
                    let rows = stmt
                        .query_map(params![hash.clone()], |row| {
                            Ok((
                                blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                                blob_to_source_id(&row.get::<_, Vec<u8>>(1)?),
                            ))
                        })
                        .map_err(internal)?;
                    for row in rows {
                        let (id, source) = row.map_err(internal)?;
                        removed.insert(id, source);
                    }
                    drop(stmt);
                    tx.execute(
                        "DELETE FROM asset WHERE content_hash = ?1",
                        params![hash.clone()],
                    )
                    .map_err(internal)?;
                    tx.execute(
                        "INSERT INTO blocklist (content_hash, label, blocked_at)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(content_hash) DO UPDATE SET label = excluded.label",
                        params![hash, filename, now_ms()],
                    )
                    .map_err(internal)?;
                    continue;
                }
            }

            let source = <[u8; 16]>::try_from(source_blob.as_slice())
                .ok()
                .map(SourceId::from_bytes)
                .ok_or_else(|| LibError::Internal("invalid source id in asset row".into()))?;
            tx.execute(
                "DELETE FROM asset WHERE id = ?1",
                params![removal.asset.as_bytes().to_vec()],
            )
            .map_err(internal)?;
            removed.insert(removal.asset, source);
        }

        let state = match req.state {
            DupReviewState::Pending => "pending",
            DupReviewState::Resolved => "resolved",
            DupReviewState::Dismissed => "dismissed",
        };
        tx.execute(
            "INSERT INTO duplicate_review (review_key, state, chosen_keep, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(review_key) DO UPDATE SET
                 state = excluded.state,
                 chosen_keep = COALESCE(excluded.chosen_keep, duplicate_review.chosen_keep),
                 updated_at = excluded.updated_at",
            params![
                req.review,
                state,
                req.keep.map(|id| id.as_bytes().to_vec()),
                now_ms()
            ],
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)?;
        Ok(DuplicateReviewOutcome {
            removed_assets: removed.into_iter().collect(),
        })
    }

    /// Select one bounded near-duplicate comparison window. A scan pins its first successful mode:
    /// exact fallback stays exact, while an ANN cursor whose graph disappears must be restarted
    /// because switching component algorithms mid-scan cannot preserve membership semantics.
    fn near_duplicate_window(
        &self,
        cursor: NearDupCursor,
        media: Option<MediaType>,
        vis: &Visibility,
        partial: &mut dam_api::PartialStatus,
    ) -> Result<NearDupWindow, LibError> {
        if cursor.mode == Some(NearDupMode::Exact) {
            return self.near_duplicate_exact_window(cursor, media, vis, partial);
        }

        #[cfg(feature = "ann")]
        {
            let block =
                load_near_dup_block(self, media, vis, cursor.after.as_ref(), NEAR_DUP_SEED_PAGE)?;
            if let Some((components, expansion_capped)) =
                near_components_ann(self, &block.candidates, vis)?
            {
                if expansion_capped {
                    partial.complete = false;
                    partial.warnings.push(dam_api::ItemWarning {
                        subject: "near-duplicates".into(),
                        code: "duplicate_component_expansion_capped".into(),
                        message: format!(
                            "near-duplicate component expansion is capped at {NEAR_DUP_EXPANSION_MAX} assets per seed page"
                        ),
                    });
                }
                let current = NearDupCursor {
                    mode: Some(NearDupMode::Ann),
                    right_after: None,
                    ..cursor
                };
                let next = block.has_next.then(|| NearDupCursor {
                    version: 3,
                    scan_id: current.scan_id.clone(),
                    media: current.media.clone(),
                    mode: Some(NearDupMode::Ann),
                    after: block.end,
                    right_after: None,
                    component_offset: 0,
                });
                return Ok(NearDupWindow {
                    components,
                    generations: block.generations,
                    suppress_existing_overlap: current.after.is_some(),
                    current,
                    next,
                });
            }

            // An ANN cursor describes components and overlap history from one published graph.
            // Falling back mid-scan cannot be a lossless continuation: the exact component may
            // strictly contain a previously emitted approximate component, and suppressing that
            // overlap would hide its newly discovered members. Require a fresh scan instead of
            // returning a deceptively complete continuation.
            if cursor.mode == Some(NearDupMode::Ann) {
                return Err(LibError::BadRequest(
                    "near-duplicate ANN state changed; restart the scan without its cursor".into(),
                ));
            }
        }

        // No ANN page has been emitted for this scan, so pin the bounded exact-window fallback.
        let exact = NearDupCursor {
            version: 3,
            scan_id: cursor.scan_id,
            media: cursor.media,
            mode: Some(NearDupMode::Exact),
            after: None,
            right_after: None,
            component_offset: 0,
        };
        self.near_duplicate_exact_window(exact, media, vis, partial)
    }

    fn near_duplicate_exact_window(
        &self,
        mut cursor: NearDupCursor,
        media: Option<MediaType>,
        vis: &Visibility,
        partial: &mut dam_api::PartialStatus,
    ) -> Result<NearDupWindow, LibError> {
        cursor.mode = Some(NearDupMode::Exact);
        cursor.right_after = None;
        // Copy one bounded window under a SQLite snapshot, then release the guard before cosine
        // work. Within the 2,000-vector window this preserves the original exact connected-
        // component semantics. A larger catalog is keyset-paged and explicitly partial: combining
        // independently scored block pairs would manufacture overlapping, non-global components.
        let block = {
            let conn = self.read()?;
            load_near_dup_block_from_conn(
                &conn,
                media,
                vis,
                cursor.after.as_ref(),
                NEAR_DUP_SEED_PAGE,
            )?
        };

        if cursor.after.is_some() || block.has_next {
            partial.complete = false;
            partial.warnings.push(dam_api::ItemWarning {
                subject: "near-duplicates".into(),
                code: "duplicate_exact_seed_page".into(),
                message: format!(
                    "without a ready ANN base, near-duplicate components are exact within each {NEAR_DUP_SEED_PAGE}-asset window; cross-window relations may be omitted"
                ),
            });
        }
        let components = near_components(block.candidates);
        let next = block.has_next.then(|| NearDupCursor {
            version: 3,
            scan_id: cursor.scan_id.clone(),
            media: cursor.media.clone(),
            mode: Some(NearDupMode::Exact),
            after: block.end,
            right_after: None,
            component_offset: 0,
        });

        Ok(NearDupWindow {
            components,
            generations: block.generations,
            current: cursor,
            next,
            // Exact pages are disjoint, so suppression cannot hide an intra-page component.
            suppress_existing_overlap: true,
        })
    }

    /// Persist the exact bounded component membership that produced an opaque near-review id.
    /// Review mutations can then validate against the page the server actually returned instead
    /// of recomputing under a different ANN generation, visibility scope, or seed page.
    fn remember_near_duplicate_groups(
        &self,
        groups: &[(String, String, i64, Vec<AssetId>)],
        scan_id: &str,
        suppress_existing_overlap: bool,
    ) -> Result<std::collections::HashSet<String>, LibError> {
        if groups.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let recorded_at = now_ms();
        tx.execute(
            "DELETE FROM near_duplicate_review_snapshot WHERE created_at < ?1",
            params![recorded_at - 86_400_000],
        )
        .map_err(internal)?;
        tx.execute(
            "DELETE FROM near_duplicate_scan_member WHERE created_at < ?1",
            params![recorded_at - 86_400_000],
        )
        .map_err(internal)?;
        let mut saved = std::collections::HashSet::new();
        for (review, space, expected_generation, ids) in groups {
            let current_generation: Option<i64> = tx
                .query_row(
                    "SELECT generation FROM ann_space_state WHERE space_id=?1",
                    params![space],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            if current_generation != Some(*expected_generation) {
                continue;
            }
            let scan_generation_changed: bool = tx
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM near_duplicate_scan_member
                        WHERE scan_id=?1 AND space_id=?2 AND generation<>?3
                          AND created_at>=?4
                     )",
                    params![scan_id, space, expected_generation, recorded_at - 900_000],
                    |row| row.get(0),
                )
                .map_err(internal)?;
            if scan_generation_changed {
                return Err(LibError::BadRequest(
                    "near-duplicate ANN state changed; restart the scan without its cursor".into(),
                ));
            }
            if suppress_existing_overlap && !ids.is_empty() {
                let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "SELECT EXISTS(
                       SELECT 1 FROM near_duplicate_scan_member member
                      WHERE member.space_id=? AND member.generation=? AND member.scan_id=?
                        AND member.created_at>=? AND member.asset_id IN ({placeholders})
                     )"
                );
                let mut binds = vec![
                    Value::Text(space.clone()),
                    Value::Integer(*expected_generation),
                    Value::Text(scan_id.to_string()),
                    Value::Integer(recorded_at - 900_000),
                ];
                binds.extend(ids.iter().map(|id| Value::Blob(id.as_bytes().to_vec())));
                let overlaps: bool = tx
                    .query_row(&sql, rusqlite::params_from_iter(binds.iter()), |row| {
                        row.get(0)
                    })
                    .map_err(internal)?;
                if overlaps {
                    continue;
                }
            }
            tx.execute(
                "DELETE FROM near_duplicate_review_snapshot WHERE review_key=?1",
                params![review],
            )
            .map_err(internal)?;
            tx.execute(
                "INSERT INTO near_duplicate_review_snapshot
                     (review_key,scan_id,space_id,generation,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![review, scan_id, space, expected_generation, recorded_at],
            )
            .map_err(internal)?;
            let mut inserted = 0usize;
            for id in ids {
                inserted += tx
                    .execute(
                        "INSERT INTO near_duplicate_review_member(review_key,asset_id)
                     SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM asset WHERE id=?2)",
                        params![review, id.as_bytes().to_vec()],
                    )
                    .map_err(internal)?;
            }
            if inserted == ids.len() && inserted >= 2 {
                for id in ids {
                    tx.execute(
                        "INSERT INTO near_duplicate_scan_member
                             (scan_id,space_id,generation,asset_id,created_at)
                         VALUES(?1,?2,?3,?4,?5)
                         ON CONFLICT(scan_id,space_id,asset_id) DO UPDATE SET
                             generation=excluded.generation,
                             created_at=excluded.created_at",
                        params![
                            scan_id,
                            space,
                            expected_generation,
                            id.as_bytes().to_vec(),
                            recorded_at
                        ],
                    )
                    .map_err(internal)?;
                }
                saved.insert(review.clone());
            } else {
                tx.execute(
                    "DELETE FROM near_duplicate_review_snapshot WHERE review_key=?1",
                    params![review],
                )
                .map_err(internal)?;
            }
        }
        tx.commit().map_err(internal)?;
        Ok(saved)
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
        let conn = self.read()?;
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
        let conn = self.read()?;
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
        let member_details = Self::duplicate_members_for_ids(&conn, &ids, vis)?;
        let group = Self::build_dup_group_from_summaries(
            DupGroupSeed {
                kind: DupKind::Exact,
                ids: &ids,
                total_members: membership.count,
                group: Some(membership.group.clone()),
                members_cursor,
                signal: "identical bytes (same content hash)",
                review: &format!("exact:{}", membership.group),
            },
            &member_details,
        );
        group
            .map(|group| Self::apply_duplicate_review(&conn, group, DupReviewFilter::All))
            .transpose()
            .map(Option::flatten)
    }

    /// Continue one exact group's member summaries with a stable `(size, id)` keyset.
    pub fn duplicate_group_members(
        &self,
        req: &DupGroupMembersRequest,
        vis: &Visibility,
    ) -> Result<Page<DupMember>, LibError> {
        let hash = decode_hash_hex(&req.group)?;
        let after = decode_dup_member_cursor(req.after.as_ref())?;
        let limit = req.limit.clamp(1, DUP_GROUP_PAGE_MAX) as usize;
        let conn = self.read()?;
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
        let members = Self::duplicate_members_for_ids(&conn, &ids, vis)?;
        let items: Vec<DupMember> = ids
            .iter()
            .filter_map(|id| members.get(id).cloned())
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

    /// Build a `DupGroup` from member ids: load review-specific comparison metadata and pick the
    /// suggested keep by license, size, and modification time. Skips groups that collapse to fewer
    /// than two resolvable members —
    /// which also re-forms visibility-filtered groups (issue #42 leak audit): a duplicate pair
    /// spanning a shared and an unshared source collapses to one visible member, and a group of one
    /// is not a duplicate, so the hidden file's existence never shows.
    fn build_dup_group_from_summaries(
        seed: DupGroupSeed<'_>,
        map: &std::collections::HashMap<AssetId, DupMember>,
    ) -> Option<DupGroup> {
        let mut members: Vec<DupMember> = seed
            .ids
            .iter()
            .filter_map(|id| map.get(id).cloned())
            .collect();
        if members.len() < 2 {
            return None;
        }
        // Near components arrive in signal order; make their presentation stable and fidelity-led.
        if seed.kind == DupKind::Near {
            members.sort_by_key(|member| (std::cmp::Reverse(member.asset.size), member.asset.id));
        }
        let suggested_keep = members
            .iter()
            .max_by_key(|member| {
                (
                    license_rank(member.asset.license.status),
                    member.asset.size,
                    member.modified_at.unwrap_or(i64::MIN),
                    member.asset.id,
                )
            })
            .expect("duplicate group has members")
            .asset
            .id;
        let winner = members
            .iter()
            .find(|member| member.asset.id == suggested_keep)
            .expect("suggested keep is a member");
        let suggested_keep_reason = format!(
            "Best license ({}); ties prefer the larger file, newer modified date, then stable catalog order.",
            winner.asset.license.status.as_str()
        );
        let media = members[0].asset.media;
        Some(DupGroup {
            kind: seed.kind,
            media,
            group: seed.group,
            review: seed.review.to_string(),
            review_state: DupReviewState::Pending,
            members,
            total_members: seed.total_members,
            members_cursor: seed.members_cursor,
            signal: seed.signal.to_string(),
            suggested_keep,
            suggested_keep_reason,
            chosen_keep: None,
        })
    }

    fn apply_duplicate_review(
        conn: &Connection,
        mut group: DupGroup,
        filter: DupReviewFilter,
    ) -> Result<Option<DupGroup>, LibError> {
        let saved: Option<(String, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT state, chosen_keep FROM duplicate_review WHERE review_key = ?1",
                params![group.review],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        if let Some((state, keep)) = saved {
            group.review_state = match state.as_str() {
                "resolved" => DupReviewState::Resolved,
                "dismissed" => DupReviewState::Dismissed,
                _ => DupReviewState::Pending,
            };
            group.chosen_keep = keep.and_then(|bytes| {
                <[u8; 16]>::try_from(bytes.as_slice())
                    .ok()
                    .map(AssetId::from_bytes)
            });
        }
        let included = match filter {
            DupReviewFilter::All => true,
            DupReviewFilter::Pending => group.review_state == DupReviewState::Pending,
            DupReviewFilter::Resolved => group.review_state == DupReviewState::Resolved,
            DupReviewFilter::Dismissed => group.review_state == DupReviewState::Dismissed,
        };
        Ok(included.then_some(group))
    }

    /// Hydrate the metadata whose only consumer is duplicate review. Keeping these columns off
    /// `AssetSummary` avoids adding path/source/timestamps to every ordinary browse row.
    fn duplicate_members_for_ids(
        conn: &Connection,
        ids: &[AssetId],
        vis: &Visibility,
    ) -> Result<std::collections::HashMap<AssetId, DupMember>, LibError> {
        let mut map = std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds = Vec::new();
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        where_sql.push_str(&format!(" AND asset.id IN ({placeholders})"));
        binds.extend(ids.iter().map(|id| Value::Blob(id.as_bytes().to_vec())));
        let sql = format!(
            "{GRID_SELECT}, asset.path, source.name, asset.source_modified_at, asset.analysed_at{GRID_PENDING_SELECT}
               FROM asset JOIN source ON source.id = asset.source_id {ATTR_JOINS} {where_sql}"
        );
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok(DupMember {
                    asset: row_to_summary(row)?,
                    path: row.get(16)?,
                    source: row.get(17)?,
                    modified_at: row.get(18)?,
                    analyzed_at: row.get(19)?,
                })
            })
            .map_err(internal)?;
        for row in rows {
            let member = row.map_err(internal)?;
            map.insert(member.asset.id, member);
        }
        Ok(map)
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
        let sql = format!("{GRID_SELECT}{GRID_PENDING_SELECT} FROM asset {ATTR_JOINS} {where_sql}");
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

fn license_rank(status: LicenseStatus) -> u8 {
    match status {
        LicenseStatus::Permissive => 3,
        LicenseStatus::Attribution => 2,
        LicenseStatus::Unknown => 1,
        LicenseStatus::Restricted => 0,
    }
}

#[cfg(feature = "ann")]
fn load_near_dup_block(
    store: &Store,
    media: Option<MediaType>,
    vis: &Visibility,
    after: Option<&NearDupKey>,
    size: usize,
) -> Result<NearDupBlock, LibError> {
    let conn = store.read()?;
    load_near_dup_block_from_conn(&conn, media, vis, after, size)
}

fn load_near_dup_block_from_conn(
    conn: &Connection,
    media: Option<MediaType>,
    vis: &Visibility,
    after: Option<&NearDupKey>,
    size: usize,
) -> Result<NearDupBlock, LibError> {
    // Compare only embeddings from the same declared space and media. Ordering by the complete
    // `(media, space, id)` key makes both ANN seed pages and exact pair blocks stable keysets.
    let mut where_sql = String::from(" WHERE 1=1");
    let mut binds: Vec<Value> = Vec::new();
    if let Some(media) = media {
        where_sql.push_str(" AND e.media_type = ?");
        binds.push(Value::Text(media.as_str().to_string()));
    }
    push_visibility(vis, "a", &mut where_sql, &mut binds);
    if let Some(after) = after {
        let id = after.asset_id()?;
        where_sql.push_str(
            " AND (e.media_type > ? OR
                  (e.media_type = ? AND e.space_id > ?) OR
                  (e.media_type = ? AND e.space_id = ? AND e.asset_id > ?))",
        );
        binds.push(Value::Text(after.media.clone()));
        binds.push(Value::Text(after.media.clone()));
        binds.push(Value::Text(after.space.clone()));
        binds.push(Value::Text(after.media.clone()));
        binds.push(Value::Text(after.space.clone()));
        binds.push(Value::Blob(id.as_bytes().to_vec()));
    }
    let sql = format!(
        "SELECT e.asset_id, e.space_id, e.media_type, e.vec, state.generation
           FROM embedding e JOIN asset a ON a.id = e.asset_id
           JOIN ann_space_state state ON state.space_id=e.space_id
           {where_sql}
          ORDER BY e.media_type, e.space_id, e.asset_id LIMIT ?"
    );
    binds.push(Value::Integer((size + 1) as i64));

    let mut candidates = Vec::new();
    let mut generations = BTreeMap::new();
    let mut stmt = conn.prepare(&sql).map_err(internal)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(internal)?;
    for row in rows {
        let (id, space, media, vector, generation) = row.map_err(internal)?;
        generations.insert(space.clone(), generation);
        candidates.push((blob_to_asset_id(&id), space, media, bytes_to_f32(&vector)));
    }
    let has_next = candidates.len() > size;
    candidates.truncate(size);
    let end = candidates.last().map(NearDupKey::from_candidate);
    Ok(NearDupBlock {
        candidates,
        generations,
        has_next,
        end,
    })
}

fn near_components(candidates: Vec<NearCandidate>) -> Vec<NearComponent> {
    let mut partitions = NearPartitions::new();
    for (id, space, media, vector) in candidates {
        // Empty vectors carry no signal and must not become a partition whose cosine
        // implementation treats every pair as zero-length neighbours.
        if !vector.is_empty() {
            partitions
                .entry((media, space, vector.len()))
                .or_default()
                .push((id, vector));
        }
    }

    let mut computed = Vec::new();
    for ((media, space, _dimension), partition) in partitions {
        let mut uf = UnionFind::new(partition.len());
        for i in 0..partition.len() {
            for j in (i + 1)..partition.len() {
                if cosine(&partition[i].1, &partition[j].1) >= NEAR_DUP_COSINE {
                    uf.union(i, j);
                }
            }
        }
        for component in uf
            .components()
            .into_iter()
            .filter(|component| component.len() >= 2)
        {
            let mut ids: Vec<AssetId> = component
                .into_iter()
                .map(|index| partition[index].0)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            if ids.len() >= 2 {
                computed.push((media.clone(), space.clone(), ids));
            }
        }
    }
    computed.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    computed
}

/// Bounded HNSW edge generation followed by the same exact cosine threshold used by the scan
/// path. Only candidates already admitted by media + visibility SQL may form an edge, so hidden
/// assets cannot bridge two visible components. `None` requests exact fallback while a base is
/// being built or recovered.
#[cfg(feature = "ann")]
fn near_components_ann(
    store: &Store,
    candidates: &[NearCandidate],
    vis: &Visibility,
) -> Result<Option<(Vec<NearComponent>, bool)>, LibError> {
    near_components_ann_with(candidates, |media, space, dimension, vector, count| {
        ann_near_neighbors(store, media, space, dimension, vector, count, vis)
    })
}

#[cfg(feature = "ann")]
fn ann_near_neighbors(
    store: &Store,
    media: &str,
    space: &str,
    dimension: usize,
    vector: &[f32],
    count: usize,
    vis: &Visibility,
) -> Result<Option<Vec<NearNeighbour>>, LibError> {
    let Some(ids) = store.ann_candidate_ids(space, vector, count)? else {
        return Ok(None);
    };
    let mut vectors = Vec::new();
    {
        let conn = store.read()?;
        for chunk in ids.chunks(400) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut where_sql = format!(
                " WHERE e.space_id=? AND e.media_type=? AND LENGTH(e.vec)=?
                    AND e.asset_id IN ({placeholders})"
            );
            let mut binds = vec![
                Value::Text(space.to_string()),
                Value::Text(media.to_string()),
                Value::Integer((dimension.saturating_mul(4)) as i64),
            ];
            binds.extend(chunk.iter().map(|id| Value::Blob(id.as_bytes().to_vec())));
            push_visibility(vis, "a", &mut where_sql, &mut binds);
            let sql = format!(
                "SELECT e.asset_id,e.vec FROM embedding e JOIN asset a ON a.id=e.asset_id {where_sql}"
            );
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    Ok((
                        blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                        bytes_to_f32(&row.get::<_, Vec<u8>>(1)?),
                    ))
                })
                .map_err(internal)?;
            for row in rows {
                vectors.push(row.map_err(internal)?);
            }
        }
    }
    vectors.sort_by(|(left_id, left), (right_id, right)| {
        cosine(vector, right)
            .total_cmp(&cosine(vector, left))
            .then_with(|| left_id.cmp(right_id))
    });
    vectors.truncate(count);
    Ok(Some(vectors))
}

#[cfg(feature = "ann")]
fn near_components_ann_with(
    candidates: &[NearCandidate],
    mut find: impl FnMut(
        &str,
        &str,
        usize,
        &[f32],
        usize,
    ) -> Result<Option<Vec<NearNeighbour>>, LibError>,
) -> Result<Option<(Vec<NearComponent>, bool)>, LibError> {
    const ANN_NEIGHBOURS: usize = 32; // store overfetches to 256 base candidates, then exact tests
    let mut partitions = NearPartitions::new();
    for (id, space, media, vector) in candidates {
        if !vector.is_empty() {
            partitions
                .entry((media.clone(), space.clone(), vector.len()))
                .or_default()
                .push((*id, vector.clone()));
        }
    }

    let mut computed = Vec::new();
    let mut expansion_capped = false;
    for ((media, space, dimension), mut partition) in partitions {
        let mut positions: std::collections::HashMap<AssetId, usize> = partition
            .iter()
            .enumerate()
            .map(|(index, (id, _))| (*id, index))
            .collect();
        let mut uf = UnionFind::new(partition.len());
        let mut index = 0usize;
        let mut capped_nodes = std::collections::HashSet::new();
        while index < partition.len() {
            let vector = partition[index].1.clone();
            let Some(neighbours) = find(&media, &space, dimension, &vector, ANN_NEIGHBOURS)? else {
                return Ok(None);
            };
            for (candidate, candidate_vector) in neighbours {
                if candidate == partition[index].0
                    || cosine(&vector, &candidate_vector) < NEAR_DUP_COSINE
                {
                    continue;
                }
                let other = if let Some(&position) = positions.get(&candidate) {
                    position
                } else {
                    if partition.len() == NEAR_DUP_EXPANSION_MAX {
                        expansion_capped = true;
                        capped_nodes.insert(index);
                        continue;
                    }
                    let position = partition.len();
                    partition.push((candidate, candidate_vector));
                    positions.insert(candidate, position);
                    uf.push();
                    position
                };
                // kNN is directed: the higher-index point may be the only endpoint that returns
                // this edge. Union in either direction; UnionFind makes repeated edges harmless.
                uf.union(index, other);
            }
            index += 1;
        }
        let capped_roots: std::collections::HashSet<_> = capped_nodes
            .into_iter()
            .map(|index| uf.find(index))
            .collect();
        for component in uf.components() {
            if component.len() < 2 {
                continue;
            }
            let root = uf.find(component[0]);
            if capped_roots.contains(&root) {
                // Never return a component known to be truncated; unrelated components in this
                // partition remain valid and are still surfaced with an honest partial warning.
                continue;
            }
            let mut ids: Vec<_> = component
                .into_iter()
                .map(|index| partition[index].0)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            if ids.len() >= 2 {
                computed.push((media.clone(), space.clone(), ids));
            }
        }
    }
    computed.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    Ok(Some((computed, expansion_capped)))
}

fn near_review_id(space: &str, ids: &[AssetId]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(space.as_bytes());
    for id in ids {
        hash.update(id.as_bytes());
    }
    format!("near:{}", hash.finalize().to_hex())
}

/// Resolve a review id back to its current server-computed membership. Mutations must never trust
/// caller-supplied asset ids: an exact review proves the content hash, while a near review is
/// reconstructed from the same complete, media/space/dimension-partitioned signal set as listing.
fn duplicate_review_members(
    conn: &Connection,
    review: &str,
) -> Result<std::collections::BTreeSet<AssetId>, LibError> {
    if let Some(encoded_hash) = review.strip_prefix("exact:") {
        let hash = decode_hash_hex(encoded_hash)?;
        if hash.len() != 32 {
            return Err(LibError::BadRequest("invalid duplicate review id".into()));
        }
        let mut stmt = conn
            .prepare("SELECT id FROM asset WHERE content_hash = ?1 ORDER BY id")
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![hash], |row| {
                Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        let members = rows
            .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()
            .map_err(internal)?;
        if members.len() < 2 {
            return Err(LibError::BadRequest(
                "duplicate review group is no longer available".into(),
            ));
        }
        return Ok(members);
    }

    if !review.starts_with("near:") || review.len() != "near:".len() + 64 {
        return Err(LibError::BadRequest("invalid duplicate review id".into()));
    }
    let fresh: bool = conn
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM near_duplicate_review_snapshot snapshot
               JOIN ann_space_state state ON state.space_id=snapshot.space_id
              WHERE snapshot.review_key=?1 AND snapshot.generation=state.generation
                AND snapshot.created_at>=?2
             )",
            params![review, now_ms() - 900_000],
            |row| row.get(0),
        )
        .map_err(internal)?;
    if !fresh {
        return Err(LibError::BadRequest(
            "duplicate review group changed; refresh the review page".into(),
        ));
    }
    let mut stmt = conn
        .prepare(
            "SELECT member.asset_id
               FROM near_duplicate_review_member member
               JOIN asset ON asset.id=member.asset_id
              WHERE member.review_key=?1 ORDER BY member.asset_id",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![review], |row| {
            Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
        })
        .map_err(internal)?;
    let members = rows
        .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()
        .map_err(internal)?;
    if members.len() >= 2 {
        return Ok(members);
    }
    Err(LibError::BadRequest(
        "duplicate review group is no longer available".into(),
    ))
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

fn encode_near_dup_cursor(cursor: NearDupCursor) -> Result<Cursor, LibError> {
    let payload = serde_json::to_vec(&cursor).map_err(internal)?;
    Ok(Cursor(format!(
        "{NEAR_DUP_CURSOR_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    )))
}

fn decode_near_dup_cursor(
    cursor: Option<&Cursor>,
    media: Option<MediaType>,
) -> Result<NearDupCursor, LibError> {
    let expected_media = media.map(|value| value.as_str().to_string());
    let Some(Cursor(raw)) = cursor else {
        return Ok(NearDupCursor {
            version: 3,
            scan_id: Uuid::now_v7().to_string(),
            media: expected_media,
            mode: None,
            after: None,
            right_after: None,
            component_offset: 0,
        });
    };
    if raw.len() > 4_096 {
        return Err(LibError::BadRequest(
            "near-duplicate cursor is too large".into(),
        ));
    }
    let encoded = raw
        .strip_prefix(NEAR_DUP_CURSOR_PREFIX)
        .ok_or_else(|| LibError::BadRequest("invalid near-duplicate cursor version".into()))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| LibError::BadRequest("invalid near-duplicate cursor encoding".into()))?;
    let decoded: NearDupCursor = serde_json::from_slice(&bytes)
        .map_err(|_| LibError::BadRequest("invalid near-duplicate cursor payload".into()))?;
    let keys_valid = decoded
        .after
        .iter()
        .chain(decoded.right_after.iter())
        .all(|key| {
            key.asset_id().is_ok()
                && expected_media
                    .as_ref()
                    .is_none_or(|media| &key.media == media)
        });
    let mode_valid = match decoded.mode {
        Some(NearDupMode::Ann) => decoded.right_after.is_none(),
        Some(NearDupMode::Exact) => decoded.right_after.is_none(),
        None => false,
    };
    if decoded.version != 3
        || decoded.scan_id.parse::<Uuid>().is_err()
        || decoded.media != expected_media
        || !keys_valid
        || !mode_valid
        || decoded.component_offset > NEAR_DUP_EXPANSION_CURSOR_MAX
    {
        return Err(LibError::BadRequest(
            "near-duplicate cursor does not match this request".into(),
        ));
    }
    Ok(decoded)
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
    #[cfg(feature = "ann")]
    fn push(&mut self) {
        self.parent.push(self.parent.len());
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
    use dam_sources::SourceConnection;

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

    fn duplicate_request(kind: DupKind, media: Option<MediaType>) -> DupRequest {
        DupRequest {
            kind,
            media,
            limit: 20,
            after: None,
            review: DupReviewFilter::Pending,
        }
    }

    #[test]
    fn pending_member_projection_preserves_duplicate_metadata_columns() {
        let store = duplicate_store(1, 2);
        let page = store
            .query_assets_semantic(&QueryRequest::default(), None, &Visibility::Full)
            .unwrap();
        let pending_id = page.items[0].id;
        let before = store.get_asset(&pending_id).unwrap();
        let generation = store.begin_source_scan(&before.source_id).unwrap();
        store
            .apply_quick_discovery(
                &before.source_id,
                generation,
                &[crate::PendingDiscovery {
                    path: before.path.clone(),
                    size: 30,
                    modified_ms: Some(2),
                    media: Some(MediaType::Image),
                    format: Some("png".into()),
                }],
            )
            .unwrap();
        let current = store.get_asset(&pending_id).unwrap();
        let ids: Vec<_> = page.items.iter().map(|summary| summary.id).collect();
        let conn = store.read().unwrap();
        let members = Store::duplicate_members_for_ids(&conn, &ids, &Visibility::Full).unwrap();
        let member = &members[&pending_id];
        assert_eq!(member.path, before.path);
        assert_eq!(member.source, "duplicates");
        assert_eq!(member.modified_at, current.timestamps.modified);
        assert_eq!(member.analyzed_at, current.timestamps.analyzed);
        assert_eq!(
            member
                .asset
                .key_attrs
                .get("ingest_status")
                .map(String::as_str),
            Some("pending_verification")
        );
        assert!(!members[&page.items[1].id]
            .asset
            .key_attrs
            .contains_key("ingest_status"));
    }

    fn insert_test_asset(
        store: &Store,
        source_id: SourceId,
        path: &str,
        media_type: MediaType,
        content_hash: Option<ContentHash>,
    ) -> AssetId {
        store
            .upsert_asset(&NewAsset {
                source_id,
                path: path.into(),
                filename: path.into(),
                content_hash,
                size_bytes: Some(path.len() as i64),
                source_modified_at: Some(1_700_000_000_000),
                scanned_at: now_ms(),
                media_type,
                format: path.rsplit('.').next().unwrap_or("bin").into(),
            })
            .unwrap()
            .0
    }

    fn exact_fallback_boundary_store(count: usize) -> (Store, AssetId, AssetId) {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: format!("/exact-fallback-{count}"),
                },
                "exact fallback",
                false,
            )
            .unwrap();
        let mut ids: Vec<_> = (0..count)
            .map(|index| {
                insert_test_asset(
                    &store,
                    source,
                    &format!("asset-{index:05}.png"),
                    MediaType::Image,
                    None,
                )
            })
            .collect();
        ids.sort_unstable();
        let first = ids[0];
        let last = *ids.last().unwrap();
        for id in ids {
            let vector = if id == first || id == last {
                [1.0]
            } else {
                [-1.0]
            };
            store
                .set_embedding(&id, "exact-fallback", MediaType::Image, &vector, "test@1")
                .unwrap();
        }
        (store, first, last)
    }

    fn find_exact_fallback_pair(
        store: &Store,
        first: AssetId,
        last: AssetId,
        max_pages: usize,
    ) -> DupGroup {
        let wanted: std::collections::BTreeSet<_> = [first, last].into_iter().collect();
        let mut after = None;
        for _ in 0..max_pages {
            let page = store
                .duplicates(
                    &DupRequest {
                        after,
                        ..duplicate_request(DupKind::Near, Some(MediaType::Image))
                    },
                    &Visibility::Full,
                )
                .unwrap();
            assert!(
                page.partial.complete,
                "ordinary-catalog exact fallback must remain complete"
            );
            if let Some(group) = page.items.into_iter().find(|group| {
                group
                    .members
                    .iter()
                    .map(|member| member.asset.id)
                    .collect::<std::collections::BTreeSet<_>>()
                    == wanted
            }) {
                return group;
            }
            after = page.cursor;
            if after.is_none() {
                break;
            }
        }
        panic!("cross-block near-duplicate pair was not reachable")
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
                    review: DupReviewFilter::Pending,
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
                    review: DupReviewFilter::Pending,
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
                review: DupReviewFilter::Pending,
            },
            &Visibility::Full,
        );
        assert!(matches!(invalid, Err(LibError::BadRequest(_))));
    }

    #[test]
    fn exact_duplicate_review_covers_every_catalogued_media_type() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/all-media".into(),
                },
                "all media",
                false,
            )
            .unwrap();
        let media = [
            (MediaType::Image, "png"),
            (MediaType::Audio, "wav"),
            (MediaType::Model, "glb"),
            (MediaType::Video, "mp4"),
            (MediaType::Document, "pdf"),
        ];
        for (index, (kind, extension)) in media.into_iter().enumerate() {
            let hash = ContentHash([(index + 1) as u8; 32]);
            insert_test_asset(
                &store,
                source,
                &format!("{index}-a.{extension}"),
                kind,
                Some(hash),
            );
            insert_test_asset(
                &store,
                source,
                &format!("{index}-b.{extension}"),
                kind,
                Some(hash),
            );

            let page = store
                .duplicates(
                    &duplicate_request(DupKind::Exact, Some(kind)),
                    &Visibility::Full,
                )
                .unwrap();
            assert_eq!(page.items.len(), 1, "missing exact {kind:?} duplicate");
            assert_eq!(page.items[0].media, kind);
            assert_eq!(page.items[0].members.len(), 2);
        }
    }

    #[test]
    fn near_duplicate_review_only_compares_valid_media_space_signals() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-signals".into(),
                },
                "near signals",
                false,
            )
            .unwrap();
        let image = insert_test_asset(&store, source, "image-a.png", MediaType::Image, None);
        let document = insert_test_asset(&store, source, "document.pdf", MediaType::Document, None);
        let other_space =
            insert_test_asset(&store, source, "image-other.png", MediaType::Image, None);
        store
            .set_embedding(&image, "shared", MediaType::Image, &[1.0, 0.0], "test@1")
            .unwrap();
        store
            .set_embedding(
                &document,
                "shared",
                MediaType::Document,
                &[1.0, 0.0],
                "test@1",
            )
            .unwrap();
        store
            .set_embedding(
                &other_space,
                "other",
                MediaType::Image,
                &[1.0, 0.0],
                "test@1",
            )
            .unwrap();
        assert!(store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap()
            .items
            .is_empty());

        let matching = insert_test_asset(&store, source, "image-b.png", MediaType::Image, None);
        store
            .set_embedding(
                &matching,
                "shared",
                MediaType::Image,
                &[0.99, 0.01],
                "test@1",
            )
            .unwrap();
        let first = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        let second = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].review, second.items[0].review);
        let ids: std::collections::BTreeSet<_> = first.items[0]
            .members
            .iter()
            .map(|member| member.asset.id)
            .collect();
        assert_eq!(ids, [image, matching].into_iter().collect());
    }

    #[test]
    fn exact_fallback_is_complete_and_actionable_for_an_ordinary_catalog() {
        let (store, first, last) = exact_fallback_boundary_store(129);
        let group = find_exact_fallback_pair(&store, first, last, 3);
        store
            .review_duplicate(&DupReviewRequest {
                review: group.review,
                state: DupReviewState::Resolved,
                keep: Some(first),
                removals: Vec::new(),
            })
            .unwrap();
    }

    #[test]
    fn exact_fallback_marks_cross_window_relations_partial_at_scale() {
        let (store, first, last) = exact_fallback_boundary_store(2_002);
        let wanted: std::collections::BTreeSet<_> = [first, last].into_iter().collect();
        let first_page = store
            .duplicates(
                &duplicate_request(DupKind::Near, Some(MediaType::Image)),
                &Visibility::Full,
            )
            .unwrap();
        assert!(!first_page.partial.complete);
        assert!(first_page.partial.warnings.iter().any(|warning| {
            warning.code == "duplicate_exact_seed_page"
                && warning
                    .message
                    .contains("cross-window relations may be omitted")
        }));
        assert!(!first_page.items.iter().any(|group| {
            group
                .members
                .iter()
                .map(|member| member.asset.id)
                .collect::<std::collections::BTreeSet<_>>()
                == wanted
        }));
        let second_page = store
            .duplicates(
                &DupRequest {
                    after: first_page.cursor,
                    ..duplicate_request(DupKind::Near, Some(MediaType::Image))
                },
                &Visibility::Full,
            )
            .unwrap();
        assert!(!second_page.partial.complete);
    }

    #[test]
    fn near_duplicate_visibility_does_not_use_hidden_bridge_assets() {
        let store = Store::open_in_memory().unwrap();
        let visible_one = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-visible-one".into(),
                },
                "near visible one",
                false,
            )
            .unwrap();
        let hidden = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-hidden".into(),
                },
                "near hidden",
                false,
            )
            .unwrap();
        let visible_two = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-visible-two".into(),
                },
                "near visible two",
                false,
            )
            .unwrap();
        let first = insert_test_asset(&store, visible_one, "first.png", MediaType::Image, None);
        let bridge = insert_test_asset(&store, hidden, "bridge.png", MediaType::Image, None);
        let third = insert_test_asset(&store, visible_two, "third.png", MediaType::Image, None);
        for (id, vector) in [
            (first, [1.0, 0.0]),
            (bridge, [0.9553, 0.2955]),
            (third, [0.8253, 0.5646]),
        ] {
            store
                .set_embedding(&id, "bridge-space", MediaType::Image, &vector, "test@1")
                .unwrap();
        }
        assert_eq!(
            store
                .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
                .unwrap()
                .items[0]
                .total_members,
            3
        );

        let visibility = Visibility::Restricted(dam_api::service::VisibilityScope {
            sources: [visible_one, visible_two].into_iter().collect(),
            ..Default::default()
        });
        assert!(store
            .duplicates(&duplicate_request(DupKind::Near, None), &visibility)
            .unwrap()
            .items
            .is_empty());
    }

    #[test]
    fn near_duplicate_listing_reaches_spaces_after_the_old_global_window() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-complete-catalog".into(),
                },
                "near complete catalog",
                false,
            )
            .unwrap();
        for index in 0..2_001 {
            let id = insert_test_asset(
                &store,
                source,
                &format!("prefix-{index}.png"),
                MediaType::Image,
                None,
            );
            store
                .set_embedding(&id, "a-prefix", MediaType::Image, &[1.0, 0.0], "test@1")
                .unwrap();
        }
        let first = insert_test_asset(&store, source, "later-first.png", MediaType::Image, None);
        let second = insert_test_asset(&store, source, "later-second.png", MediaType::Image, None);
        for id in [first, second] {
            store
                .set_embedding(&id, "z-target", MediaType::Image, &[0.0, 1.0], "test@1")
                .unwrap();
        }

        let mut after = None;
        let mut found = false;
        for _ in 0..200 {
            let page = store
                .duplicates(
                    &DupRequest {
                        after,
                        ..duplicate_request(DupKind::Near, None)
                    },
                    &Visibility::Full,
                )
                .unwrap();
            found |= page.items.iter().any(|group| {
                let ids: std::collections::BTreeSet<_> =
                    group.members.iter().map(|member| member.asset.id).collect();
                ids == [first, second].into_iter().collect()
            });
            assert!(
                !page.partial.complete,
                "a paged exact fallback must disclose omitted cross-window relations"
            );
            after = page.cursor;
            if after.is_none() {
                break;
            }
        }
        assert!(found, "keyset pages must eventually reach the later space");
    }

    #[cfg(feature = "ann")]
    #[test]
    fn file_backed_ann_listing_review_token_remains_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/ann-review".into(),
                },
                "ann review",
                false,
            )
            .unwrap();
        let first = insert_test_asset(
            &store,
            source,
            "ann-review-first.png",
            MediaType::Image,
            None,
        );
        let second = insert_test_asset(
            &store,
            source,
            "ann-review-second.png",
            MediaType::Image,
            None,
        );
        store
            .set_embedding(
                &first,
                "ann-review",
                MediaType::Image,
                &[1.0, 0.0],
                "test@1",
            )
            .unwrap();
        store
            .set_embedding(
                &second,
                "ann-review",
                MediaType::Image,
                &[0.999, 0.001],
                "test@1",
            )
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            if store
                .ann_candidate_ids("ann-review", &[1.0, 0.0], 2)
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "ANN index did not become queryable"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        let page = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        assert_eq!(page.items.len(), 1);
        let group = &page.items[0];
        store
            .review_duplicate(&DupReviewRequest {
                review: group.review.clone(),
                state: DupReviewState::Resolved,
                keep: Some(group.suggested_keep),
                removals: Vec::new(),
            })
            .unwrap();
    }

    #[cfg(feature = "ann")]
    #[test]
    fn ann_cursor_requires_restart_when_its_base_is_unavailable() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/ann-cursor-restart".into(),
                },
                "ann cursor restart",
                false,
            )
            .unwrap();
        let asset = insert_test_asset(
            &store,
            source,
            "ann-cursor-restart.png",
            MediaType::Image,
            None,
        );
        store
            .set_embedding(
                &asset,
                "ann-cursor-restart",
                MediaType::Image,
                &[1.0, 0.0],
                "test@1",
            )
            .unwrap();
        let cursor = encode_near_dup_cursor(NearDupCursor {
            version: 3,
            scan_id: Uuid::now_v7().to_string(),
            media: None,
            mode: Some(NearDupMode::Ann),
            after: None,
            right_after: None,
            component_offset: 0,
        })
        .unwrap();
        let error = store
            .duplicates(
                &DupRequest {
                    after: Some(cursor),
                    ..duplicate_request(DupKind::Near, None)
                },
                &Visibility::Full,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            LibError::BadRequest(message)
                if message.contains("restart the scan without its cursor")
        ));
    }

    #[test]
    fn near_duplicate_scan_rejects_an_embedding_generation_change() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-generation-change".into(),
                },
                "near generation change",
                false,
            )
            .unwrap();
        let first = insert_test_asset(
            &store,
            source,
            "near-generation-first.png",
            MediaType::Image,
            None,
        );
        let second = insert_test_asset(
            &store,
            source,
            "near-generation-second.png",
            MediaType::Image,
            None,
        );
        for id in [first, second] {
            store
                .set_embedding(
                    &id,
                    "near-generation",
                    MediaType::Image,
                    &[1.0, 0.0],
                    "test@1",
                )
                .unwrap();
        }
        let generation = |store: &Store| {
            store
                .read()
                .unwrap()
                .query_row(
                    "SELECT generation FROM ann_space_state WHERE space_id='near-generation'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        let scan_id = Uuid::now_v7().to_string();
        let ids = vec![first, second];
        let first_review = near_review_id("near-generation", &ids);
        assert!(store
            .remember_near_duplicate_groups(
                &[(
                    first_review.clone(),
                    "near-generation".into(),
                    generation(&store),
                    ids.clone(),
                )],
                &scan_id,
                true,
            )
            .unwrap()
            .contains(&first_review));

        store
            .set_embedding(
                &second,
                "near-generation",
                MediaType::Image,
                &[0.999, 0.001],
                "test@1",
            )
            .unwrap();
        let error = store
            .remember_near_duplicate_groups(
                &[(
                    near_review_id("near-generation", &ids),
                    "near-generation".into(),
                    generation(&store),
                    ids,
                )],
                &scan_id,
                true,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            LibError::BadRequest(message)
                if message.contains("restart the scan without its cursor")
        ));
    }

    #[cfg(feature = "ann")]
    #[test]
    fn file_backed_ann_seed_pages_do_not_repeat_overlapping_groups() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/ann-pages".into(),
                },
                "ann pages",
                false,
            )
            .unwrap();
        for index in 0..(NEAR_DUP_SEED_PAGE + 1) {
            let id = insert_test_asset(
                &store,
                source,
                &format!("ann-page-{index}.png"),
                MediaType::Image,
                None,
            );
            store
                .set_embedding(&id, "ann-pages", MediaType::Image, &[1.0, 0.0], "test@1")
                .unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            if store
                .ann_candidate_ids("ann-pages", &[1.0, 0.0], 2)
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "ANN build timed out");
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        let first = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        assert_eq!(first.items.len(), 1);
        let independent = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        assert_eq!(independent.items.len(), 1, "a new scan is independent");
        let second = store
            .duplicates(
                &DupRequest {
                    after: first.cursor.clone(),
                    ..duplicate_request(DupKind::Near, None)
                },
                &Visibility::Full,
            )
            .unwrap();
        assert!(
            second.items.is_empty(),
            "later seed pages must suppress groups overlapping an earlier page"
        );
        assert!(second.cursor.is_none());
        let independent_second = store
            .duplicates(
                &DupRequest {
                    after: independent.cursor,
                    ..duplicate_request(DupKind::Near, None)
                },
                &Visibility::Full,
            )
            .unwrap();
        assert!(
            independent_second.items.is_empty(),
            "each scan retains its own overlap history"
        );
    }

    #[cfg(feature = "ann")]
    #[test]
    fn asymmetric_ann_edge_is_emitted_by_the_page_that_discovers_it() {
        let id = |suffix: u8| {
            let mut bytes = [0_u8; 16];
            bytes[15] = suffix;
            AssetId::from_bytes(bytes)
        };
        let first = id(1);
        let second = id(2);
        let vector = |asset: AssetId| match asset {
            value if value == first => vec![1.0, 0.0],
            _ => vec![0.99, 0.01],
        };
        let neighbours = |query: &[f32]| {
            if query == vector(first) {
                Vec::new()
            } else {
                vec![(first, vector(first))]
            }
        };

        let first_page = vec![(first, "space".into(), "image".into(), vector(first))];
        let first_components =
            near_components_ann_with(&first_page, |_media, _space, _dimension, query, _count| {
                Ok(Some(neighbours(query)))
            })
            .unwrap()
            .unwrap();
        assert!(first_components.0.is_empty());

        let second_page = vec![(second, "space".into(), "image".into(), vector(second))];
        let second_components =
            near_components_ann_with(&second_page, |_media, _space, _dimension, query, _count| {
                Ok(Some(neighbours(query)))
            })
            .unwrap()
            .unwrap();
        assert_eq!(second_components.0[0].2, vec![first, second]);
    }

    #[test]
    fn near_review_token_expires_when_its_embedding_space_changes() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-stale-review".into(),
                },
                "near stale review",
                false,
            )
            .unwrap();
        let first = insert_test_asset(&store, source, "first.png", MediaType::Image, None);
        let second = insert_test_asset(&store, source, "second.png", MediaType::Image, None);
        for id in [first, second] {
            store
                .set_embedding(&id, "stale-space", MediaType::Image, &[1.0, 0.0], "test@1")
                .unwrap();
        }
        let group = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap()
            .items
            .pop()
            .unwrap();
        store
            .set_embedding(
                &second,
                "stale-space",
                MediaType::Image,
                &[0.0, 1.0],
                "test@1",
            )
            .unwrap();
        let result = store.review_duplicate(&DupReviewRequest {
            review: group.review,
            state: DupReviewState::Resolved,
            keep: Some(first),
            removals: Vec::new(),
        });
        assert!(
            matches!(result, Err(LibError::BadRequest(message)) if message.contains("refresh"))
        );
    }

    #[test]
    fn duplicate_review_state_persists_and_rejects_cross_group_targets_atomically() {
        let store = duplicate_store(2, 2);
        let pending = store
            .duplicates(&duplicate_request(DupKind::Exact, None), &Visibility::Full)
            .unwrap();
        let group = &pending.items[0];
        let keep = group.members[0].asset.id;
        let remove = group.members[1].asset.id;
        let outside = pending.items[1].members[0].asset.id;

        let failed = store.review_duplicate(&DupReviewRequest {
            review: group.review.clone(),
            state: DupReviewState::Resolved,
            keep: Some(keep),
            removals: vec![
                DupReviewRemoval {
                    asset: remove,
                    block: false,
                },
                DupReviewRemoval {
                    asset: outside,
                    block: false,
                },
            ],
        });
        assert!(matches!(failed, Err(LibError::BadRequest(_))));
        assert!(
            store.get_asset(&remove).is_ok(),
            "failed decision must roll back deletion"
        );

        store
            .review_duplicate(&DupReviewRequest {
                review: group.review.clone(),
                state: DupReviewState::Resolved,
                keep: Some(keep),
                removals: Vec::new(),
            })
            .unwrap();
        assert!(store
            .duplicates(&duplicate_request(DupKind::Exact, None), &Visibility::Full)
            .unwrap()
            .items
            .iter()
            .all(|item| item.review != group.review));
        let resolved = store
            .duplicates(
                &DupRequest {
                    review: DupReviewFilter::Resolved,
                    ..duplicate_request(DupKind::Exact, None)
                },
                &Visibility::Full,
            )
            .unwrap();
        let saved = resolved
            .items
            .iter()
            .find(|item| item.review == group.review)
            .expect("resolved group survives navigation/refetch");
        assert_eq!(saved.chosen_keep, Some(keep));

        store
            .review_duplicate(&DupReviewRequest {
                review: group.review.clone(),
                state: DupReviewState::Pending,
                keep: Some(keep),
                removals: vec![DupReviewRemoval {
                    asset: remove,
                    block: false,
                }],
            })
            .unwrap();
        assert!(matches!(
            store.get_asset(&remove),
            Err(LibError::NotFound(_))
        ));
    }

    #[test]
    fn near_review_pagination_consumes_filtered_groups_without_repeating_cursor() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-pages".into(),
                },
                "near pages",
                false,
            )
            .unwrap();
        for space in ["a", "b", "c"] {
            for member in 0..2 {
                let id = insert_test_asset(
                    &store,
                    source,
                    &format!("{space}-{member}.png"),
                    MediaType::Image,
                    None,
                );
                store
                    .set_embedding(&id, space, MediaType::Image, &[1.0, 0.0], "test@1")
                    .unwrap();
            }
        }
        let all = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        assert_eq!(all.items.len(), 3);
        for group in all.items.iter().take(2) {
            store
                .review_duplicate(&DupReviewRequest {
                    review: group.review.clone(),
                    state: DupReviewState::Resolved,
                    keep: Some(group.suggested_keep),
                    removals: Vec::new(),
                })
                .unwrap();
        }

        let page = store
            .duplicates(
                &DupRequest {
                    limit: 1,
                    ..duplicate_request(DupKind::Near, None)
                },
                &Visibility::Full,
            )
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].review, all.items[2].review);
        assert!(page.cursor.is_none(), "all raw components were consumed");
    }

    #[test]
    fn duplicate_group_hydration_has_a_hard_member_cap() {
        let store = duplicate_store(1, DUP_GROUP_MEMBER_MAX + 7);
        let id = {
            let conn = store.read().unwrap();
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
        let mut seen: Vec<AssetId> = group.members.iter().map(|member| member.asset.id).collect();
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
            seen.extend(page.items.iter().map(|member| member.asset.id));
            cursor = page.cursor;
        }
        let expected: Vec<AssetId> = {
            let conn = store.read().unwrap();
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
}
