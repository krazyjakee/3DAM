//! Bounded-memory export reads. This is deliberately a store concern: selection predicates and
//! hydration have to remain in one set-based SQLite query rather than becoming an id query followed
//! by one `get_asset` call per result.

use super::*;
use crate::helpers::*;

/// The fields consumed by every manifest encoder. Unlike [`Asset`], this does not hydrate media
/// attributes, collections, rejected/suggested tags, or timestamps that an export never writes.
#[derive(Debug)]
pub struct ExportAssetRow {
    pub id: AssetId,
    pub name: String,
    pub path: String,
    pub media: MediaType,
    pub format: String,
    pub size_bytes: u64,
    pub hash: Option<ContentHash>,
    pub license_id: Option<String>,
    pub license_status: LicenseStatus,
    pub commercial: Option<bool>,
    pub modify: Option<bool>,
    pub redistribute: Option<bool>,
    pub attribution: Option<bool>,
    pub attribution_holder: Option<String>,
    pub attribution_credit: Option<String>,
    pub license_url: Option<String>,
    /// Confirmed tags in the same deterministic name order used by `get_asset`.
    pub tags: String,
    pub note: String,
}

/// An already-authorized export selector. Smart collections are represented by their saved query;
/// manual collections retain their membership ordering.
pub enum ExportSelection {
    Assets(Vec<AssetId>),
    ManualCollection(CollectionId),
    Query(QueryRequest),
}

/// Observable bounds for regression tests and diagnostics. Query count grows by batches, never by
/// assets; `max_batch_rows` is the peak number of hydrated rows retained by this API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportStreamStats {
    pub rows: u64,
    pub queries: u64,
    pub max_batch_rows: usize,
}

const EXPORT_COLUMNS: &str = "asset.id, asset.filename, asset.path, asset.media_type, asset.format,
    asset.size_bytes + COALESCE(model_attr.dependency_bytes, 0), asset.content_hash,
    asset.license_id, asset.license_status, asset.rights_commercial, asset.rights_modify,
    asset.rights_redistribute, asset.rights_attribution, asset.attribution_holder,
    asset.attribution_credit, asset.license_url,
    COALESCE((SELECT group_concat(name, ';') FROM
        (SELECT tag.name AS name FROM asset_tag
         JOIN tag ON tag.id = asset_tag.tag_id
         WHERE asset_tag.asset_id = asset.id AND asset_tag.state = 'confirmed'
         ORDER BY tag.name)), ''),
    COALESCE(asset_note.body, '')";

impl Store {
    /// Resolve source attribution for a large explicit input set without one query per asset.
    pub fn asset_sources(&self, ids: &[AssetId]) -> Result<Vec<SourceId>, LibError> {
        let mut sources = std::collections::BTreeSet::new();
        for ids in ids.chunks(256) {
            if ids.is_empty() {
                continue;
            }
            let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(",");
            let binds = ids
                .iter()
                .map(|id| Value::Blob(id.as_bytes().to_vec()))
                .collect::<Vec<_>>();
            let sql = format!("SELECT DISTINCT source_id FROM asset WHERE id IN ({placeholders})");
            let conn = self.read()?;
            let mut statement = conn.prepare(&sql).map_err(internal)?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    let source: Vec<u8> = row.get(0)?;
                    Ok(SourceId(uuid_from_slice(&source)))
                })
                .map_err(internal)?;
            for source in rows {
                sources.insert(source.map_err(internal)?);
            }
        }
        Ok(sources.into_iter().collect())
    }

    /// Set-based visibility validation for large explicit job inputs. Batches stay below SQLite's
    /// bind limit and replace the former one async store round-trip per asset.
    pub fn assets_visible(&self, ids: &[AssetId], vis: &Visibility) -> Result<bool, LibError> {
        for ids in ids.chunks(256) {
            if ids.is_empty() {
                continue;
            }
            let values = (0..ids.len()).map(|_| "(?)").collect::<Vec<_>>().join(",");
            let mut binds: Vec<Value> = ids
                .iter()
                .map(|id| Value::Blob(id.as_bytes().to_vec()))
                .collect();
            let mut where_sql = " WHERE 1=1".to_string();
            push_visibility(vis, "asset", &mut where_sql, &mut binds);
            let sql = format!(
                "WITH selected(asset_id) AS (VALUES {values})
                 SELECT COUNT(*) FROM selected JOIN asset ON asset.id = selected.asset_id
                 {where_sql}"
            );
            let conn = self.read()?;
            let visible: i64 = conn
                .query_row(&sql, rusqlite::params_from_iter(binds.iter()), |row| {
                    row.get(0)
                })
                .map_err(internal)?;
            if visible != ids.len() as i64 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Walk an export selection in bounded batches. Each batch is hydrated by one set-based query
    /// and handed to `visit` after the SQLite connection lock is released, so filesystem encoders
    /// never hold the catalog lock. A future background export job can checkpoint cancellation and
    /// progress in `visit` without changing the query or public export DTOs.
    pub fn stream_export_rows(
        &self,
        selection: &ExportSelection,
        vis: &Visibility,
        batch_size: usize,
        mut visit: impl FnMut(&[ExportAssetRow]) -> Result<(), LibError>,
    ) -> Result<ExportStreamStats, LibError> {
        if batch_size == 0 {
            return Err(LibError::BadRequest(
                "export batch size must be greater than zero".into(),
            ));
        }
        let mut stats = ExportStreamStats::default();
        match selection {
            ExportSelection::Assets(ids) => {
                for ids in ids.chunks(batch_size) {
                    let batch = self.export_explicit_batch(ids, vis)?;
                    stats.queries += 1;
                    stats.rows += batch.len() as u64;
                    stats.max_batch_rows = stats.max_batch_rows.max(batch.len());
                    visit(&batch)?;
                }
            }
            ExportSelection::ManualCollection(collection) => {
                let mut cursor: Option<(i64, AssetId)> = None;
                loop {
                    let batch =
                        self.export_collection_batch(collection, vis, cursor, batch_size)?;
                    stats.queries += 1;
                    if batch.is_empty() {
                        break;
                    }
                    cursor = batch
                        .last()
                        .map(|(_, added_at)| (*added_at, batch.last().unwrap().0.id));
                    let rows: Vec<ExportAssetRow> = batch.into_iter().map(|(row, _)| row).collect();
                    let len = rows.len();
                    stats.rows += len as u64;
                    stats.max_batch_rows = stats.max_batch_rows.max(len);
                    visit(&rows)?;
                    if len < batch_size {
                        break;
                    }
                }
            }
            ExportSelection::Query(req) => {
                let mut cursor: Option<(String, AssetId)> = None;
                loop {
                    let batch = self.export_query_batch(req, vis, cursor.as_ref(), batch_size)?;
                    stats.queries += 1;
                    if batch.is_empty() {
                        break;
                    }
                    cursor = batch.last().map(|row| (row.name.clone(), row.id));
                    let len = batch.len();
                    stats.rows += len as u64;
                    stats.max_batch_rows = stats.max_batch_rows.max(len);
                    visit(&batch)?;
                    if len < batch_size {
                        break;
                    }
                }
            }
        }
        Ok(stats)
    }

    fn export_explicit_batch(
        &self,
        ids: &[AssetId],
        vis: &Visibility,
    ) -> Result<Vec<ExportAssetRow>, LibError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let values = (0..ids.len())
            .map(|_| "(?, ?)")
            .collect::<Vec<_>>()
            .join(",");
        let mut binds = Vec::with_capacity(ids.len() * 2);
        for (position, id) in ids.iter().enumerate() {
            binds.push(Value::Integer(position as i64));
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        let mut where_sql = " WHERE 1=1".to_string();
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let sql = format!(
            "WITH selected(position, asset_id) AS (VALUES {values})
             SELECT {EXPORT_COLUMNS} FROM selected
             JOIN asset ON asset.id = selected.asset_id
             LEFT JOIN model_attr ON model_attr.asset_id = asset.id
             LEFT JOIN asset_note ON asset_note.asset_id = asset.id
             {where_sql} ORDER BY selected.position"
        );
        self.read_export_rows(&sql, &binds)
    }

    fn export_collection_batch(
        &self,
        collection: &CollectionId,
        vis: &Visibility,
        cursor: Option<(i64, AssetId)>,
        batch_size: usize,
    ) -> Result<Vec<(ExportAssetRow, i64)>, LibError> {
        let mut where_sql = " WHERE cm.collection_id = ?".to_string();
        let mut binds = vec![Value::Blob(collection.as_bytes().to_vec())];
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        if let Some((added_at, id)) = cursor {
            where_sql.push_str(" AND (cm.added_at < ? OR (cm.added_at = ? AND asset.id > ?))");
            binds.push(Value::Integer(added_at));
            binds.push(Value::Integer(added_at));
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        binds.push(Value::Integer(batch_size as i64));
        let sql = format!(
            "SELECT {EXPORT_COLUMNS}, cm.added_at FROM collection_member cm
             JOIN asset ON asset.id = cm.asset_id
             LEFT JOIN model_attr ON model_attr.asset_id = asset.id
             LEFT JOIN asset_note ON asset_note.asset_id = asset.id
             {where_sql} ORDER BY cm.added_at DESC, asset.id ASC LIMIT ?"
        );
        let conn = self.read()?;
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                Ok((row_to_export(r)?, r.get(18)?))
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    fn export_query_batch(
        &self,
        req: &QueryRequest,
        vis: &Visibility,
        cursor: Option<&(String, AssetId)>,
        batch_size: usize,
    ) -> Result<Vec<ExportAssetRow>, LibError> {
        let (mut where_sql, mut binds) = build_where(req, &self.synonyms, vis)?;
        if let Some((name, id)) = cursor {
            where_sql
                .push_str(" AND (asset.filename > ? OR (asset.filename = ? AND asset.id > ?))");
            binds.push(Value::Text(name.clone()));
            binds.push(Value::Text(name.clone()));
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        binds.push(Value::Integer(batch_size as i64));
        let sql = format!(
            "SELECT {EXPORT_COLUMNS} FROM asset {ATTR_JOINS}
             LEFT JOIN asset_note ON asset_note.asset_id = asset.id
             {where_sql} ORDER BY asset.filename ASC, asset.id ASC LIMIT ?"
        );
        self.read_export_rows(&sql, &binds)
    }

    fn read_export_rows(
        &self,
        sql: &str,
        binds: &[Value],
    ) -> Result<Vec<ExportAssetRow>, LibError> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), row_to_export)
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }
}

fn row_to_export(r: &rusqlite::Row<'_>) -> rusqlite::Result<ExportAssetRow> {
    let hash: Option<Vec<u8>> = r.get(6)?;
    Ok(ExportAssetRow {
        id: blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?),
        name: r.get(1)?,
        path: r.get(2)?,
        media: MediaType::parse(&r.get::<_, String>(3)?).unwrap_or(MediaType::Image),
        format: r.get(4)?,
        size_bytes: r.get::<_, Option<i64>>(5)?.unwrap_or(0).max(0) as u64,
        hash: hash
            .and_then(|blob| <[u8; 32]>::try_from(blob.as_slice()).ok())
            .map(ContentHash),
        license_id: r.get(7)?,
        license_status: LicenseStatus::parse(&r.get::<_, String>(8)?),
        commercial: r.get::<_, Option<i64>>(9)?.map(|v| v != 0),
        modify: r.get::<_, Option<i64>>(10)?.map(|v| v != 0),
        redistribute: r.get::<_, Option<i64>>(11)?.map(|v| v != 0),
        attribution: r.get::<_, Option<i64>>(12)?.map(|v| v != 0),
        attribution_holder: r.get(13)?,
        attribution_credit: r.get(14)?,
        license_url: r.get(15)?,
        tags: r.get(16)?,
        note: r.get(17)?,
    })
}
