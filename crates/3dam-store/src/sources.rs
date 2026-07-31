//! Source registration, delta path index, and scan bookkeeping — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    // ── sources ──────────────────────────────────────────────────────────────

    pub fn add_source(
        &self,
        connection: &SourceConnection,
        name: &str,
        watch: bool,
    ) -> Result<SourceId, LibError> {
        let id = SourceId::new();
        let now = now_ms();
        // The connection blob carries the secret (§3.2). It is persisted here and only ever handed
        // back to the engine via `get_source_connection`; `SourceInfo` exposes the sanitised URI.
        let conn_json = serde_json::to_string(connection).map_err(internal)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, connection, online, watch, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?6)",
            params![
                id.as_bytes().to_vec(),
                name,
                connection.kind(),
                conn_json,
                watch as i64,
                now,
            ],
        )
        .map_err(internal)?;
        Ok(id)
    }

    /// The full connection blob (**including the secret**) for rebuilding a backend at scan/read
    /// time. Never leaves the engine — clients only ever see the sanitised `SourceInfo.uri`.
    pub fn get_source_connection(&self, id: &SourceId) -> Result<SourceConnection, LibError> {
        let conn = self.conn.lock().unwrap();
        let blob: String = conn
            .query_row(
                "SELECT connection FROM source WHERE id = ?1",
                params![id.as_bytes().to_vec()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?
            .ok_or_else(|| LibError::NotFound(format!("source {id}")))?;
        parse_connection(&blob)
    }

    pub fn list_sources(&self) -> Result<Vec<SourceInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, kind, connection, online, last_scanned_at, last_error, watch
                 FROM source ORDER BY created_at",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| Self::row_to_source(r, &conn))
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// Immediate subfolders directly under `prefix` within one source, each with its whole-subtree
    /// asset count (issue #66). `prefix` is source-relative, empty or ending in `/`. Derived on the
    /// fly from the stored paths — the immediate child folder of a descendant is the first path
    /// segment after the prefix, kept only when the remainder still holds a `/` (else it's a file
    /// sitting directly in this folder, not a subfolder). `length()`/`substr()` are character-based
    /// in SQLite, so a multibyte prefix offsets correctly.
    pub fn list_folders(
        &self,
        source: &SourceId,
        prefix: &str,
    ) -> Result<Vec<FolderEntry>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT folder, COUNT(*) FROM (
                    SELECT CASE WHEN instr(rest, '/') > 0
                                THEN substr(rest, 1, instr(rest, '/') - 1)
                                ELSE NULL END AS folder
                    FROM (SELECT substr(path, length(?2) + 1) AS rest
                          FROM asset
                          WHERE source_id = ?1 AND path LIKE ?3 ESCAPE '\\')
                 )
                 WHERE folder IS NOT NULL AND folder <> ''
                 GROUP BY folder ORDER BY folder COLLATE NOCASE",
            )
            .map_err(internal)?;
        let like = format!("{}%", escape_like(prefix));
        let rows = stmt
            .query_map(params![source.as_bytes().to_vec(), prefix, like], |r| {
                Ok(FolderEntry {
                    name: r.get(0)?,
                    asset_count: r.get::<_, i64>(1)? as u64,
                })
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    pub fn get_source(&self, id: &SourceId) -> Result<Option<SourceInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, kind, connection, online, last_scanned_at, last_error, watch
             FROM source WHERE id = ?1",
            params![id.as_bytes().to_vec()],
            |r| Self::row_to_source(r, &conn),
        )
        .optional()
        .map_err(internal)
    }

    fn row_to_source(r: &rusqlite::Row, conn: &Connection) -> rusqlite::Result<SourceInfo> {
        let id_blob: Vec<u8> = r.get(0)?;
        let id = blob_to_source_id(&id_blob);
        let name: String = r.get(1)?;
        let kind_s: String = r.get(2)?;
        let connection: String = r.get(3)?;
        let online: i64 = r.get(4)?;
        let last_scanned_at: Option<i64> = r.get(5)?;
        let last_error: Option<String> = r.get(6)?;
        let watch: i64 = r.get(7)?;
        // Prefer the typed connection's secret-free display URI; fall back to the legacy `{"uri":…}`
        // shape (pre-phase-4 local sources) so old dev libraries still list cleanly.
        let uri = parse_connection(&connection)
            .map(|c| c.display_uri())
            .ok()
            .unwrap_or_else(|| {
                serde_json::from_str::<serde_json::Value>(&connection)
                    .ok()
                    .and_then(|v| v.get("uri").and_then(|u| u.as_str().map(String::from)))
                    .unwrap_or_default()
            });
        let kind = SourceKind::parse(&kind_s).unwrap_or(SourceKind::LocalFs);
        let asset_count: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset WHERE source_id = ?1",
                params![id_blob],
                |c| c.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64;
        let state = if let Some(err) = last_error.clone() {
            SourceState::Error(err)
        } else if online != 0 {
            SourceState::Online
        } else {
            SourceState::Offline
        };
        Ok(SourceInfo {
            id,
            kind,
            name,
            uri,
            state,
            stats: SourceStats {
                asset_count,
                last_scanned_at,
                last_error,
            },
            watch: watch != 0,
            // Left false here: answering it means touching the filesystem or a remote host, which
            // this row-mapper runs under the connection lock and must not do. The engine fills it
            // in after the query (`EmbeddedLibrary::mark_writable`), where the probe is off-lock.
            writable: false,
        })
    }

    pub fn remove_source(&self, id: &SourceId, keep_metadata: bool) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        if keep_metadata {
            // Mark offline but keep cached rows (fail-soft, PRODUCT_SPEC §6.1).
            conn.execute(
                "UPDATE source SET online = 0, updated_at = ?2 WHERE id = ?1",
                params![id.as_bytes().to_vec(), now_ms()],
            )
            .map_err(internal)?;
        } else {
            // ON DELETE CASCADE clears its assets.
            let n = conn
                .execute(
                    "DELETE FROM source WHERE id = ?1",
                    params![id.as_bytes().to_vec()],
                )
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::NotFound(format!("source {id}")));
            }
        }
        Ok(())
    }

    pub fn set_source_scanned(&self, id: &SourceId, at: i64) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE source SET last_scanned_at = ?2, last_error = NULL, online = 1, updated_at = ?2 WHERE id = ?1",
            params![id.as_bytes().to_vec(), at],
        )
        .map_err(internal)?;
        Ok(())
    }

    pub fn set_source_error(&self, id: &SourceId, err: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE source SET last_error = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.as_bytes().to_vec(), err, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// `path -> (size_bytes, source_modified_at)` for every asset of a source. The delta re-scan
    /// compares each walked entry's cheap change token (size+mtime) against this to decide whether
    /// to re-open bytes at all (tech-spec 07 §2.2).
    pub fn source_path_index(&self, source_id: &SourceId) -> Result<PathIndex, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT path, size_bytes, source_modified_at FROM asset WHERE source_id = ?1")
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![source_id.as_bytes().to_vec()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?),
                ))
            })
            .map_err(internal)?;
        let mut map = std::collections::HashMap::new();
        for r in rows {
            let (p, tok) = r.map_err(internal)?;
            map.insert(p, tok);
        }
        Ok(map)
    }

    /// Mark a source's rows at these paths **missing** (asset `flags` bit 0) without deleting them —
    /// their catalog rows persist as absent until the user prunes (non-destructive, §2.2). Returns
    /// the number of rows touched.
    pub fn mark_paths_missing(
        &self,
        source_id: &SourceId,
        paths: &[String],
    ) -> Result<u64, LibError> {
        if paths.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(internal)?;
        let now = now_ms();
        let mut n = 0u64;
        for p in paths {
            n += tx
                .execute(
                    "UPDATE asset SET flags = flags | 1, updated_at = ?3
                     WHERE source_id = ?1 AND path = ?2",
                    params![source_id.as_bytes().to_vec(), p, now],
                )
                .map_err(internal)? as u64;
        }
        tx.commit().map_err(internal)?;
        Ok(n)
    }
}
