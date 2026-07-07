//! Faceted query, id enumeration, and library stats — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    /// Faceted query → a page of summaries. Cursor is an offset (slice-simple; keyset later).
    pub fn query_assets(&self, req: &QueryRequest) -> Result<Page<AssetSummary>, LibError> {
        let limit = req.page.clamped(QUERY_MAX_LIMIT);
        let offset = decode_offset(req.page.after.as_ref())?;

        let (where_sql, binds) = build_where(req)?;

        let dir = match req.sort.dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };
        // ORDER BY, plus any binds it needs (only relevance, which references the search term). A
        // relevance sort without a text query has nothing to rank, so it degrades to name order.
        let rank_text = req.text.as_ref().filter(|t| !t.is_empty());
        let (order_clause, order_binds): (String, Vec<Value>) = match req.sort.field {
            SortField::Relevance if rank_text.is_some() => (
                // No FTS in v1 — a cheap proxy over the filename LIKE match: earliest substring hit
                // wins, then the shortest name (closest to an exact match), then name for stability.
                "INSTR(LOWER(filename), LOWER(?)) ASC, LENGTH(filename) ASC, filename ASC".into(),
                vec![Value::Text(rank_text.unwrap().clone())],
            ),
            SortField::Relevance | SortField::Name => {
                (format!("filename {dir}, asset.id ASC"), Vec::new())
            }
            SortField::Size => (format!("size_bytes {dir}, asset.id ASC"), Vec::new()),
            SortField::Scanned => (format!("scanned_at {dir}, asset.id ASC"), Vec::new()),
        };

        let conn = self.conn.lock().unwrap();

        // Total for this filter (best-effort; cheap enough at slice scale).
        let count_sql = format!("SELECT COUNT(*) FROM asset{where_sql}");
        let total: i64 = conn
            .query_row(&count_sql, rusqlite::params_from_iter(binds.iter()), |r| {
                r.get(0)
            })
            .map_err(internal)?;

        // LEFT JOIN the per-type attr tables so each grid row carries a couple of cheap key
        // attributes (dimensions / duration / triangles) without an N+1 fetch. Column names stay
        // unambiguous across the joined tables, so the bare-name filters above keep working.
        let sql = format!(
            "SELECT asset.id, filename, media_type, format, size_bytes, license_id, license_status,
                    image_attr.width, image_attr.height, audio_attr.duration_ms, model_attr.triangle_count,
                    audio_attr.class
             FROM asset
             LEFT JOIN image_attr ON image_attr.asset_id = asset.id
             LEFT JOIN audio_attr ON audio_attr.asset_id = asset.id
             LEFT JOIN model_attr ON model_attr.asset_id = asset.id
             {where_sql} ORDER BY {order_clause} LIMIT ? OFFSET ?"
        );
        // Bind order is positional across the whole statement: WHERE binds, then the ORDER BY term,
        // then LIMIT/OFFSET.
        let mut page_binds = binds.clone();
        page_binds.extend(order_binds);
        page_binds.push(Value::Integer(limit as i64));
        page_binds.push(Value::Integer(offset as i64));

        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(page_binds.iter()), |r| {
                let id = blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?);
                let name: String = r.get(1)?;
                let media_s: String = r.get(2)?;
                let format: String = r.get(3)?;
                let size: Option<i64> = r.get(4)?;
                let license_id: Option<String> = r.get(5)?;
                let license_status: String = r.get(6)?;
                let media = MediaType::parse(&media_s).unwrap_or(MediaType::Image);
                let width: Option<i64> = r.get(7)?;
                let height: Option<i64> = r.get(8)?;
                let duration_ms: Option<i64> = r.get(9)?;
                let tri_count: Option<i64> = r.get(10)?;
                let audio_class: Option<String> = r.get(11)?;
                Ok(AssetSummary {
                    id,
                    name,
                    media,
                    format,
                    size: size.unwrap_or(0) as u64,
                    license: LicenseBadge {
                        id: license_id,
                        status: LicenseStatus::parse(&license_status),
                    },
                    top_tags: Vec::new(),
                    origin: Origin::Local,
                    key_attrs: grid_key_attrs(
                        media,
                        width,
                        height,
                        duration_ms,
                        tri_count,
                        audio_class.as_deref(),
                    ),
                })
            })
            .map_err(internal)?;
        let mut items = Vec::new();
        for r in rows {
            items.push(r.map_err(internal)?);
        }

        let next = if (offset + items.len()) < total as usize {
            Some(Cursor((offset + items.len()).to_string()))
        } else {
            None
        };
        Ok(Page {
            items,
            cursor: next,
            total: Some(total as u64),
            partial: Default::default(),
        })
    }

    /// Every asset id matching a query's text + filters, ordered by name — the unbounded id set an
    /// export or smart-folder resolution walks (no pagination). Ignores `page`/`sort`/`facets`.
    pub fn query_asset_ids(&self, req: &QueryRequest) -> Result<Vec<AssetId>, LibError> {
        let (where_sql, binds) = build_where(req)?;
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT asset.id FROM asset
             LEFT JOIN image_attr ON image_attr.asset_id = asset.id
             LEFT JOIN audio_attr ON audio_attr.asset_id = asset.id
             LEFT JOIN model_attr ON model_attr.asset_id = asset.id
             {where_sql} ORDER BY filename ASC, asset.id ASC"
        );
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                Ok(blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(internal)?);
        }
        Ok(out)
    }

    /// All member ids of a collection (unbounded), newest-added first.
    pub fn collection_member_ids(&self, id: &CollectionId) -> Result<Vec<AssetId>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT asset_id FROM collection_member WHERE collection_id = ?1 ORDER BY added_at DESC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![id.as_bytes().to_vec()], |r| {
                Ok(blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(internal)?);
        }
        Ok(out)
    }

    pub fn stats(&self) -> Result<LibraryStats, LibError> {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM asset", [], |r| r.get(0))
            .map_err(internal)?;
        let unanalyzed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset WHERE analysed_at IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(internal)?;
        let sources: i64 = conn
            .query_row("SELECT COUNT(*) FROM source", [], |r| r.get(0))
            .map_err(internal)?;

        let mut by_media = CountMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT media_type, COUNT(*) FROM asset GROUP BY media_type")
                .map_err(internal)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(internal)?;
            for r in rows {
                let (k, v) = r.map_err(internal)?;
                by_media.insert(k, v as u64);
            }
        }
        let mut by_source = CountMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT s.name, COUNT(a.id) FROM source s
                     LEFT JOIN asset a ON a.source_id = s.id GROUP BY s.id",
                )
                .map_err(internal)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(internal)?;
            for r in rows {
                let (k, v) = r.map_err(internal)?;
                by_source.insert(k, v as u64);
            }
        }
        Ok(LibraryStats {
            total: total as u64,
            by_media,
            by_source,
            unanalyzed: unanalyzed as u64,
            sources: sources as u64,
        })
    }
}
