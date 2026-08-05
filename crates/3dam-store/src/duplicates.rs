//! Duplicate detection and review (tech-spec 05 §4): exact content-hash groups, near-duplicate
//! embedding components, the cursors that page both, and the durable review decisions applied on
//! top. Part of the `Store` impl.
use super::*;
use crate::helpers::*;
use crate::similarity::{bytes_to_f32, cosine};
use std::collections::BTreeMap;

const NEAR_DUP_COSINE: f32 = 0.92;
const NEAR_DUP_CANDIDATE_MAX: usize = 2_000;
type NearCandidate = (AssetId, String, String, Vec<f32>);
type NearComponent = (String, String, Vec<AssetId>);
type NearPartitions = BTreeMap<(String, String, usize), Vec<(AssetId, Vec<f32>)>>;

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
        let has_more;
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
                let offset = decode_near_dup_cursor(req.after.as_ref(), NEAR_DUP_CANDIDATE_MAX)?;
                // Compare only embeddings from the same declared space and media. The old flat
                // candidate list compared unrelated dimensions (for example image stats against
                // document text) whenever "All media" was selected, manufacturing groups from a
                // signal that had no meaning. A media appears in near review only if it has rows in
                // a real embedding space; exact review remains independent and covers all five.
                let mut sql = String::from(
                    "SELECT e.asset_id, e.space_id, e.media_type, e.vec
                       FROM embedding e JOIN asset a ON a.id = e.asset_id",
                );
                let mut where_sql = String::from(" WHERE 1=1");
                let mut binds: Vec<Value> = Vec::new();
                if let Some(m) = req.media {
                    where_sql.push_str(" AND e.media_type = ?");
                    binds.push(Value::Text(m.as_str().to_string()));
                }
                push_visibility(vis, "a", &mut where_sql, &mut binds);
                sql.push_str(&where_sql);
                sql.push_str(" ORDER BY e.media_type, e.space_id, e.asset_id LIMIT ?");
                binds.push(Value::Integer((NEAR_DUP_CANDIDATE_MAX + 1) as i64));
                // Scoped: the pairwise union-find below is CPU work over up to
                // `NEAR_DUP_CANDIDATE_MAX` vectors, and a read guard pins a WAL snapshot for as long
                // as it lives. The candidates are owned by then, so the connection goes back to the
                // pool first and a second guard hydrates the surviving members.
                let mut candidates = Vec::new();
                {
                    let conn = self.read()?;
                    let mut stmt = conn.prepare(&sql).map_err(internal)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                            Ok((
                                r.get::<_, Vec<u8>>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, Vec<u8>>(3)?,
                            ))
                        })
                        .map_err(internal)?;
                    for r in rows {
                        let (id_blob, space, media, vbytes) = r.map_err(internal)?;
                        candidates.push((
                            blob_to_asset_id(&id_blob),
                            space,
                            media,
                            bytes_to_f32(&vbytes),
                        ));
                    }
                }
                if candidates.len() > NEAR_DUP_CANDIDATE_MAX {
                    candidates.truncate(NEAR_DUP_CANDIDATE_MAX);
                    partial.complete = false;
                    partial.warnings.push(dam_api::ItemWarning {
                        subject: "near-duplicates".into(),
                        code: "duplicate_candidates_capped".into(),
                        message: format!(
                            "near-duplicate analysis is capped at {NEAR_DUP_CANDIDATE_MAX} candidates"
                        ),
                    });
                }

                let computed = near_components(candidates);
                let remaining = computed.get(offset..).unwrap_or_default();
                let all_ids: Vec<AssetId> = remaining
                    .iter()
                    .flat_map(|(_, _, ids)| ids.iter().take(DUP_GROUP_MEMBER_MAX).copied())
                    .collect();
                let conn = self.read()?;
                let member_details = Self::duplicate_members_for_ids(&conn, &all_ids, vis)?;
                let mut consumed = 0usize;
                for (_media, space, ids) in remaining {
                    consumed += 1;
                    let member_ids: Vec<AssetId> =
                        ids.iter().take(DUP_GROUP_MEMBER_MAX).copied().collect();
                    let review = near_review_id(space, ids);
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
                let next_offset = offset.saturating_add(consumed);
                has_more = next_offset < computed.len();
                if has_more {
                    next_cursor = Some(Cursor(format!("near:{next_offset}")));
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
            "{GRID_SELECT}, asset.path, source.name, asset.source_modified_at, asset.analysed_at
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

fn license_rank(status: LicenseStatus) -> u8 {
    match status {
        LicenseStatus::Permissive => 3,
        LicenseStatus::Attribution => 2,
        LicenseStatus::Unknown => 1,
        LicenseStatus::Restricted => 0,
    }
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
/// reconstructed from the same bounded, media/space/dimension-partitioned signal set as listing.
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
    let mut stmt = conn
        .prepare(
            "SELECT e.asset_id, e.space_id, e.media_type, e.vec
             FROM embedding e JOIN asset a ON a.id = e.asset_id
             ORDER BY e.media_type, e.space_id, e.asset_id LIMIT ?1",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![(NEAR_DUP_CANDIDATE_MAX + 1) as i64], |row| {
            Ok((
                blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                bytes_to_f32(&row.get::<_, Vec<u8>>(3)?),
            ))
        })
        .map_err(internal)?;
    let mut candidates = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    candidates.truncate(NEAR_DUP_CANDIDATE_MAX);
    for (_media, space, ids) in near_components(candidates) {
        if near_review_id(&space, &ids) == review {
            return Ok(ids.into_iter().collect());
        }
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
