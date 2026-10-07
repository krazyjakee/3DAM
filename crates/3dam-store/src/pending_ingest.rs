//! Durable source discovery, isolated from the hash-verified asset catalog.
//! New paths are admitted only after enrichment's blocklist/revision transaction.

use super::*;
use crate::helpers::{attr_table, blob_to_asset_id};

#[derive(Clone, Debug)]
pub struct PendingDiscovery {
    pub path: String,
    pub size: u64,
    pub modified_ms: Option<i64>,
    /// None still observes an existing rejected path without admitting a new asset.
    pub media: Option<MediaType>,
    pub format: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PendingIngest {
    pub source_id: SourceId,
    pub path: String,
    pub size: u64,
    pub modified_ms: Option<i64>,
    pub media: MediaType,
    pub format: String,
    pub revision: i64,
    pub generation: i64,
}

pub struct QuickDiscoveryOutcome {
    pub examined: u64,
    pub queued: u64,
    pub invalidated: Vec<AssetId>,
    pub generation_current: bool,
}

#[derive(Debug)]
pub enum PendingCommitOutcome {
    Written { id: AssetId, inserted: bool },
    Blocked { removed_asset: Option<AssetId> },
    Stale,
    Cancelled,
}

impl Store {
    /// One bounded discovery transaction, containing no file-content access.
    pub fn apply_quick_discovery(
        &self,
        source: &SourceId,
        generation: i64,
        entries: &[PendingDiscovery],
    ) -> Result<QuickDiscoveryOutcome, LibError> {
        if entries.len() > 128 {
            return Err(LibError::BadRequest(
                "discovery batch exceeds 128 paths".into(),
            ));
        }
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let current: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM source WHERE id=?1 AND scan_generation=?2)
                   AND EXISTS(SELECT 1 FROM scan_spool.active WHERE source_id=?1 AND generation=?2)",
                params![source.as_bytes().to_vec(), generation],
                |row| row.get(0),
            )
            .map_err(internal)?;
        let mut out = QuickDiscoveryOutcome {
            examined: 0,
            queued: 0,
            invalidated: Vec::new(),
            generation_current: current,
        };
        if !current {
            return Ok(out);
        }
        for entry in entries {
            let token = Self::observe_source_path_in(&tx, source, &entry.path, generation)?;
            tx.execute("INSERT OR IGNORE INTO scan_spool.pending_observed(source_id,generation,path)
                SELECT ?1,?3,?2 WHERE EXISTS(SELECT 1 FROM pending_ingest WHERE source_id=?1 AND path=?2)",
                params![source.as_bytes().to_vec(), entry.path, generation]).map_err(internal)?;
            let (Some(media), Some(format)) = (entry.media, entry.format.as_deref()) else {
                continue;
            };
            out.examined += 1;
            let size = i64::try_from(entry.size)
                .map_err(|_| LibError::BadRequest("file size exceeds catalog range".into()))?;
            let pending: Option<(i64, Option<i64>)> = tx.query_row("SELECT size_bytes,source_modified_at FROM pending_ingest WHERE source_id=?1 AND path=?2",
                params![source.as_bytes().to_vec(), entry.path], |row| Ok((row.get(0)?,row.get(1)?))).optional().map_err(internal)?;
            let verified: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM asset WHERE source_id=?1 AND path=?2 AND content_hash IS NOT NULL)",
                params![source.as_bytes().to_vec(),entry.path],|row|row.get(0)).map_err(internal)?;
            let unchanged = entry.modified_ms.is_some()
                && (pending == Some((size, entry.modified_ms))
                    || (pending.is_none()
                        && verified
                        && token == Some((Some(size), entry.modified_ms))));
            if unchanged {
                continue;
            }
            // Source-local monotonic revisions survive queue deletion/recreation,
            // so an old worker can never match a later row through an ABA cycle.
            let revision:i64=tx.query_row("UPDATE source SET ingest_revision=ingest_revision+1 WHERE id=?1 RETURNING ingest_revision",
                params![source.as_bytes().to_vec()],|row|row.get(0)).map_err(internal)?;
            tx.execute("INSERT INTO pending_ingest(source_id,path,size_bytes,source_modified_at,media_type,format,revision,seen_generation,queued_at)
                VALUES(?1,?2,?3,?4,?5,?6,?9,?7,?8)
                ON CONFLICT(source_id,path) DO UPDATE SET size_bytes=excluded.size_bytes,source_modified_at=excluded.source_modified_at,
                media_type=excluded.media_type,format=excluded.format,revision=excluded.revision,
                seen_generation=excluded.seen_generation,queued_at=excluded.queued_at,retry_after=0,attempts=0,last_error=NULL",
                params![source.as_bytes().to_vec(),entry.path,size,entry.modified_ms,media.as_str(),format,generation,now_ms(),revision]).map_err(internal)?;
            out.queued += 1;
            let id: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT id FROM asset WHERE source_id=?1 AND path=?2",
                    params![source.as_bytes().to_vec(), entry.path],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            if let Some(id) = id {
                // The path names a new revision. Never retain a hash, vector, body,
                // automatic classification, or derivative gate from its former bytes.
                tx.execute("UPDATE asset SET content_hash=NULL,size_bytes=?2,source_modified_at=?3,
                    analysis_version=0,analysed_at=NULL,derivative_version=0,thumbnail_key=NULL,preview_key=NULL,updated_at=?4 WHERE id=?1",
                    params![id,size,entry.modified_ms,now_ms()]).map_err(internal)?;
                tx.execute("DELETE FROM embedding WHERE asset_id=?1", params![id])
                    .map_err(internal)?;
                let tags_changed=tx.execute(
                    "DELETE FROM asset_tag WHERE asset_id=?1 AND source<>'user' AND state<>'rejected'",
                    params![id],
                ).map_err(internal)?;
                if tags_changed > 0 {
                    Self::reindex_asset_tags(&tx, &blob_to_asset_id(&id));
                }
                tx.execute("UPDATE asset_fts SET text='' WHERE rowid=(SELECT rowid FROM asset WHERE id=?1) AND text IS NOT ''", params![id]).map_err(internal)?;
                for family in MediaType::ALL {
                    tx.execute(
                        &format!("DELETE FROM {} WHERE asset_id=?1", attr_table(*family)),
                        params![id],
                    )
                    .map_err(internal)?;
                }
                out.invalidated.push(blob_to_asset_id(&id));
            }
        }
        tx.commit().map_err(internal)?;
        self.wake_ann(!out.invalidated.is_empty());
        Ok(out)
    }

    pub fn pending_ingest_for_path(
        &self,
        source: &SourceId,
        path: &str,
    ) -> Result<Option<PendingIngest>, LibError> {
        let conn = self.read()?;
        conn.query_row("SELECT path,size_bytes,source_modified_at,media_type,format,revision,seen_generation FROM pending_ingest WHERE source_id=?1 AND path=?2",
            params![source.as_bytes().to_vec(),path], |row| pending_row(row,*source)).optional().map_err(internal)
    }

    pub fn pending_ingest_count(&self, source: &SourceId) -> Result<u64, LibError> {
        let conn = self.read()?;
        conn.query_row(
            "SELECT count(*) FROM pending_ingest WHERE source_id=?1",
            params![source.as_bytes().to_vec()],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count.max(0) as u64)
        .map_err(internal)
    }

    /// A bounded page of runnable work. No in-progress flag can strand a row after
    /// a crash; a retry remains durable until a verified commit removes it.
    pub fn list_pending_ingest(
        &self,
        source: &SourceId,
        limit: usize,
    ) -> Result<Vec<PendingIngest>, LibError> {
        self.list_pending_ingest_scoped(source, limit, &[])
    }

    pub fn list_pending_ingest_scoped(
        &self,
        source: &SourceId,
        limit: usize,
        scopes: &[String],
    ) -> Result<Vec<PendingIngest>, LibError> {
        if scopes.len() > 256 {
            return Err(LibError::BadRequest("too many verification scopes".into()));
        }
        let conn = self.read()?;
        let mut sql=String::from("SELECT path,size_bytes,source_modified_at,media_type,format,revision,seen_generation FROM pending_ingest WHERE source_id=? AND retry_after<=?");
        let mut arguments = vec![
            Value::Blob(source.as_bytes().to_vec()),
            Value::Integer(now_ms()),
        ];
        if !scopes.is_empty()
            && !scopes
                .iter()
                .any(|scope| scope.trim_matches('/').is_empty())
        {
            sql.push_str(" AND (");
            for (index, scope) in scopes.iter().enumerate() {
                if index > 0 {
                    sql.push_str(" OR ");
                }
                sql.push_str("path=? OR substr(path,1,length(?))=?");
                let root = scope.trim_matches('/').to_string();
                let prefix = format!("{root}/");
                arguments.push(Value::Text(root));
                arguments.push(Value::Text(prefix.clone()));
                arguments.push(Value::Text(prefix));
            }
            sql.push(')');
        }
        sql.push_str(" ORDER BY path LIMIT ?");
        arguments.push(Value::Integer(limit.clamp(1, 128) as i64));
        let mut statement = conn.prepare(&sql).map_err(internal)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(arguments), |row| {
                pending_row(row, *source)
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// Call only after an authoritative walk and successful catalog reconciliation.
    /// Scope matching is by path segments, never a directory timestamp heuristic.
    pub fn finish_pending_discovery(
        &self,
        source: &SourceId,
        generation: i64,
        scopes: &[String],
    ) -> Result<bool, LibError> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let current: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM source WHERE id=?1 AND scan_generation=?2)
                   AND EXISTS(SELECT 1 FROM scan_spool.active WHERE source_id=?1 AND generation=?2)",
                params![source.as_bytes().to_vec(), generation],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if !current {
            return Ok(false);
        }
        if scopes.is_empty() {
            tx.execute(
                "DELETE FROM pending_ingest WHERE source_id=?1 AND seen_generation<>?2
                        AND NOT EXISTS(SELECT 1 FROM scan_spool.pending_observed o WHERE o.source_id=pending_ingest.source_id AND o.generation=?2 AND o.path=pending_ingest.path)",
                params![source.as_bytes().to_vec(), generation],
            )
            .map_err(internal)?;
        } else {
            for scope in scopes {
                let root = scope.trim_matches('/');
                if root.is_empty() {
                    tx.execute(
                        "DELETE FROM pending_ingest WHERE source_id=?1 AND seen_generation<>?2
                        AND NOT EXISTS(SELECT 1 FROM scan_spool.pending_observed o WHERE o.source_id=pending_ingest.source_id AND o.generation=?2 AND o.path=pending_ingest.path)",
                        params![source.as_bytes().to_vec(), generation],
                    )
                    .map_err(internal)?;
                } else {
                    let prefix = format!("{root}/");
                    tx.execute("DELETE FROM pending_ingest WHERE source_id=?1 AND seen_generation<>?2
                        AND NOT EXISTS(SELECT 1 FROM scan_spool.pending_observed o WHERE o.source_id=pending_ingest.source_id AND o.generation=?2 AND o.path=pending_ingest.path) AND (path=?3 OR substr(path,1,length(?4))=?4)",
                        params![source.as_bytes().to_vec(),generation,root,prefix]).map_err(internal)?;
                }
            }
        }
        tx.commit().map_err(internal)?;
        Ok(true)
    }

    pub fn retry_pending_ingest(
        &self,
        target: &PendingIngest,
        error: &str,
    ) -> Result<(), LibError> {
        let conn = self.write();
        conn.execute("UPDATE pending_ingest SET attempts=attempts+1,last_error=?4,retry_after=?5 WHERE source_id=?1 AND path=?2 AND revision=?3",
            params![target.source_id.as_bytes().to_vec(),target.path,target.revision,error,now_ms()+30_000]).map_err(internal)?;
        Ok(())
    }

    /// A positive source stat discovered newer bytes after discovery. Supersede
    /// only this pending revision; a newer queued revision always wins.
    pub fn refresh_pending_ingest_token(
        &self,
        target: &PendingIngest,
        size: u64,
        modified_ms: Option<i64>,
    ) -> Result<bool, LibError> {
        let size = i64::try_from(size)
            .map_err(|_| LibError::BadRequest("source size exceeds catalog range".into()))?;
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM pending_ingest WHERE source_id=?1 AND path=?2 AND revision=?3)",
            params![target.source_id.as_bytes().to_vec(),target.path,target.revision],|row|row.get(0)).map_err(internal)?;
        if !exists {
            return Ok(false);
        }
        let revision:i64=tx.query_row("UPDATE source SET ingest_revision=ingest_revision+1 WHERE id=?1 RETURNING ingest_revision",
            params![target.source_id.as_bytes().to_vec()],|row|row.get(0)).map_err(internal)?;
        tx.execute("UPDATE pending_ingest SET size_bytes=?4,source_modified_at=?5,revision=?7,
            seen_generation=(SELECT scan_generation FROM source WHERE id=?1),queued_at=?6,retry_after=?8,attempts=0,last_error=NULL
            WHERE source_id=?1 AND path=?2 AND revision=?3",params![target.source_id.as_bytes().to_vec(),target.path,target.revision,size,modified_ms,now_ms(),revision,now_ms()+1000]).map_err(internal)?;
        tx.commit().map_err(internal)?;
        Ok(true)
    }

    /// Revision, cancellation and blocklist checks share the catalog upsert's
    /// transaction. The caller already verified the freshly fetched source token.
    pub fn commit_pending_ingest(
        &self,
        job: &JobId,
        target: &PendingIngest,
        asset: &NewAsset,
        attrs: &MediaAttributes,
    ) -> Result<PendingCommitOutcome, LibError> {
        if asset.source_id != target.source_id
            || asset.path != target.path
            || asset.content_hash.is_none()
            || asset.size_bytes != Some(target.size as i64)
            || asset.source_modified_at != target.modified_ms
        {
            return Err(LibError::BadRequest(
                "pending ingest commit does not match its verified revision".into(),
            ));
        }
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let cancelled: bool = tx
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM job WHERE id=?1 AND state<>'cancelled')",
                params![job.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if cancelled {
            return Ok(PendingCommitOutcome::Cancelled);
        }
        let current: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pending_ingest p JOIN source s ON s.id=p.source_id
            WHERE p.source_id=?1 AND p.path=?2 AND p.revision=?3 AND p.size_bytes=?4 AND p.source_modified_at IS ?5
            )",params![target.source_id.as_bytes().to_vec(),target.path,target.revision,target.size as i64,target.modified_ms],|row|row.get(0)).map_err(internal)?;
        if !current {
            return Ok(PendingCommitOutcome::Stale);
        }
        let hash = asset.content_hash.as_ref().expect("validated hash");
        let outcome = if Self::is_blocked_in(&tx, hash)? {
            let removed: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT id FROM asset WHERE source_id=?1 AND path=?2",
                    params![target.source_id.as_bytes().to_vec(), target.path],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            tx.execute(
                "DELETE FROM asset WHERE source_id=?1 AND path=?2",
                params![target.source_id.as_bytes().to_vec(), target.path],
            )
            .map_err(internal)?;
            PendingCommitOutcome::Blocked {
                removed_asset: removed.map(|id| blob_to_asset_id(&id)),
            }
        } else {
            let upsert = Self::upsert_asset_in(&tx, asset)?;
            Self::set_media_attrs_in(&tx, &upsert.id, attrs)?;
            PendingCommitOutcome::Written {
                id: upsert.id,
                inserted: upsert.inserted,
            }
        };
        tx.execute(
            "DELETE FROM pending_ingest WHERE source_id=?1 AND path=?2 AND revision=?3",
            params![
                target.source_id.as_bytes().to_vec(),
                target.path,
                target.revision
            ],
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)?;
        self.wake_ann(matches!(
            outcome,
            PendingCommitOutcome::Blocked {
                removed_asset: Some(_)
            }
        ));
        Ok(outcome)
    }
}

fn pending_row(row: &rusqlite::Row<'_>, source_id: SourceId) -> rusqlite::Result<PendingIngest> {
    let media: String = row.get(3)?;
    let media = MediaType::parse(&media).ok_or(rusqlite::Error::InvalidQuery)?;
    Ok(PendingIngest {
        source_id,
        path: row.get(0)?,
        size: row.get::<_, i64>(1)? as u64,
        modified_ms: row.get(2)?,
        media,
        format: row.get(4)?,
        revision: row.get(5)?,
        generation: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(store: &Store) -> SourceId {
        store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "test",
                false,
            )
            .unwrap()
    }
    fn entry(path: &str, size: u64, mtime: i64) -> PendingDiscovery {
        PendingDiscovery {
            path: path.into(),
            size,
            modified_ms: Some(mtime),
            media: Some(MediaType::Image),
            format: Some("png".into()),
        }
    }
    fn write(target: &PendingIngest, hash: u8) -> NewAsset {
        NewAsset {
            source_id: target.source_id,
            path: target.path.clone(),
            filename: target.path.clone(),
            content_hash: Some(ContentHash([hash; 32])),
            size_bytes: Some(target.size as i64),
            source_modified_at: target.modified_ms,
            scanned_at: now_ms(),
            media_type: target.media,
            format: target.format.clone(),
        }
    }
    fn job(store: &Store, source: SourceId) -> JobId {
        store
            .create_job(JobKind::Enrich, "{}", None, &[source])
            .unwrap()
    }

    #[test]
    fn discovery_never_admits_a_hashless_new_asset() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let generation = store.begin_source_scan(&source).unwrap();
        let outcome = store
            .apply_quick_discovery(
                &source,
                generation,
                &[entry("large.png", 8 * 1024 * 1024 * 1024, 10)],
            )
            .unwrap();
        assert_eq!(outcome.queued, 1);
        assert_eq!(store.pending_ingest_count(&source).unwrap(), 1);
        assert_eq!(store.list_sources().unwrap()[0].stats.asset_count, 0);
        let target = store.list_pending_ingest(&source, 128).unwrap().remove(0);
        let outcome = store
            .commit_pending_ingest(
                &job(&store, source),
                &target,
                &write(&target, 1),
                &MediaAttributes::Image(Default::default()),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            PendingCommitOutcome::Written { inserted: true, .. }
        ));
        assert_eq!(store.pending_ingest_count(&source).unwrap(), 0);
        assert_eq!(store.list_sources().unwrap()[0].stats.asset_count, 1);
    }

    #[test]
    fn changed_revision_clears_old_hash_metadata_vectors_and_gates_atomically() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let target = PendingIngest {
            source_id: source,
            path: "a.png".into(),
            size: 10,
            modified_ms: Some(1),
            media: MediaType::Image,
            format: "png".into(),
            revision: 0,
            generation: 0,
        };
        let (id, _) = store.upsert_asset(&write(&target, 1)).unwrap();
        store
            .set_media_attrs(
                &id,
                &MediaAttributes::Image(ImageAttributes {
                    width: Some(100),
                    height: Some(200),
                    ..Default::default()
                }),
            )
            .unwrap();
        {
            let conn = store.write();
            conn.execute("UPDATE asset SET analysis_version=9,analysed_at=1,derivative_version=9 WHERE id=?1",params![id.as_bytes().to_vec()]).unwrap();
            conn.execute("INSERT INTO embedding(asset_id,space_id,media_type,dim,vec,extractor,created_at) VALUES(?1,'image@v1','image',1,x'0000803f','test',0)",params![id.as_bytes().to_vec()]).unwrap();
        }
        {
            let conn = store.write();
            for (name, state, source) in [
                ("manual", "confirmed", "user"),
                ("automatic", "confirmed", "classifier"),
                ("rejected", "rejected", "classifier"),
            ] {
                let tag = Store::intern_tag(&conn, name).unwrap();
                conn.execute("INSERT INTO asset_tag(asset_id,tag_id,state,source,created_at) VALUES(?1,?2,?3,?4,0)", params![id.as_bytes().to_vec(),tag,state,source]).unwrap();
            }
            Store::reindex_asset_tags(&conn, &id);
        }
        let generation = store.begin_source_scan(&source).unwrap();
        let outcome = store
            .apply_quick_discovery(&source, generation, &[entry("a.png", 20, 2)])
            .unwrap();
        assert_eq!(outcome.invalidated, vec![id]);
        let asset = store.get_asset(&id).unwrap();
        assert_eq!(asset.hash, None);
        assert_eq!(asset.timestamps.analyzed, None);
        assert!(matches!(asset.attributes, MediaAttributes::None));
        let conn = store.read().unwrap();
        let state:(i64,i64,i64)=conn.query_row("SELECT analysis_version,derivative_version,(SELECT count(*) FROM embedding WHERE asset_id=a.id) FROM asset a WHERE id=?1",params![id.as_bytes().to_vec()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(state, (0, 0, 0));
        let tags = Store::load_tags(&conn, id.as_bytes());
        assert_eq!(tags.len(), 2);
        assert!(tags.iter().any(|tag| tag.name == "manual"));
        assert!(tags.iter().any(|tag| tag.name == "rejected"));
        let indexed: String = conn
            .query_row(
                "SELECT tags FROM asset_fts WHERE rowid=(SELECT rowid FROM asset WHERE id=?1)",
                params![id.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, "manual");
    }

    #[test]
    fn stale_worker_cannot_replace_a_newer_revision() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let generation = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(&source, generation, &[entry("a.png", 10, 1)])
            .unwrap();
        let stale = store
            .pending_ingest_for_path(&source, "a.png")
            .unwrap()
            .unwrap();
        store
            .apply_quick_discovery(&source, generation, &[entry("a.png", 20, 2)])
            .unwrap();
        let outcome = store
            .commit_pending_ingest(
                &job(&store, source),
                &stale,
                &write(&stale, 1),
                &MediaAttributes::Image(Default::default()),
            )
            .unwrap();
        assert!(matches!(outcome, PendingCommitOutcome::Stale));
        assert_eq!(store.list_sources().unwrap()[0].stats.asset_count, 0);
        assert_eq!(
            store
                .pending_ingest_for_path(&source, "a.png")
                .unwrap()
                .unwrap()
                .size,
            20
        );
    }

    #[test]
    fn unchanged_pending_revision_survives_a_new_discovery_generation() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let first = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(&source, first, &[entry("a.png", 10, 1)])
            .unwrap();
        let target = store
            .pending_ingest_for_path(&source, "a.png")
            .unwrap()
            .unwrap();
        let second = store.begin_source_scan(&source).unwrap();
        let outcome = store
            .apply_quick_discovery(&source, second, &[entry("a.png", 10, 1)])
            .unwrap();
        assert_eq!(outcome.queued, 0);
        let refreshed = store
            .pending_ingest_for_path(&source, "a.png")
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.revision, target.revision);
        assert_eq!(refreshed.generation, first);
        assert!(matches!(
            store
                .commit_pending_ingest(
                    &job(&store, source),
                    &target,
                    &write(&target, 1),
                    &MediaAttributes::Image(Default::default())
                )
                .unwrap(),
            PendingCommitOutcome::Written { .. }
        ));
    }

    #[test]
    fn unchanged_pending_discovery_does_not_rewrite_catalog_wal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let source = source(&store);
        let entries: Vec<_> = (0..1024)
            .map(|i| entry(&format!("{i}.png"), 10, 1))
            .collect();
        let first = store.begin_source_scan(&source).unwrap();
        for chunk in entries.chunks(128) {
            store.apply_quick_discovery(&source, first, chunk).unwrap();
        }
        let second = store.begin_source_scan(&source).unwrap();
        let (busy, _, _): (i64, i64, i64) = store
            .exclusive()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(
            busy, 0,
            "the fixture WAL baseline must actually be truncated"
        );
        let before = store
            .pending_ingest_for_path(&source, "0.png")
            .unwrap()
            .unwrap();
        for chunk in entries.chunks(128) {
            assert_eq!(
                store
                    .apply_quick_discovery(&source, second, chunk)
                    .unwrap()
                    .queued,
                0
            );
        }
        assert_eq!(store.pending_ingest_count(&source).unwrap(), 1024);
        let after = store
            .pending_ingest_for_path(&source, "0.png")
            .unwrap()
            .unwrap();
        assert_eq!(before.revision, after.revision);
        assert_eq!(before.generation, after.generation);
        assert_eq!(
            std::fs::metadata(dir.path().join("library.db-wal"))
                .unwrap()
                .len(),
            0
        );
        assert!(store
            .finish_pending_discovery(&source, second, &[])
            .unwrap());
        assert_eq!(store.pending_ingest_count(&source).unwrap(), 1024);
    }

    #[test]
    fn restart_without_observation_spool_never_deletes_pending_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let source;
        let generation;
        {
            let store = Store::open(dir.path()).unwrap();
            source = self::source(&store);
            generation = store.begin_source_scan(&source).unwrap();
            store
                .apply_quick_discovery(&source, generation, &[entry("a.png", 10, 1)])
                .unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        assert!(!store
            .finish_pending_discovery(&source, generation, &[])
            .unwrap());
        assert_eq!(store.pending_ingest_count(&source).unwrap(), 1);
    }

    #[test]
    fn unrelated_scoped_discovery_does_not_strand_pending_verification() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let first = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(&source, first, &[entry("Art/a.png", 10, 1)])
            .unwrap();
        let target = store
            .pending_ingest_for_path(&source, "Art/a.png")
            .unwrap()
            .unwrap();
        let second = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(&source, second, &[entry("Other/b.png", 10, 1)])
            .unwrap();
        store
            .finish_pending_discovery(&source, second, &["Other".into()])
            .unwrap();
        assert!(matches!(
            store
                .commit_pending_ingest(
                    &job(&store, source),
                    &target,
                    &write(&target, 1),
                    &MediaAttributes::None
                )
                .unwrap(),
            PendingCommitOutcome::Written { .. }
        ));
    }

    #[test]
    fn recreated_queue_rows_never_match_an_old_workers_revision() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let first = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(&source, first, &[entry("a.png", 10, 1)])
            .unwrap();
        let old = store
            .pending_ingest_for_path(&source, "a.png")
            .unwrap()
            .unwrap();
        let job = job(&store, source);
        store
            .commit_pending_ingest(
                &job,
                &old,
                &write(&old, 1),
                &MediaAttributes::Image(Default::default()),
            )
            .unwrap();
        let second = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(&source, second, &[entry("a.png", 20, 2)])
            .unwrap();
        store
            .apply_quick_discovery(&source, second, &[entry("a.png", 10, 1)])
            .unwrap();
        let new = store
            .pending_ingest_for_path(&source, "a.png")
            .unwrap()
            .unwrap();
        assert!(new.revision > old.revision);
        assert!(matches!(
            store
                .commit_pending_ingest(
                    &job,
                    &old,
                    &write(&old, 1),
                    &MediaAttributes::Image(Default::default())
                )
                .unwrap(),
            PendingCommitOutcome::Stale
        ));
    }

    #[test]
    fn hashless_and_cancelled_commits_cannot_admit_pending_paths() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let generation = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(&source, generation, &[entry("a.png", 10, 1)])
            .unwrap();
        let target = store.list_pending_ingest(&source, 1).unwrap().remove(0);
        let job = job(&store, source);
        let mut asset = write(&target, 1);
        asset.content_hash = None;
        assert!(store
            .commit_pending_ingest(&job, &target, &asset, &MediaAttributes::None)
            .is_err());
        store
            .set_job_state(&job, JobState::Cancelled, None)
            .unwrap();
        assert!(matches!(
            store
                .commit_pending_ingest(&job, &target, &write(&target, 1), &MediaAttributes::None)
                .unwrap(),
            PendingCommitOutcome::Cancelled
        ));
        assert_eq!(store.pending_ingest_count(&source).unwrap(), 1);
    }

    #[test]
    fn blocklist_is_checked_inside_verified_admission() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let generation = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(&source, generation, &[entry("a.png", 10, 1)])
            .unwrap();
        let target = store.list_pending_ingest(&source, 1).unwrap().remove(0);
        store
            .write()
            .execute(
                "INSERT INTO blocklist(content_hash,label,blocked_at) VALUES(?1,'blocked',0)",
                params![vec![1u8; 32]],
            )
            .unwrap();
        assert!(matches!(
            store
                .commit_pending_ingest(
                    &job(&store, source),
                    &target,
                    &write(&target, 1),
                    &MediaAttributes::Image(Default::default())
                )
                .unwrap(),
            PendingCommitOutcome::Blocked {
                removed_asset: None
            }
        ));
        assert_eq!(store.list_sources().unwrap()[0].stats.asset_count, 0);
        assert_eq!(store.pending_ingest_count(&source).unwrap(), 0);
    }

    #[test]
    fn scoped_missing_reconciliation_preserves_neighbouring_branches() {
        let store = Store::open_in_memory().unwrap();
        let source = source(&store);
        let first = store.begin_source_scan(&source).unwrap();
        store
            .apply_quick_discovery(
                &source,
                first,
                &[entry("Art/a.png", 10, 1), entry("Artist/b.png", 10, 1)],
            )
            .unwrap();
        let second = store.begin_source_scan(&source).unwrap();
        assert_eq!(
            store.pending_ingest_count(&source).unwrap(),
            2,
            "beginning a scan is never proof of absence"
        );
        store
            .finish_pending_discovery(&source, second, &["Art".into()])
            .unwrap();
        assert!(store
            .pending_ingest_for_path(&source, "Art/a.png")
            .unwrap()
            .is_none());
        assert!(store
            .pending_ingest_for_path(&source, "Artist/b.png")
            .unwrap()
            .is_some());
    }

    #[test]
    fn pending_revisions_and_retry_state_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let source;
        let revision;
        {
            let store = Store::open(dir.path()).unwrap();
            source = self::source(&store);
            let generation = store.begin_source_scan(&source).unwrap();
            store
                .apply_quick_discovery(&source, generation, &[entry("a.png", 10, 1)])
                .unwrap();
            let target = store.list_pending_ingest(&source, 1).unwrap().remove(0);
            revision = target.revision;
            store
                .retry_pending_ingest(&target, "temporary source fault")
                .unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        let target = store
            .pending_ingest_for_path(&source, "a.png")
            .unwrap()
            .unwrap();
        assert_eq!(target.revision, revision);
        assert_eq!(store.pending_ingest_count(&source).unwrap(), 1);
        assert!(
            store.list_pending_ingest(&source, 1).unwrap().is_empty(),
            "restart must respect retry backoff"
        );
    }
}
