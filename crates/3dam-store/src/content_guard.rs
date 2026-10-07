//! Revision checks shared by content serving and transaction-bound background writes.

use super::*;

impl Store {
    /// Related source files may be pending before their first catalog row exists.
    pub fn ensure_source_path_ready(&self, source: &SourceId, path: &str) -> Result<(), LibError> {
        let conn = self.read()?;
        let pending: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_ingest WHERE source_id=?1 AND path=?2)",
                params![source.as_bytes().to_vec(), path],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if pending {
            return Err(LibError::Conflict(
                "related content revision is awaiting enrichment".into(),
            ));
        }
        Ok(())
    }

    /// A caller may retain an Asset while quick discovery invalidates its bytes. Check the
    /// current row as well as the durable pending queue before serving any content-keyed result.
    pub fn ensure_content_ready(&self, asset: &Asset) -> Result<(), LibError> {
        let conn = self.read()?;
        Self::ensure_content_ready_in(&conn, &asset.summary.id, Some(asset.hash))
    }

    pub(crate) fn ensure_content_ready_in(
        conn: &Connection,
        id: &AssetId,
        expected_hash: Option<Option<ContentHash>>,
    ) -> Result<(), LibError> {
        let row = conn
            .query_row(
                "SELECT a.content_hash, EXISTS(SELECT 1 FROM pending_ingest p
                WHERE p.source_id=a.source_id AND p.path=a.path)
             FROM asset a WHERE a.id=?1",
                params![id.as_bytes().to_vec()],
                |row| Ok((row.get::<_, Option<Vec<u8>>>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()
            .map_err(internal)?;
        let Some((current_hash, pending)) = row else {
            return Err(LibError::NotFound(format!("asset {id}")));
        };
        if pending {
            return Err(LibError::Conflict(
                "asset content revision is awaiting enrichment".into(),
            ));
        }
        if let Some(expected_hash) = expected_hash {
            let expected = expected_hash.map(|hash| hash.as_bytes().to_vec());
            if current_hash != expected {
                return Err(LibError::Conflict(
                    "asset content changed while work was in progress".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retained_asset_cannot_serve_pending_or_subsequently_verified_different_content() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/revision-test".into(),
                },
                "test",
                false,
            )
            .unwrap();
        let (id, _) = store
            .upsert_asset(&NewAsset {
                source_id: source,
                path: "asset.png".into(),
                filename: "asset.png".into(),
                content_hash: Some(ContentHash([1; 32])),
                size_bytes: Some(1),
                source_modified_at: Some(1),
                scanned_at: now_ms(),
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        let retained = store.get_asset(&id).unwrap();
        store.ensure_content_ready(&retained).unwrap();
        {
            let conn = store.write();
            conn.execute("INSERT INTO pending_ingest(source_id,path,size_bytes,source_modified_at,media_type,format,revision,seen_generation,queued_at)
                VALUES(?1,'asset.png',2,2,'image','png',1,0,0)", params![source.as_bytes().to_vec()]).unwrap();
        }
        assert!(matches!(
            store.ensure_content_ready(&retained),
            Err(LibError::Conflict(_))
        ));
        {
            let conn = store.write();
            conn.execute("DELETE FROM pending_ingest", []).unwrap();
            conn.execute(
                "UPDATE asset SET content_hash=?1 WHERE id=?2",
                params![vec![2_u8; 32], id.as_bytes().to_vec()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.ensure_content_ready(&retained),
            Err(LibError::Conflict(_))
        ));
        store
            .ensure_content_ready(&store.get_asset(&id).unwrap())
            .unwrap();
    }
}
