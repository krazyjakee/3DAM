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
        self.add_source_with_auth(id, connection, name, watch, None)?;
        Ok(id)
    }

    /// Insert a source whose credential, if any, has already been committed to the host secret
    /// store. Inline secrets are rejected even though `SourceConnection` serde also omits them:
    /// silently dropping a password would leave a source registered but unusable.
    pub fn add_source_with_auth(
        &self,
        id: SourceId,
        connection: &SourceConnection,
        name: &str,
        watch: bool,
        auth_ref: Option<&str>,
    ) -> Result<(), LibError> {
        if connection.has_inline_credentials() {
            return Err(LibError::Internal(
                "source credentials must be secured before catalog persistence".into(),
            ));
        }
        if auth_ref.is_some_and(|reference| reference != format!("3dam.source.{id}")) {
            return Err(LibError::Internal(
                "source credential reference is not a canonical opaque source key".into(),
            ));
        }
        let now = now_ms();
        // `SourceConnection` structurally omits runtime credential fields; `auth_ref` is the only
        // persisted auth value, and is an opaque identifier rather than credential material.
        let conn_json = serde_json::to_string(connection).map_err(internal)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, connection, auth_ref, online, watch, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?7)",
            params![
                id.as_bytes().to_vec(),
                name,
                connection.kind(),
                conn_json,
                auth_ref,
                watch as i64,
                now,
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// The secret-free connection plus its opaque credential reference. The engine resolves that
    /// reference immediately before opening a backend; clients only see `SourceInfo.uri`.
    pub fn get_source_connection(&self, id: &SourceId) -> Result<SourceConnection, LibError> {
        let conn = self.conn.lock().unwrap();
        let (blob, auth_ref): (String, Option<String>) = conn
            .query_row(
                "SELECT connection, auth_ref FROM source WHERE id = ?1",
                params![id.as_bytes().to_vec()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?
            .ok_or_else(|| LibError::NotFound(format!("source {id}")))?;
        let mut connection = parse_connection(&blob)?;
        connection.set_credential_ref(auth_ref);
        Ok(connection)
    }

    /// Every source row's stored connection and opaque auth reference. Used only by the startup
    /// migration that extracts credentials from pre-issue-103 connection JSON.
    pub fn source_connections(
        &self,
    ) -> Result<Vec<(SourceId, SourceConnection, Option<String>)>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, connection, auth_ref FROM source ORDER BY created_at")
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                let id = blob_to_source_id(&r.get::<_, Vec<u8>>(0)?);
                Ok((id, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
            })
            .map_err(internal)?;
        let rows = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;
        rows.into_iter()
            .map(|(id, blob, auth_ref)| Ok((id, parse_connection(&blob)?, auth_ref)))
            .collect()
    }

    pub fn source_credentials_migrated(&self) -> Result<bool, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM host_migration WHERE key = 'source_credentials_v1')",
            [],
            |r| r.get::<_, bool>(0),
        )
        .map_err(internal)
    }

    pub fn mark_source_credentials_migrated(&self) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO host_migration (key, completed_at) VALUES ('source_credentials_v1', ?1)
             ON CONFLICT(key) DO UPDATE SET completed_at = excluded.completed_at",
            params![now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Atomically replace legacy inline connection blobs with their redacted forms and install
    /// the corresponding opaque references. The caller writes all secret-store entries first, so
    /// a failure here leaves the old rows (and therefore source availability) intact for retry.
    pub fn rewrite_source_credentials(
        &self,
        updates: &[(SourceId, SourceConnection, String)],
    ) -> Result<(), LibError> {
        let encoded = updates
            .iter()
            .map(|(id, connection, auth_ref)| {
                if connection.has_inline_credentials() {
                    return Err(LibError::Internal(
                        "credential migration attempted to persist inline material".into(),
                    ));
                }
                if auth_ref != &format!("3dam.source.{id}") {
                    return Err(LibError::Internal(
                        "credential migration produced a non-canonical source reference".into(),
                    ));
                }
                Ok((
                    *id,
                    serde_json::to_string(connection).map_err(internal)?,
                    auth_ref.clone(),
                ))
            })
            .collect::<Result<Vec<_>, LibError>>()?;
        let mut conn = self.conn.lock().unwrap();
        // Deleted/updated SQLite payload can otherwise survive in free pages. `secure_delete`
        // zeros cells changed below; the checkpoint + VACUUM in `scrub_source_storage_locked`
        // removes older copies from the WAL and rebuilds the main file without free-page remnants.
        conn.pragma_update(None, "secure_delete", "ON")
            .map_err(internal)?;
        let tx = conn.transaction().map_err(internal)?;
        for (id, blob, auth_ref) in encoded {
            tx.execute(
                "UPDATE source SET connection = ?2, auth_ref = ?3, updated_at = ?4 WHERE id = ?1",
                params![id.as_bytes().to_vec(), blob, auth_ref, now_ms()],
            )
            .map_err(internal)?;
        }
        tx.commit().map_err(internal)?;
        Self::scrub_source_storage_locked(&conn)
    }

    /// Complete or retry the physical scrub after a prior process committed redacted rows but
    /// failed before checkpoint/VACUUM. Idempotent and invoked only while the host migration marker
    /// is absent, never on ordinary opens.
    pub fn scrub_source_storage(&self) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        Self::scrub_source_storage_locked(&conn)
    }

    fn scrub_source_storage_locked(conn: &Connection) -> Result<(), LibError> {
        let checkpoint = |connection: &Connection| -> Result<(), LibError> {
            let busy: i64 = connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                .map_err(internal)?;
            if busy != 0 {
                return Err(LibError::SourceUnavailable(
                    "credential migration cleanup is blocked by another library process; close it and retry"
                        .into(),
                ));
            }
            Ok(())
        };
        checkpoint(conn)?;
        conn.execute("VACUUM", []).map_err(internal)?;
        checkpoint(conn)
    }

    pub fn pending_source_credential_cleanup(&self) -> Result<Vec<String>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT auth_ref FROM host_secret_cleanup ORDER BY auth_ref")
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    pub fn complete_source_credential_cleanup(&self, auth_ref: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM host_secret_cleanup WHERE auth_ref = ?1",
            params![auth_ref],
        )
        .map_err(internal)?;
        Ok(())
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

    /// Immediate subfolders directly under `prefix` within one source, each with its materialized
    /// whole-subtree asset count (issues #66/#136). `prefix` is source-relative, empty or ending in
    /// `/`. The `(source_id, parent_path)` index makes this one point lookup plus the returned child
    /// rows; no descendant asset paths are read or split during expansion.
    pub fn list_folders(
        &self,
        source: &SourceId,
        prefix: &str,
    ) -> Result<Vec<FolderEntry>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name, descendant_asset_count FROM folder
                  WHERE source_id = ?1 AND parent_path = ?2 AND path <> ''
                  ORDER BY name COLLATE NOCASE",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![source.as_bytes().to_vec(), prefix], |r| {
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
            writable_reason: None,
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
            // Queue the opaque ref in the same transaction that removes the source. Host-secret
            // deletion is acknowledged separately, so a locked provider cannot create an orphan.
            let mut conn = conn;
            let tx = conn.transaction().map_err(internal)?;
            tx.execute(
                "INSERT OR IGNORE INTO host_secret_cleanup(auth_ref)
                 SELECT auth_ref FROM source WHERE id = ?1 AND auth_ref IS NOT NULL",
                params![id.as_bytes().to_vec()],
            )
            .map_err(internal)?;
            // ON DELETE CASCADE clears its assets.
            let n = tx
                .execute(
                    "DELETE FROM source WHERE id = ?1",
                    params![id.as_bytes().to_vec()],
                )
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::NotFound(format!("source {id}")));
            }
            tx.commit().map_err(internal)?;
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

    /// Start one streamed enumeration and return its source-local generation. The increment is a
    /// transaction so two concurrent scans cannot receive the same generation.
    pub fn begin_source_scan(&self, source_id: &SourceId) -> Result<i64, LibError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let changed = tx
            .execute(
                "UPDATE source SET scan_generation = scan_generation + 1 WHERE id = ?1",
                params![source_id.as_bytes().to_vec()],
            )
            .map_err(internal)?;
        if changed == 0 {
            return Err(LibError::NotFound(format!("source {source_id}")));
        }
        let generation = tx
            .query_row(
                "SELECT scan_generation FROM source WHERE id = ?1",
                params![source_id.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .map_err(internal)?;
        tx.commit().map_err(internal)?;
        Ok(generation)
    }

    /// Stamp one enumerated path and return its prior delta change token. The unique
    /// `(source_id,path)` index makes this constant-space point work. Stamps are monotonic and are
    /// accepted only for the source's current generation, so a superseded older walk cannot erase
    /// a newer walk's observation. Seeing a formerly-missing path also restores it immediately.
    pub fn observe_source_path(
        &self,
        source_id: &SourceId,
        path: &str,
        generation: i64,
    ) -> Result<Option<SourceChangeToken>, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "UPDATE asset
                SET seen_generation = max(seen_generation, ?3), flags = flags & -2
              WHERE source_id = ?1 AND path = ?2
                AND ?3 = (SELECT scan_generation FROM source WHERE id = ?1)
              RETURNING size_bytes, source_modified_at",
            params![source_id.as_bytes().to_vec(), path, generation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(internal)
    }

    /// Finish an exhaustively enumerated generation. The missing transition and source-success
    /// marker commit together. `None` means a newer scan began first; the older scan is deliberately
    /// barred from marking anything missing. Callers must never invoke this after cancellation or
    /// any whole-walk/per-entry listing failure.
    pub fn finish_source_scan(
        &self,
        source_id: &SourceId,
        generation: i64,
        scanned_at: i64,
    ) -> Result<Option<u64>, LibError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(internal)?;
        let current = tx
            .query_row(
                "SELECT scan_generation FROM source WHERE id = ?1",
                params![source_id.as_bytes().to_vec()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(internal)?
            .ok_or_else(|| LibError::NotFound(format!("source {source_id}")))?;
        if current != generation {
            tx.commit().map_err(internal)?;
            return Ok(None);
        }
        let missing = tx
            .execute(
                "UPDATE asset SET flags = flags | 1, updated_at = ?3
                  WHERE source_id = ?1 AND seen_generation <> ?2 AND (flags & 1) = 0",
                params![source_id.as_bytes().to_vec(), generation, scanned_at],
            )
            .map_err(internal)? as u64;
        tx.execute(
            "UPDATE source
                SET last_scanned_at = ?2, last_error = NULL, online = 1, updated_at = ?2
              WHERE id = ?1",
            params![source_id.as_bytes().to_vec(), scanned_at],
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)?;
        Ok(Some(missing))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(source_id: SourceId, path: &str) -> NewAsset {
        NewAsset {
            source_id,
            path: path.into(),
            filename: path.rsplit('/').next().unwrap().into(),
            content_hash: None,
            size_bytes: Some(10),
            source_modified_at: Some(20),
            scanned_at: now_ms(),
            media_type: MediaType::Image,
            format: "png".into(),
        }
    }

    fn fixture() -> (Store, SourceId) {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/scan-generation".into(),
                },
                "scan-generation",
                false,
            )
            .unwrap();
        for path in ["a.png", "b.png", "c.png"] {
            store.upsert_asset(&scanned(source, path)).unwrap();
        }
        (store, source)
    }

    fn missing(store: &Store, source: SourceId, path: &str) -> bool {
        store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT (flags & 1) <> 0 FROM asset WHERE source_id = ?1 AND path = ?2",
                params![source.as_bytes().to_vec(), path],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn scan_generation_marks_only_unseen_rows_after_successful_finish() {
        let (store, source) = fixture();
        let generation = store.begin_source_scan(&source).unwrap();
        assert_eq!(
            store
                .observe_source_path(&source, "a.png", generation)
                .unwrap(),
            Some((Some(10), Some(20)))
        );
        assert_eq!(
            store
                .finish_source_scan(&source, generation, now_ms())
                .unwrap(),
            Some(2)
        );
        assert!(!missing(&store, source, "a.png"));
        assert!(missing(&store, source, "b.png"));
        assert!(missing(&store, source, "c.png"));
    }

    #[test]
    fn unfinished_and_superseded_generations_cannot_mark_unseen_rows_missing() {
        let (store, source) = fixture();

        // Cancellation/failure abandons the generation without calling finish. Observations may
        // restore paths actually seen, but unseen rows remain untouched.
        let abandoned = store.begin_source_scan(&source).unwrap();
        store
            .observe_source_path(&source, "a.png", abandoned)
            .unwrap();
        assert!(!missing(&store, source, "b.png"));

        let older = store.begin_source_scan(&source).unwrap();
        store.observe_source_path(&source, "a.png", older).unwrap();
        let newer = store.begin_source_scan(&source).unwrap();
        store.observe_source_path(&source, "b.png", newer).unwrap();
        // This late old-generation stamp must not overwrite the newer stamp on the same row.
        assert_eq!(
            store.observe_source_path(&source, "b.png", older).unwrap(),
            None
        );
        let b_seen: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT seen_generation FROM asset WHERE source_id = ?1 AND path = 'b.png'",
                params![source.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(b_seen, newer);
        assert_eq!(
            store.finish_source_scan(&source, older, now_ms()).unwrap(),
            None
        );
        let last_scanned: Option<i64> = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT last_scanned_at FROM source WHERE id = ?1",
                params![source.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            last_scanned, None,
            "a superseded finish changed the source success timestamp"
        );
        assert!(
            ["a.png", "b.png", "c.png"]
                .into_iter()
                .all(|path| !missing(&store, source, path)),
            "a superseded finish changed missing flags"
        );
        assert_eq!(
            store.finish_source_scan(&source, newer, now_ms()).unwrap(),
            Some(2)
        );
        assert!(missing(&store, source, "a.png"));
        assert!(!missing(&store, source, "b.png"));
        assert!(missing(&store, source, "c.png"));
    }

    #[test]
    fn observing_an_unchanged_missing_row_restores_it_immediately() {
        let (store, source) = fixture();
        store
            .mark_paths_missing(&source, &["b.png".to_string()])
            .unwrap();
        assert!(missing(&store, source, "b.png"));

        let generation = store.begin_source_scan(&source).unwrap();
        let token = store
            .observe_source_path(&source, "b.png", generation)
            .unwrap();
        assert_eq!(token, Some((Some(10), Some(20))));
        assert!(
            !missing(&store, source, "b.png"),
            "a listed delta-unchanged asset stayed missing"
        );
        // Simulate cancellation: no finish call, and unrelated unseen rows remain present.
        assert!(!missing(&store, source, "a.png"));
        assert!(!missing(&store, source, "c.png"));
    }

    #[test]
    fn asset_insert_during_a_scan_is_stamped_before_finish() {
        let (store, source) = fixture();
        let generation = store.begin_source_scan(&source).unwrap();
        store.upsert_asset(&scanned(source, "new.png")).unwrap();
        let seen: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT seen_generation FROM asset WHERE source_id = ?1 AND path = 'new.png'",
                params![source.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(seen, generation);
        assert_eq!(
            store
                .finish_source_scan(&source, generation, now_ms())
                .unwrap(),
            Some(3)
        );
        assert!(!missing(&store, source, "new.png"));
    }

    fn streamed_reconciliation_fixture(rows: usize) -> std::time::Duration {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/streamed-reconciliation".into(),
                },
                "streamed-reconciliation",
                false,
            )
            .unwrap();
        {
            let mut connection = store.conn.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO asset(
                        id, source_id, path, filename, size_bytes, source_modified_at, scanned_at,
                        media_type, format, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?3, 10, 20, 0, 'image', 'png', 0, 0)",
                )
                .unwrap();
            for index in 0..rows {
                let path = format!("asset_{index:07}.png");
                insert
                    .execute(params![
                        AssetId::new().as_bytes().to_vec(),
                        source.as_bytes().to_vec(),
                        path
                    ])
                    .unwrap();
            }
            drop(insert);
            transaction.commit().unwrap();
        }

        let started = std::time::Instant::now();
        let generation = store.begin_source_scan(&source).unwrap();
        // Produce and discard one path at a time: the API cannot retain a catalog-sized path set.
        for index in (0..rows).step_by(2) {
            let path = format!("asset_{index:07}.png");
            assert!(store
                .observe_source_path(&source, &path, generation)
                .unwrap()
                .is_some());
        }
        assert_eq!(
            store
                .finish_source_scan(&source, generation, now_ms())
                .unwrap(),
            Some((rows / 2) as u64)
        );
        started.elapsed()
    }

    #[test]
    fn scaled_streamed_reconciliation_has_constant_path_memory() {
        let elapsed = streamed_reconciliation_fixture(20_000);
        eprintln!("20k streamed reconciliation: {elapsed:?}");
        assert!(elapsed < std::time::Duration::from_secs(10));
    }

    /// Explicit product-scale wall-time/RSS fixture. Observe peak RSS externally while running:
    /// `cargo test -p dam-store million_asset_streamed_reconciliation -- --ignored --nocapture`.
    #[test]
    #[ignore = "builds the explicit 1M-asset reconciliation fixture"]
    fn million_asset_streamed_reconciliation() {
        let elapsed = streamed_reconciliation_fixture(1_000_000);
        eprintln!("1M streamed reconciliation: {elapsed:?}");
    }
}
