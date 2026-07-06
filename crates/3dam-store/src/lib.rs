//! `dam-store` — the SQLite metadata store (tech-spec 02). Private to `3dam-core`; no front-end
//! links it (dependency rule 4, tech-spec 01 §2). Methods are synchronous and internally locked;
//! `3dam-core` calls them from a blocking context off the async runtime (tech-spec 14).

mod schema;

use dam_api::dto::*;
use dam_api::id::{AssetId, ContentHash, JobId, SourceId};
use dam_api::page::{Cursor, Page};
use dam_api::LibError;
use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const QUERY_MAX_LIMIT: u32 = 500;

/// Unix epoch milliseconds — the time unit used across the schema (tech-spec 02 §3.1).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn internal<E: std::fmt::Display>(e: E) -> LibError {
    LibError::Internal(e.to_string())
}

/// A row to insert/reconcile during a scan.
pub struct NewAsset {
    pub source_id: SourceId,
    pub path: String,
    pub filename: String,
    pub content_hash: Option<ContentHash>,
    pub size_bytes: Option<i64>,
    pub source_modified_at: Option<i64>,
    pub scanned_at: i64,
    pub media_type: MediaType,
    pub format: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) `library.db` in `data_dir`, applying pending migrations.
    pub fn open(data_dir: &Path) -> Result<Store, LibError> {
        std::fs::create_dir_all(data_dir).map_err(internal)?;
        let db_path = data_dir.join("library.db");
        let conn = Connection::open(&db_path).map_err(internal)?;
        Self::from_conn(conn)
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory() -> Result<Store, LibError> {
        let conn = Connection::open_in_memory().map_err(internal)?;
        Self::from_conn(conn)
    }

    fn from_conn(conn: Connection) -> Result<Store, LibError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(internal)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(internal)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(internal)?;
        let store = Store {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), LibError> {
        let mut conn = self.conn.lock().unwrap();
        let current: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(internal)?;
        let target = schema::MIGRATIONS.len() as i64;
        if current > target {
            return Err(LibError::Internal(format!(
                "library is schema v{current} but this build only understands v{target}; upgrade 3dam"
            )));
        }
        for (i, step) in schema::MIGRATIONS.iter().enumerate() {
            let version = (i + 1) as i64;
            if version > current {
                let tx = conn.transaction().map_err(internal)?;
                tx.execute_batch(step).map_err(internal)?;
                tx.pragma_update(None, "user_version", version)
                    .map_err(internal)?;
                tx.commit().map_err(internal)?;
                tracing::info!(version, "applied migration");
            }
        }
        Ok(())
    }

    // ── sources ──────────────────────────────────────────────────────────────

    pub fn add_source(
        &self,
        kind: SourceKind,
        uri: &str,
        name: &str,
        watch: bool,
    ) -> Result<SourceId, LibError> {
        let id = SourceId::new();
        let now = now_ms();
        let connection = serde_json::json!({ "uri": uri }).to_string();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO source (id, name, kind, connection, online, watch, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?6)",
            params![
                id.as_bytes().to_vec(),
                name,
                kind.as_str(),
                connection,
                watch as i64,
                now,
            ],
        )
        .map_err(internal)?;
        Ok(id)
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(internal)?);
        }
        Ok(out)
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
        let uri = serde_json::from_str::<serde_json::Value>(&connection)
            .ok()
            .and_then(|v| v.get("uri").and_then(|u| u.as_str().map(String::from)))
            .unwrap_or_default();
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

    // ── assets ─────────────────────────────────────────────────────────────

    /// Insert a new asset or reconcile an existing `(source_id, path)` row (delta re-scan).
    /// Returns the id and whether it was newly inserted.
    pub fn upsert_asset(&self, a: &NewAsset) -> Result<(AssetId, bool), LibError> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<Vec<u8>> = conn
            .query_row(
                "SELECT id FROM asset WHERE source_id = ?1 AND path = ?2",
                params![a.source_id.as_bytes().to_vec(), a.path],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        let hash_blob = a.content_hash.map(|h| h.as_bytes().to_vec());
        let now = now_ms();
        if let Some(id_blob) = existing {
            conn.execute(
                "UPDATE asset SET content_hash = ?2, filename = ?3, size_bytes = ?4,
                    source_modified_at = ?5, scanned_at = ?6, media_type = ?7, format = ?8,
                    updated_at = ?9 WHERE id = ?1",
                params![
                    id_blob,
                    hash_blob,
                    a.filename,
                    a.size_bytes,
                    a.source_modified_at,
                    a.scanned_at,
                    a.media_type.as_str(),
                    a.format,
                    now,
                ],
            )
            .map_err(internal)?;
            Ok((blob_to_asset_id(&id_blob), false))
        } else {
            let id = AssetId::new();
            conn.execute(
                "INSERT INTO asset (id, content_hash, source_id, path, filename, size_bytes,
                    source_modified_at, scanned_at, media_type, format, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                params![
                    id.as_bytes().to_vec(),
                    hash_blob,
                    a.source_id.as_bytes().to_vec(),
                    a.path,
                    a.filename,
                    a.size_bytes,
                    a.source_modified_at,
                    a.scanned_at,
                    a.media_type.as_str(),
                    a.format,
                    now,
                ],
            )
            .map_err(internal)?;
            Ok((id, true))
        }
    }

    /// Persist the cheap-tier media attributes into the per-type attr table (tech-spec 02 §3.2,
    /// 04 §5). Idempotent upsert keyed by `asset_id`; called after each `upsert_asset` during a scan.
    pub fn set_media_attrs(&self, id: &AssetId, attrs: &MediaAttributes) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let key = id.as_bytes().to_vec();
        match attrs {
            MediaAttributes::Audio(a) => {
                conn.execute(
                    "INSERT INTO audio_attr (asset_id, duration_ms, sample_rate, bit_depth, channels, codec, container)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(asset_id) DO UPDATE SET
                        duration_ms=excluded.duration_ms, sample_rate=excluded.sample_rate,
                        bit_depth=excluded.bit_depth, channels=excluded.channels,
                        codec=excluded.codec, container=excluded.container",
                    params![key, a.duration_ms, a.sample_rate, a.bit_depth, a.channels, a.codec, a.container],
                )
                .map_err(internal)?;
            }
            MediaAttributes::Image(i) => {
                conn.execute(
                    "INSERT INTO image_attr (asset_id, width, height, color_depth, has_alpha, color_space)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(asset_id) DO UPDATE SET
                        width=excluded.width, height=excluded.height, color_depth=excluded.color_depth,
                        has_alpha=excluded.has_alpha, color_space=excluded.color_space",
                    params![
                        key,
                        i.width,
                        i.height,
                        i.color_depth,
                        i.has_alpha.map(|b| b as i64),
                        i.color_space,
                    ],
                )
                .map_err(internal)?;
            }
            MediaAttributes::Model(m) => {
                conn.execute(
                    "INSERT INTO model_attr (asset_id, vertex_count, triangle_count, mesh_count,
                        material_count, texture_count, has_rig, has_animation, has_uv)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(asset_id) DO UPDATE SET
                        vertex_count=excluded.vertex_count, triangle_count=excluded.triangle_count,
                        mesh_count=excluded.mesh_count, material_count=excluded.material_count,
                        texture_count=excluded.texture_count, has_rig=excluded.has_rig,
                        has_animation=excluded.has_animation, has_uv=excluded.has_uv",
                    params![
                        key,
                        m.vertex_count,
                        m.triangle_count,
                        m.mesh_count,
                        m.material_count,
                        m.texture_count,
                        m.has_rig.map(|b| b as i64),
                        m.has_animation.map(|b| b as i64),
                        m.has_uvs.map(|b| b as i64),
                    ],
                )
                .map_err(internal)?;
            }
            MediaAttributes::None => {}
        }
        Ok(())
    }

    /// Load the media-specific attribute struct for an asset (the per-type attr table), or `None`
    /// if the cheap tier has not run / found nothing.
    fn load_media_attrs(conn: &Connection, id_blob: &[u8], media: MediaType) -> MediaAttributes {
        match media {
            MediaType::Audio => conn
                .query_row(
                    "SELECT duration_ms, sample_rate, bit_depth, channels, codec, container FROM audio_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        Ok(AudioAttributes {
                            duration_ms: r.get(0)?,
                            sample_rate: r.get(1)?,
                            bit_depth: r.get(2)?,
                            channels: r.get(3)?,
                            codec: r.get(4)?,
                            container: r.get(5)?,
                        })
                    },
                )
                .optional()
                .ok()
                .flatten()
                .map(MediaAttributes::Audio)
                .unwrap_or(MediaAttributes::None),
            MediaType::Image => conn
                .query_row(
                    "SELECT width, height, color_depth, has_alpha, color_space FROM image_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        Ok(ImageAttributes {
                            width: r.get(0)?,
                            height: r.get(1)?,
                            color_depth: r.get(2)?,
                            has_alpha: r.get::<_, Option<i64>>(3)?.map(|v| v != 0),
                            color_space: r.get(4)?,
                        })
                    },
                )
                .optional()
                .ok()
                .flatten()
                .map(MediaAttributes::Image)
                .unwrap_or(MediaAttributes::None),
            MediaType::Model => conn
                .query_row(
                    "SELECT vertex_count, triangle_count, mesh_count, material_count, texture_count,
                            has_rig, has_animation, has_uv FROM model_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        Ok(ModelAttributes {
                            vertex_count: r.get(0)?,
                            triangle_count: r.get(1)?,
                            mesh_count: r.get(2)?,
                            material_count: r.get(3)?,
                            texture_count: r.get(4)?,
                            has_rig: r.get::<_, Option<i64>>(5)?.map(|v| v != 0),
                            has_animation: r.get::<_, Option<i64>>(6)?.map(|v| v != 0),
                            has_uvs: r.get::<_, Option<i64>>(7)?.map(|v| v != 0),
                        })
                    },
                )
                .optional()
                .ok()
                .flatten()
                .map(MediaAttributes::Model)
                .unwrap_or(MediaAttributes::None),
        }
    }

    pub fn get_asset(&self, id: &AssetId) -> Result<Asset, LibError> {
        let conn = self.conn.lock().unwrap();
        let asset = conn
            .query_row(
                "SELECT id, content_hash, source_id, path, filename, size_bytes,
                        source_created_at, source_modified_at, scanned_at, analysed_at,
                        media_type, format, license_id, license_status, license_provenance,
                        rights_commercial, rights_modify, rights_redistribute, rights_attribution,
                        attribution_holder, attribution_credit, license_url, created_at
                 FROM asset WHERE id = ?1",
                params![id.as_bytes().to_vec()],
                Self::row_to_asset,
            )
            .optional()
            .map_err(internal)?;
        let mut asset = asset.ok_or_else(|| LibError::NotFound(format!("asset {id}")))?;
        // Attach the cheap-tier media attributes from the per-type table (tech-spec 04 §5).
        asset.attributes = Self::load_media_attrs(&conn, id.as_bytes(), asset.summary.media);
        Ok(asset)
    }

    fn row_to_asset(r: &rusqlite::Row) -> rusqlite::Result<Asset> {
        let id = blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?);
        let hash: Option<Vec<u8>> = r.get(1)?;
        let source_id = blob_to_source_id(&r.get::<_, Vec<u8>>(2)?);
        let path: String = r.get(3)?;
        let filename: String = r.get(4)?;
        let size_bytes: Option<i64> = r.get(5)?;
        let source_created_at: Option<i64> = r.get(6)?;
        let source_modified_at: Option<i64> = r.get(7)?;
        let scanned_at: i64 = r.get(8)?;
        let analysed_at: Option<i64> = r.get(9)?;
        let media_s: String = r.get(10)?;
        let format: String = r.get(11)?;
        let license_id: Option<String> = r.get(12)?;
        let license_status: String = r.get(13)?;
        let license_provenance: String = r.get(14)?;
        let commercial: Option<i64> = r.get(15)?;
        let modify: Option<i64> = r.get(16)?;
        let redistribute: Option<i64> = r.get(17)?;
        let attribution: Option<i64> = r.get(18)?;
        let holder: Option<String> = r.get(19)?;
        let credit: Option<String> = r.get(20)?;
        let url: Option<String> = r.get(21)?;

        let media = MediaType::parse(&media_s).unwrap_or(MediaType::Image);
        let status = LicenseStatus::parse(&license_status);
        let summary = AssetSummary {
            id,
            name: filename,
            media,
            format,
            size: size_bytes.unwrap_or(0) as u64,
            license: LicenseBadge {
                id: license_id.clone(),
                status,
            },
            top_tags: Vec::new(),
            origin: Origin::Local,
            key_attrs: SmallMap::new(),
        };
        Ok(Asset {
            summary,
            hash: hash
                .and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok())
                .map(ContentHash),
            source_id,
            path,
            timestamps: AssetTimes {
                created: source_created_at,
                modified: source_modified_at,
                scanned: scanned_at,
                analyzed: analysed_at,
            },
            attributes: MediaAttributes::None,
            license: License {
                id: license_id,
                status,
                commercial: commercial.map(|v| v != 0),
                modify: modify.map(|v| v != 0),
                redistribute: redistribute.map(|v| v != 0),
                attribution: attribution.map(|v| v != 0),
                holder,
                credit,
                url,
                provenance: license_provenance,
            },
            tags: Vec::new(),
            collections: Vec::new(),
        })
    }

    /// Faceted query → a page of summaries. Cursor is an offset (slice-simple; keyset later).
    pub fn query_assets(&self, req: &QueryRequest) -> Result<Page<AssetSummary>, LibError> {
        let limit = req.page.clamped(QUERY_MAX_LIMIT);
        let offset = decode_offset(req.page.after.as_ref())?;

        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds: Vec<Value> = Vec::new();

        if let Some(text) = req.text.as_ref().filter(|t| !t.is_empty()) {
            where_sql.push_str(" AND filename LIKE ?");
            binds.push(Value::Text(format!("%{}%", escape_like(text))));
        }
        for f in &req.filters {
            apply_filter(f, &mut where_sql, &mut binds)?;
        }

        let order = match req.sort.field {
            SortField::Name | SortField::Relevance => "filename",
            SortField::Size => "size_bytes",
            SortField::Scanned => "scanned_at",
        };
        let dir = match req.sort.dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
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
                    image_attr.width, image_attr.height, audio_attr.duration_ms, model_attr.triangle_count
             FROM asset
             LEFT JOIN image_attr ON image_attr.asset_id = asset.id
             LEFT JOIN audio_attr ON audio_attr.asset_id = asset.id
             LEFT JOIN model_attr ON model_attr.asset_id = asset.id
             {where_sql} ORDER BY {order} {dir}, asset.id ASC LIMIT ? OFFSET ?"
        );
        let mut page_binds = binds.clone();
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
                    key_attrs: grid_key_attrs(media, width, height, duration_ms, tri_count),
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

    // ── jobs ───────────────────────────────────────────────────────────────

    pub fn create_job(
        &self,
        kind: JobKind,
        params_json: &str,
        total: Option<u64>,
    ) -> Result<JobId, LibError> {
        let id = JobId::new();
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO job (id, kind, state, params, progress, done, total, created_at, updated_at)
             VALUES (?1, ?2, 'queued', ?3, 0, 0, ?4, ?5, ?5)",
            params![
                id.as_bytes().to_vec(),
                job_kind_str(kind),
                params_json,
                total.map(|t| t as i64),
                now,
            ],
        )
        .map_err(internal)?;
        Ok(id)
    }

    pub fn update_job_progress(
        &self,
        id: &JobId,
        state: JobState,
        done: u64,
        total: Option<u64>,
        current: Option<&str>,
    ) -> Result<(), LibError> {
        let progress = match total {
            Some(t) if t > 0 => (done as f64 / t as f64).min(1.0),
            _ => 0.0,
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE job SET state = ?2, done = ?3, total = ?4, current = ?5, progress = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                id.as_bytes().to_vec(),
                job_state_str(state),
                done as i64,
                total.map(|t| t as i64),
                current,
                progress,
                now_ms(),
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    pub fn set_job_state(
        &self,
        id: &JobId,
        state: JobState,
        error: Option<&str>,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE job SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.as_bytes().to_vec(),
                job_state_str(state),
                error,
                now_ms(),
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    pub fn get_job(&self, id: &JobId) -> Result<JobStatus, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, kind, state, done, total, current, error FROM job WHERE id = ?1",
            params![id.as_bytes().to_vec()],
            Self::row_to_job,
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| LibError::NotFound(format!("job {id}")))
    }

    pub fn list_jobs(&self, req: &JobListRequest) -> Result<Page<JobStatus>, LibError> {
        let limit = req.page.clamped(QUERY_MAX_LIMIT);
        let offset = decode_offset(req.page.after.as_ref())?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, state, done, total, current, error FROM job
                 ORDER BY created_at DESC LIMIT ? OFFSET ?",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], Self::row_to_job)
            .map_err(internal)?;
        let mut items = Vec::new();
        for r in rows {
            let job = r.map_err(internal)?;
            let keep_kind = req.kinds.is_empty() || req.kinds.contains(&job.kind);
            let keep_state = req.state.map(|s| s == job.state).unwrap_or(true);
            if keep_kind && keep_state {
                items.push(job);
            }
        }
        let next = if items.len() == limit as usize {
            Some(Cursor((offset + items.len()).to_string()))
        } else {
            None
        };
        Ok(Page::new(items, next))
    }

    fn row_to_job(r: &rusqlite::Row) -> rusqlite::Result<JobStatus> {
        let id = blob_to_job_id(&r.get::<_, Vec<u8>>(0)?);
        let kind_s: String = r.get(1)?;
        let state_s: String = r.get(2)?;
        let done: i64 = r.get(3)?;
        let total: Option<i64> = r.get(4)?;
        let current: Option<String> = r.get(5)?;
        let error: Option<String> = r.get(6)?;
        Ok(JobStatus {
            id,
            kind: parse_job_kind(&kind_s),
            state: parse_job_state(&state_s),
            progress: Progress {
                done: done as u64,
                total: total.map(|t| t as u64),
                current,
            },
            error,
        })
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// A tiny map of display attributes for a grid row (tech-spec 03 §4 `key_attrs`): dimensions for
/// images, duration for audio, triangle count for models. Cheap and best-effort.
fn grid_key_attrs(
    media: MediaType,
    width: Option<i64>,
    height: Option<i64>,
    duration_ms: Option<i64>,
    tri_count: Option<i64>,
) -> SmallMap {
    let mut m = SmallMap::new();
    match media {
        MediaType::Image => {
            if let (Some(w), Some(h)) = (width, height) {
                m.insert("dimensions".into(), format!("{w}×{h}"));
            }
        }
        MediaType::Audio => {
            if let Some(ms) = duration_ms {
                let secs = ms as f64 / 1000.0;
                m.insert(
                    "duration".into(),
                    format!("{:.0}:{:02}", (secs / 60.0).floor(), (secs % 60.0) as i64),
                );
            }
        }
        MediaType::Model => {
            if let Some(t) = tri_count {
                m.insert("tris".into(), t.to_string());
            }
        }
    }
    m
}

fn blob_to_asset_id(b: &[u8]) -> AssetId {
    AssetId(uuid_from_slice(b))
}
fn blob_to_source_id(b: &[u8]) -> SourceId {
    SourceId(uuid_from_slice(b))
}
fn blob_to_job_id(b: &[u8]) -> JobId {
    JobId(uuid_from_slice(b))
}
fn uuid_from_slice(b: &[u8]) -> Uuid {
    <[u8; 16]>::try_from(b)
        .map(Uuid::from_bytes)
        .unwrap_or(Uuid::nil())
}

fn decode_offset(c: Option<&Cursor>) -> Result<usize, LibError> {
    match c {
        None => Ok(0),
        Some(Cursor(s)) => s
            .parse::<usize>()
            .map_err(|_| LibError::BadRequest("invalid cursor".into())),
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn apply_filter(
    f: &Filter,
    where_sql: &mut String,
    binds: &mut Vec<Value>,
) -> Result<(), LibError> {
    use FacetField::*;
    match f.field {
        MediaType => {
            eq_or_in(f, "media_type", where_sql, binds)?;
        }
        Format => {
            eq_or_in(f, "format", where_sql, binds)?;
        }
        Source => match &f.value {
            FilterValue::Str(s) => {
                let id = s
                    .parse::<SourceId>()
                    .map_err(|_| LibError::BadRequest("invalid source id".into()))?;
                where_sql.push_str(" AND source_id = ?");
                binds.push(Value::Blob(id.as_bytes().to_vec()));
            }
            _ => {
                return Err(LibError::BadRequest(
                    "source filter wants a string id".into(),
                ))
            }
        },
        SizeBytes => {
            let col = "size_bytes";
            match (&f.op, &f.value) {
                (FilterOp::Gt, FilterValue::Num(n)) => push_cmp(where_sql, binds, col, ">", *n),
                (FilterOp::Gte, FilterValue::Num(n)) => push_cmp(where_sql, binds, col, ">=", *n),
                (FilterOp::Lt, FilterValue::Num(n)) => push_cmp(where_sql, binds, col, "<", *n),
                (FilterOp::Lte, FilterValue::Num(n)) => push_cmp(where_sql, binds, col, "<=", *n),
                (FilterOp::Range, FilterValue::Range(lo, hi)) => {
                    where_sql.push_str(" AND size_bytes BETWEEN ? AND ?");
                    binds.push(Value::Integer(*lo as i64));
                    binds.push(Value::Integer(*hi as i64));
                }
                _ => return Err(LibError::BadRequest("unsupported size filter".into())),
            }
        }
        other => {
            return Err(LibError::Unsupported(format!(
                "filter on {other:?} is not implemented in this build"
            )));
        }
    }
    Ok(())
}

fn eq_or_in(
    f: &Filter,
    col: &str,
    where_sql: &mut String,
    binds: &mut Vec<Value>,
) -> Result<(), LibError> {
    match (&f.op, &f.value) {
        (FilterOp::Eq, FilterValue::Str(s)) => {
            where_sql.push_str(&format!(" AND {col} = ?"));
            binds.push(Value::Text(s.clone()));
        }
        (FilterOp::In, FilterValue::List(items)) => {
            let placeholders = items.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            where_sql.push_str(&format!(" AND {col} IN ({placeholders})"));
            for it in items {
                if let FilterValue::Str(s) = it {
                    binds.push(Value::Text(s.clone()));
                } else {
                    return Err(LibError::BadRequest("IN list wants strings".into()));
                }
            }
        }
        _ => return Err(LibError::BadRequest(format!("unsupported op for {col}"))),
    }
    Ok(())
}

fn push_cmp(where_sql: &mut String, binds: &mut Vec<Value>, col: &str, op: &str, n: f64) {
    where_sql.push_str(&format!(" AND {col} {op} ?"));
    binds.push(Value::Integer(n as i64));
}

fn job_kind_str(k: JobKind) -> &'static str {
    match k {
        JobKind::Scan => "scan",
        JobKind::Analyze => "analyse",
        JobKind::Convert => "convert",
        JobKind::Export => "export",
    }
}
fn parse_job_kind(s: &str) -> JobKind {
    match s {
        "analyse" => JobKind::Analyze,
        "convert" => JobKind::Convert,
        "export" => JobKind::Export,
        _ => JobKind::Scan,
    }
}
fn job_state_str(s: JobState) -> &'static str {
    match s {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Paused => "paused",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}
fn parse_job_state(s: &str) -> JobState {
    match s {
        "running" => JobState::Running,
        "paused" => JobState::Paused,
        "done" => JobState::Done,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        _ => JobState::Queued,
    }
}
