//! Collections and smart-folder membership/summaries — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    // ── collections / smart folders ───────────────────────────────────────────

    pub fn create_collection(
        &self,
        name: &str,
        kind: CollectionKind,
        query_json: Option<&str>,
    ) -> Result<CollectionId, LibError> {
        let id = CollectionId::new();
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO collection (id, name, kind, query, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id.as_bytes().to_vec(), name, kind.as_str(), query_json, now],
        )
        .map_err(internal)?;
        Ok(id)
    }

    pub fn list_collections(&self) -> Result<Vec<Collection>, LibError> {
        self.list_collections_vis(&Visibility::Full)
    }

    /// Collections reachable under a visibility ceiling (issue #42 rule 5): a collection is present
    /// iff it was shared directly, **or** it is a *view over* assets the caller can already reach
    /// (≥1 member in a readable source). A shared collection's member count is its real count; for
    /// a source-derived view the count still reflects all members — the assets themselves stay
    /// filtered by the query path, so no hidden asset is enumerable through it.
    pub fn list_collections_vis(&self, vis: &Visibility) -> Result<Vec<Collection>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut where_sql = String::new();
        let mut binds: Vec<Value> = Vec::new();
        if let Some(scope) = vis.restricted() {
            let mut arms: Vec<String> = vec!["0=1".into()];
            if !scope.collections.is_empty() {
                let ph = scope
                    .collections
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                arms.push(format!("collection.id IN ({ph})"));
                for c in &scope.collections {
                    binds.push(Value::Blob(c.as_bytes().to_vec()));
                }
            }
            if !scope.sources.is_empty() {
                let ph = scope
                    .sources
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                arms.push(format!(
                    "EXISTS (SELECT 1 FROM collection_member m JOIN asset a ON a.id = m.asset_id
                             WHERE m.collection_id = collection.id AND a.source_id IN ({ph}))"
                ));
                for s in &scope.sources {
                    binds.push(Value::Blob(s.as_bytes().to_vec()));
                }
            }
            where_sql = format!(" WHERE ({})", arms.join(" OR "));
        }
        let sql = format!(
            "SELECT id, name, kind, query, created_at, updated_at,
                    (SELECT COUNT(*) FROM collection_member m WHERE m.collection_id = collection.id)
             FROM collection{where_sql} ORDER BY name COLLATE NOCASE"
        );
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                Self::row_to_collection(r, true)
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// Whether one collection is reachable under a ceiling — the single-resource form of
    /// [`Self::list_collections_vis`], used by the engine's `get_collection`/`collection_assets`
    /// guards so an unreachable collection is a plain `NotFound` (absent, not forbidden).
    pub fn collection_visible(
        &self,
        id: &CollectionId,
        vis: &Visibility,
    ) -> Result<bool, LibError> {
        let Some(scope) = vis.restricted() else {
            return Ok(true);
        };
        if scope.collections.contains(id) {
            return Ok(true);
        }
        if scope.sources.is_empty() {
            return Ok(false);
        }
        let conn = self.conn.lock().unwrap();
        let ph = scope
            .sources
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let mut binds: Vec<Value> = vec![Value::Blob(id.as_bytes().to_vec())];
        for s in &scope.sources {
            binds.push(Value::Blob(s.as_bytes().to_vec()));
        }
        let sql = format!(
            "SELECT 1 FROM collection_member m JOIN asset a ON a.id = m.asset_id
             WHERE m.collection_id = ?1 AND a.source_id IN ({ph}) LIMIT 1"
        );
        let hit: Option<i64> = conn
            .query_row(&sql, rusqlite::params_from_iter(binds.iter()), |r| r.get(0))
            .optional()
            .map_err(internal)?;
        Ok(hit.is_some())
    }

    pub fn get_collection(&self, id: &CollectionId) -> Result<Collection, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, kind, query, created_at, updated_at,
                    (SELECT COUNT(*) FROM collection_member m WHERE m.collection_id = collection.id)
             FROM collection WHERE id = ?1",
            params![id.as_bytes().to_vec()],
            |r| Self::row_to_collection(r, true),
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| LibError::NotFound(format!("collection {id}")))
    }

    /// `manual_count`: whether column 6 holds the member count (used for manual collections; a smart
    /// folder's live count is computed by the caller by running its query).
    fn row_to_collection(r: &rusqlite::Row, manual_count: bool) -> rusqlite::Result<Collection> {
        let id = CollectionId::from_bytes(
            <[u8; 16]>::try_from(r.get::<_, Vec<u8>>(0)?.as_slice()).unwrap_or([0; 16]),
        );
        let name: String = r.get(1)?;
        let kind_s: String = r.get(2)?;
        let query_s: Option<String> = r.get(3)?;
        let created_at: i64 = r.get(4)?;
        let updated_at: i64 = r.get(5)?;
        let member_count: i64 = r.get(6)?;
        let kind = CollectionKind::parse(&kind_s).unwrap_or(CollectionKind::Manual);
        let query = query_s
            .as_deref()
            .and_then(|s| serde_json::from_str::<QueryRequest>(s).ok());
        // Manual folders carry the exact member count; smart folders leave it for the caller (§6.4).
        let count = if manual_count && kind == CollectionKind::Manual {
            Some(member_count as u64)
        } else {
            None
        };
        Ok(Collection {
            id,
            name,
            kind,
            query,
            count,
            created_at,
            updated_at,
        })
    }

    pub fn update_collection(
        &self,
        id: &CollectionId,
        name: Option<&str>,
        query_json: Option<&str>,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "UPDATE collection
                 SET name = COALESCE(?2, name),
                     query = COALESCE(?3, query),
                     updated_at = ?4
                 WHERE id = ?1",
                params![id.as_bytes().to_vec(), name, query_json, now_ms()],
            )
            .map_err(internal)?;
        if n == 0 {
            return Err(LibError::NotFound(format!("collection {id}")));
        }
        Ok(())
    }

    pub fn delete_collection(&self, id: &CollectionId) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "DELETE FROM collection WHERE id = ?1",
                params![id.as_bytes().to_vec()],
            )
            .map_err(internal)?;
        if n == 0 {
            return Err(LibError::NotFound(format!("collection {id}")));
        }
        Ok(())
    }

    /// Add/remove members of a manual collection in one transaction. Idempotent.
    pub fn modify_collection_members(
        &self,
        id: &CollectionId,
        add: &[AssetId],
        remove: &[AssetId],
    ) -> Result<(), LibError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(internal)?;
        // The collection must exist and be manual (a smart folder's set is query-driven).
        let kind: Option<String> = tx
            .query_row(
                "SELECT kind FROM collection WHERE id = ?1",
                params![id.as_bytes().to_vec()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        match kind.as_deref() {
            None => return Err(LibError::NotFound(format!("collection {id}"))),
            Some("smart") => {
                return Err(LibError::BadRequest(
                    "a smart folder's membership is query-driven and cannot be edited".into(),
                ))
            }
            _ => {}
        }
        let now = now_ms();
        for a in add {
            tx.execute(
                "INSERT OR IGNORE INTO collection_member (collection_id, asset_id, added_at)
                 VALUES (?1, ?2, ?3)",
                params![id.as_bytes().to_vec(), a.as_bytes().to_vec(), now],
            )
            .map_err(internal)?;
        }
        for a in remove {
            tx.execute(
                "DELETE FROM collection_member WHERE collection_id = ?1 AND asset_id = ?2",
                params![id.as_bytes().to_vec(), a.as_bytes().to_vec()],
            )
            .map_err(internal)?;
        }
        tx.commit().map_err(internal)?;
        Ok(())
    }

    /// Members of a manual collection as grid summaries, newest-added first (bounded by `limit`).
    /// The visibility ceiling filters members: a shared collection's own members all pass (the
    /// collection id is in the ceiling), while a source-derived collection view shows only the
    /// members from readable sources.
    pub fn collection_summaries(
        &self,
        id: &CollectionId,
        limit: u32,
        vis: &Visibility,
    ) -> Result<Vec<AssetSummary>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut vis_sql = String::new();
        let mut vis_binds: Vec<Value> = Vec::new();
        push_visibility(vis, "asset", &mut vis_sql, &mut vis_binds);
        let sql = format!(
            "{GRID_SELECT} FROM collection_member cm JOIN asset ON asset.id = cm.asset_id {ATTR_JOINS} \
             WHERE cm.collection_id = ?{vis_sql} ORDER BY cm.added_at DESC, asset.id ASC LIMIT ?"
        );
        let mut binds: Vec<Value> = vec![Value::Blob(id.as_bytes().to_vec())];
        binds.extend(vis_binds);
        binds.push(Value::Integer(limit.min(QUERY_MAX_LIMIT) as i64));
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), row_to_summary)
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// Whether one asset is reachable under a ceiling: readable source, or membership in a readable
    /// collection. The single-asset guard behind `get_asset`/`read_content`/thumbnail/preview —
    /// an unreachable asset answers `NotFound`, indistinguishable from a nonexistent one.
    pub fn asset_visible(&self, id: &AssetId, vis: &Visibility) -> Result<bool, LibError> {
        if vis.restricted().is_none() {
            return Ok(true);
        }
        let conn = self.conn.lock().unwrap();
        let mut where_sql = String::from("SELECT 1 FROM asset WHERE asset.id = ?");
        let mut binds: Vec<Value> = vec![Value::Blob(id.as_bytes().to_vec())];
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let hit: Option<i64> = conn
            .query_row(&where_sql, rusqlite::params_from_iter(binds.iter()), |r| {
                r.get(0)
            })
            .optional()
            .map_err(internal)?;
        Ok(hit.is_some())
    }

    /// Collections that contain an asset (manual membership) — surfaced on the inspector record.
    pub fn collections_for_asset(&self, id: &AssetId) -> Result<Vec<CollectionId>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT collection_id FROM collection_member WHERE asset_id = ?1")
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![id.as_bytes().to_vec()], |r| {
                Ok(CollectionId::from_bytes(
                    <[u8; 16]>::try_from(r.get::<_, Vec<u8>>(0)?.as_slice()).unwrap_or([0; 16]),
                ))
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }
}
