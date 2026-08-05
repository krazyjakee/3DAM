//! Tags and automatic suggestions (tech-spec 05 §1.4): interning names, the suggest →
//! accept/reject/undo lifecycle, manual edits, the vocabulary clients autocomplete from, and the
//! per-asset FTS side-index columns those writes maintain. Part of the `Store` impl.
use super::*;
use crate::helpers::*;

impl Store {
    pub(crate) fn load_tags(conn: &Connection, id_blob: &[u8]) -> Vec<TagRef> {
        let mut stmt = match conn.prepare(
            "SELECT t.name, at.state, at.source, at.confidence, at.explanation
             FROM asset_tag at JOIN tag t ON t.id = at.tag_id
             WHERE at.asset_id = ?1 ORDER BY at.state, t.name",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![id_blob], |r| {
            Ok(TagRef {
                name: r.get(0)?,
                state: match r.get::<_, String>(1)?.as_str() {
                    "confirmed" => SuggestionState::Confirmed,
                    "rejected" => SuggestionState::Rejected,
                    _ => SuggestionState::Pending,
                },
                source: r.get(2)?,
                confidence: r.get::<_, Option<f64>>(3)?.map(|v| v as f32),
                why: r.get(4)?,
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Rewrite an asset's `tags` FTS column to confirmed tag names only. Pending automation remains
    /// discoverable in Inspector but cannot silently power full-text results before review.
    /// Best-effort: an FTS hiccup must never sink the tag write that triggered it.
    pub(crate) fn reindex_asset_tags(conn: &Connection, id: &AssetId) {
        let _ = conn.execute(
            "UPDATE asset_fts SET tags = COALESCE((
                SELECT group_concat(t.name, ' ') FROM asset_tag at
                JOIN tag t ON t.id = at.tag_id
                WHERE at.asset_id = ?1 AND at.state = 'confirmed'), '')
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
        let conn = self.write();
        Self::set_document_text_in(&conn, id, text)
    }

    /// The index write on a caller-owned connection.
    pub(crate) fn set_document_text_in(
        conn: &Connection,
        id: &AssetId,
        text: &str,
    ) -> Result<(), LibError> {
        conn.execute(
            "UPDATE asset_fts SET text = ?2
             WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
            params![id.as_bytes().to_vec(), text],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Intern a tag name, returning its id (case-insensitive unique).
    pub(crate) fn intern_tag(conn: &Connection, name: &str) -> Result<Vec<u8>, LibError> {
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
        explanation: &str,
    ) -> Result<(), LibError> {
        let mut conn = self.write();
        // Intern + insert + reindex is three statements against three tables; a concurrent reader
        // must not see the `asset_tag` row before the index that describes it (db.rs).
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let tag_id = Self::intern_tag(&tx, name)?;
        Self::suggest_tag_in(&tx, id, &tag_id, confidence, extractor, explanation)?;
        Self::reindex_asset_tags(&tx, id);
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// The suggestion row itself, on a caller-owned connection and against an **already interned**
    /// tag id.
    ///
    /// Interning is the caller's job precisely because a batch has to hoist it: a `tag_id` read
    /// inside an item savepoint that later rolls back would be a dangling foreign key for every
    /// other item that reused it. The FTS reindex is likewise the caller's, so a batch can do it
    /// once per asset instead of once per suggestion.
    pub(crate) fn suggest_tag_in(
        conn: &Connection,
        id: &AssetId,
        tag_id: &[u8],
        confidence: f32,
        extractor: &str,
        explanation: &str,
    ) -> Result<(), LibError> {
        conn.execute(
            "INSERT INTO asset_tag
                (asset_id, tag_id, state, source, confidence, extractor, created_at, explanation)
             VALUES (?1, ?2, 'suggested', 'auto', ?3, ?4, ?5, ?6)
             ON CONFLICT(asset_id, tag_id) DO UPDATE SET
                confidence=excluded.confidence,
                extractor=excluded.extractor,
                explanation=excluded.explanation
             WHERE asset_tag.state = 'suggested' AND asset_tag.source = 'auto'",
            params![
                id.as_bytes().to_vec(),
                tag_id,
                confidence.clamp(0.0, 1.0) as f64,
                extractor,
                now_ms(),
                explanation
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Apply one valid state transition to an existing automatic suggestion. This deliberately
    /// cannot manufacture a user tag or review an arbitrary name: `Undo` returns a decided row to
    /// pending, while accept/reject operate only on pending rows.
    pub fn review_suggestion(
        &self,
        id: &AssetId,
        name: &str,
        action: ReviewAction,
    ) -> Result<(), LibError> {
        let conn = self.write();
        let current: Option<String> = conn
            .query_row(
                "SELECT at.state FROM asset_tag at JOIN tag t ON t.id = at.tag_id
                 WHERE at.asset_id = ?1 AND t.name = ?2 COLLATE NOCASE AND at.source = 'auto'",
                params![id.as_bytes().to_vec(), name],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?;
        let current =
            current.ok_or_else(|| LibError::NotFound(format!("automatic suggestion {name:?}")))?;
        let next = match (action, current.as_str()) {
            (ReviewAction::Accept, "suggested") => "confirmed",
            (ReviewAction::Reject, "suggested") => "rejected",
            (ReviewAction::Undo, "confirmed" | "rejected") => "suggested",
            _ => {
                return Err(LibError::BadRequest(format!(
                    "cannot {action:?} a {current} suggestion"
                )))
            }
        };
        conn.execute(
            "UPDATE asset_tag SET state = ?3 WHERE asset_id = ?1
             AND tag_id = (SELECT id FROM tag WHERE name = ?2 COLLATE NOCASE)",
            params![id.as_bytes().to_vec(), name, next],
        )
        .map_err(internal)?;
        Self::reindex_asset_tags(&conn, id);
        Ok(())
    }

    /// The subset of explicit ids reachable through one visibility ceiling. Used by bulk writes to
    /// distinguish unreadable targets from readable-but-read-only targets without disclosing either.
    pub fn visible_asset_ids(
        &self,
        ids: &[AssetId],
        vis: &Visibility,
    ) -> Result<std::collections::HashSet<AssetId>, LibError> {
        if ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let conn = self.read()?;
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds = Vec::new();
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        where_sql.push_str(&format!(" AND asset.id IN ({placeholders})"));
        binds.extend(ids.iter().map(|id| Value::Blob(id.as_bytes().to_vec())));
        let mut stmt = conn
            .prepare(&format!("SELECT asset.id FROM asset {where_sql}"))
            .map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()
            .map_err(internal)
    }

    pub fn list_tags(
        &self,
        prefix: Option<&str>,
        limit: u32,
        vis: &Visibility,
    ) -> Result<Vec<TagInfo>, LibError> {
        let conn = self.read()?;
        // Whole-source grants can be answered exactly by the maintained source×tag rows. Manual
        // collection grants remain on the visibility-join path below because a collection may
        // overlap a source grant and the union must count an asset once.
        if vis
            .restricted()
            .is_none_or(|scope| scope.collections.is_empty())
        {
            let mut binds = Vec::new();
            let (from, count, manual) = if let Some(scope) = vis.restricted() {
                if scope.sources.is_empty() {
                    return Ok(Vec::new());
                }
                let placeholders = scope
                    .sources
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                binds.extend(
                    scope
                        .sources
                        .iter()
                        .map(|id| Value::Blob(id.as_bytes().to_vec())),
                );
                (
                    format!(
                        "source_tag_stat ats JOIN tag t ON t.id = ats.tag_id \
                         WHERE ats.source_id IN ({placeholders})"
                    ),
                    "SUM(ats.asset_count)",
                    "SUM(ats.manual_count)",
                )
            } else {
                (
                    "tag_stat ats JOIN tag t ON t.id = ats.tag_id WHERE 1=1".into(),
                    "ats.asset_count",
                    "ats.manual_count",
                )
            };
            let mut prefix_sql = String::new();
            if let Some(prefix) = prefix.filter(|prefix| !prefix.is_empty()) {
                prefix_sql.push_str(" AND t.name LIKE ? ESCAPE '\\'");
                binds.push(Value::Text(format!("{}%", escape_like(prefix))));
            }
            binds.push(Value::Integer(limit.clamp(1, 50) as i64));
            let sql = format!(
                "SELECT t.name, {count} n, {manual} manual FROM {from}{prefix_sql}
                 GROUP BY t.id HAVING n > 0 ORDER BY n DESC, t.name ASC LIMIT ?"
            );
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    Ok(TagInfo {
                        name: row.get(0)?,
                        count: row.get::<_, i64>(1)?.max(0) as u64,
                        manual: row.get::<_, i64>(2)? > 0,
                    })
                })
                .map_err(internal)?;
            return rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal);
        }
        let mut where_sql = String::from(" WHERE at.state = 'confirmed'");
        let mut binds = Vec::new();
        if let Some(prefix) = prefix.filter(|prefix| !prefix.is_empty()) {
            where_sql.push_str(" AND t.name LIKE ? ESCAPE '\\'");
            binds.push(Value::Text(format!("{}%", escape_like(prefix))));
        }
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let sql = format!(
            "SELECT t.name, COUNT(*) n,
                    MAX(CASE WHEN at.source = 'user' THEN 1 ELSE 0 END) manual
             FROM asset_tag at JOIN tag t ON t.id = at.tag_id
             JOIN asset ON asset.id = at.asset_id {where_sql}
             GROUP BY t.id ORDER BY n DESC, t.name ASC LIMIT ?"
        );
        binds.push(Value::Integer(limit.clamp(1, 50) as i64));
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok(TagInfo {
                    name: row.get(0)?,
                    count: row.get::<_, i64>(1)?.max(0) as u64,
                    manual: row.get::<_, i64>(2)? != 0,
                })
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// Preview/apply manual tag deltas in one transaction. Removing is intentionally restricted to
    /// `source = 'user'`: automatic suggestions keep their separate accept/reject lifecycle.
    pub fn edit_manual_tags(
        &self,
        ids: &[AssetId],
        add: &[String],
        remove: &[String],
        dry_run: bool,
    ) -> Result<ManualTagEditOutcome, LibError> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let mut result = TagEditResult {
            matched: ids.len() as u64,
            ..TagEditResult::default()
        };
        let mut changed_assets = Vec::new();

        for id in ids {
            let mut changed = false;
            for name in add {
                let tag_id = Self::intern_tag(&tx, name)?;
                let existing: Option<(String, String)> = tx
                    .query_row(
                        "SELECT at.state, at.source FROM asset_tag at
                         WHERE at.asset_id = ?1 AND at.tag_id = ?2",
                        params![id.as_bytes().to_vec(), &tag_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(internal)?;
                if existing
                    .as_ref()
                    .is_some_and(|(state, source)| state == "confirmed" && source == "user")
                {
                    continue;
                }
                tx.execute(
                    "INSERT INTO asset_tag
                       (asset_id, tag_id, state, source, confidence, extractor, created_at)
                     VALUES (?1, ?2, 'confirmed', 'user', NULL, NULL, ?3)
                     ON CONFLICT(asset_id, tag_id) DO UPDATE SET
                       state = 'confirmed', source = 'user', confidence = NULL, extractor = NULL,
                       explanation = NULL",
                    params![id.as_bytes().to_vec(), tag_id, now_ms()],
                )
                .map_err(internal)?;
                result.additions += 1;
                changed = true;
            }
            for name in remove {
                let removed = tx
                    .execute(
                        "DELETE FROM asset_tag WHERE asset_id = ?1 AND source = 'user'
                         AND tag_id = (SELECT id FROM tag WHERE name = ?2 COLLATE NOCASE)",
                        params![id.as_bytes().to_vec(), name],
                    )
                    .map_err(internal)?;
                if removed > 0 {
                    result.removals += 1;
                    changed = true;
                }
            }
            if changed {
                result.changed += 1;
                Self::reindex_asset_tags(&tx, id);
                let source = tx
                    .query_row(
                        "SELECT source_id FROM asset WHERE id = ?1",
                        params![id.as_bytes().to_vec()],
                        |row| Ok(blob_to_source_id(&row.get::<_, Vec<u8>>(0)?)),
                    )
                    .optional()
                    .map_err(internal)?;
                changed_assets.push((*id, source));
            }
        }
        if !dry_run {
            tx.commit().map_err(internal)?;
        }
        Ok(ManualTagEditOutcome {
            result,
            changed_assets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::service::VisibilityScope;
    use dam_sources::SourceConnection;

    /// A store holding exactly one image asset — the subject of every tag test below.
    fn tag_store() -> Store {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tag-test".into(),
                },
                "tags",
                false,
            )
            .unwrap();
        store
            .upsert_asset(&NewAsset {
                source_id: source,
                path: "0/0.png".into(),
                filename: "0.png".into(),
                content_hash: Some(ContentHash([0_u8; 32])),
                size_bytes: Some(1),
                source_modified_at: None,
                scanned_at: now_ms(),
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        store
    }

    fn first_asset(store: &Store) -> AssetId {
        let conn = store.read().unwrap();
        conn.query_row("SELECT id FROM asset LIMIT 1", [], |row| {
            Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
        })
        .unwrap()
    }

    #[test]
    fn manual_tag_edit_preview_is_reversible_idempotent_and_preserves_suggestions() {
        let store = tag_store();
        let id = first_asset(&store);
        store
            .suggest_tag(&id, "automatic", 0.8, "test@1", "test reason")
            .unwrap();

        let preview = store
            .edit_manual_tags(&[id], &["manual".into()], &["automatic".into()], true)
            .unwrap()
            .result;
        assert_eq!(
            (preview.changed, preview.additions, preview.removals),
            (1, 1, 0)
        );
        let tags = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes())
        };
        assert_eq!(
            tags.len(),
            1,
            "dry-run must roll back tag rows and FTS changes"
        );
        assert_eq!(tags[0].source, "auto");

        let added = store
            .edit_manual_tags(&[id], &["manual".into()], &[], false)
            .unwrap()
            .result;
        assert_eq!((added.changed, added.additions), (1, 1));
        let again = store
            .edit_manual_tags(&[id], &["manual".into()], &[], false)
            .unwrap()
            .result;
        assert_eq!((again.changed, again.additions), (0, 0));

        let removed = store
            .edit_manual_tags(&[id], &[], &["manual".into(), "automatic".into()], false)
            .unwrap()
            .result;
        assert_eq!((removed.changed, removed.removals), (1, 1));
        let tags = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes())
        };
        assert_eq!(tags.len(), 1);
        assert_eq!(
            tags[0].name, "automatic",
            "manual remove cannot remove auto tags"
        );
    }

    #[test]
    fn manually_adding_an_auto_tag_converts_authorship_and_vocabulary_is_confirmed_only() {
        let store = tag_store();
        let id = first_asset(&store);
        store
            .suggest_tag(&id, "convert-me", 0.9, "test@1", "test reason")
            .unwrap();
        assert!(
            store
                .list_tags(None, 20, &Visibility::Full)
                .unwrap()
                .is_empty(),
            "suggestions are not manual-tag autocomplete vocabulary"
        );

        store
            .edit_manual_tags(&[id], &["convert-me".into()], &[], false)
            .unwrap();
        let tags = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes())
        };
        assert_eq!(tags[0].source, "user");
        assert_eq!(tags[0].state, SuggestionState::Confirmed);
        assert_eq!(tags[0].confidence, None);
        assert_eq!(tags[0].why, None);
        let vocabulary = store
            .list_tags(Some("convert"), 20, &Visibility::Full)
            .unwrap();
        assert_eq!(vocabulary.len(), 1);
        assert!(vocabulary[0].manual);
    }

    #[test]
    fn suggestion_decisions_survive_reanalysis_and_undo_reopens_pending_metadata() {
        let store = tag_store();
        let id = first_asset(&store);
        store
            .suggest_tag(&id, "texture", 0.6, "image@1", "v1 visual classifier")
            .unwrap();
        store
            .review_suggestion(&id, "texture", ReviewAction::Accept)
            .unwrap();

        // A newer extractor may refresh undecided evidence, but cannot overwrite a human decision.
        store
            .suggest_tag(&id, "texture", 0.95, "image@2", "v2 visual classifier")
            .unwrap();
        let decided = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes()).remove(0)
        };
        assert_eq!(decided.state, SuggestionState::Confirmed);
        assert_eq!(decided.confidence, Some(0.6));
        assert_eq!(decided.why.as_deref(), Some("v1 visual classifier"));

        store
            .review_suggestion(&id, "texture", ReviewAction::Undo)
            .unwrap();
        store
            .suggest_tag(&id, "texture", 0.95, "image@2", "v2 visual classifier")
            .unwrap();
        let reopened = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes()).remove(0)
        };
        assert_eq!(reopened.state, SuggestionState::Pending);
        assert_eq!(reopened.confidence, Some(0.95));
        assert_eq!(reopened.why.as_deref(), Some("v2 visual classifier"));

        assert!(matches!(
            store.review_suggestion(&id, "not-proposed", ReviewAction::Accept),
            Err(LibError::NotFound(_))
        ));
        store
            .review_suggestion(&id, "texture", ReviewAction::Reject)
            .unwrap();
        assert!(matches!(
            store.review_suggestion(&id, "texture", ReviewAction::Reject),
            Err(LibError::BadRequest(_))
        ));
    }

    #[test]
    fn tag_vocabulary_escapes_prefix_wildcards_and_applies_visibility() {
        let store = Store::open_in_memory().unwrap();
        let visible_source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/visible".into(),
                },
                "visible",
                false,
            )
            .unwrap();
        let hidden_source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/hidden".into(),
                },
                "hidden",
                false,
            )
            .unwrap();
        let insert = |source_id, path: &str| {
            store
                .upsert_asset(&NewAsset {
                    source_id,
                    path: path.into(),
                    filename: path.into(),
                    content_hash: Some(ContentHash([path.as_bytes()[0]; 32])),
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: MediaType::Image,
                    format: "png".into(),
                })
                .unwrap()
                .0
        };
        let visible = insert(visible_source, "visible.png");
        let hidden = insert(hidden_source, "hidden.png");
        store
            .edit_manual_tags(&[visible], &["100%real".into(), "100x".into()], &[], false)
            .unwrap();
        store
            .edit_manual_tags(&[hidden], &["100%secret".into()], &[], false)
            .unwrap();
        let visibility = Visibility::Restricted(VisibilityScope {
            sources: [visible_source].into_iter().collect(),
            ..VisibilityScope::default()
        });
        let tags = store.list_tags(Some("100%"), 20, &visibility).unwrap();
        assert_eq!(
            tags.len(),
            1,
            "LIKE wildcards must be escaped and hidden tags omitted"
        );
        assert_eq!(tags[0].name, "100%real");
        assert_eq!(tags[0].count, 1);
    }
}
