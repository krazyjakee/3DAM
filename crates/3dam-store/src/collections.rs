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
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, kind, query, created_at, updated_at,
                        (SELECT COUNT(*) FROM collection_member m WHERE m.collection_id = collection.id)
                 FROM collection ORDER BY name COLLATE NOCASE",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| Self::row_to_collection(r, true))
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(internal)?);
        }
        Ok(out)
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
    pub fn collection_summaries(
        &self,
        id: &CollectionId,
        limit: u32,
    ) -> Result<Vec<AssetSummary>, LibError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "{GRID_SELECT} FROM collection_member cm JOIN asset ON asset.id = cm.asset_id {ATTR_JOINS} \
             WHERE cm.collection_id = ?1 ORDER BY cm.added_at DESC, asset.id ASC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(
                params![id.as_bytes().to_vec(), limit.min(QUERY_MAX_LIMIT) as i64],
                row_to_summary,
            )
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(internal)?);
        }
        Ok(out)
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(internal)?);
        }
        Ok(out)
    }
}
