//! `dam-store` — the SQLite metadata store (tech-spec 02). Private to `3dam-core`; no front-end
//! links it (dependency rule 4, tech-spec 01 §2). Methods are synchronous and internally locked;
//! `3dam-core` calls them from a blocking context off the async runtime (tech-spec 14).

mod schema;

use dam_api::dto::*;
use dam_api::id::{AssetId, CollectionId, ContentHash, JobId, SourceId};
use dam_api::page::{Cursor, Page};
use dam_api::LibError;
use dam_sources::SourceConnection;
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

/// A source's `path -> (size_bytes, source_modified_at)` change-token index for delta re-scan.
pub type PathIndex = std::collections::HashMap<String, (Option<i64>, Option<i64>)>;

/// Deserialize a persisted `source.connection` blob into the typed connection model.
fn parse_connection(blob: &str) -> Result<SourceConnection, LibError> {
    serde_json::from_str(blob)
        .map_err(|e| LibError::Internal(format!("corrupt source connection: {e}")))
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

/// One asset the analysis pass must (re-)process — enough to locate the file and decode it, plus the
/// content hash that keys the extractor cache (tech-spec 05 §7.1). Produced by [`Store::list_analysis_targets`].
pub struct AnalysisTarget {
    pub id: AssetId,
    /// Absolute source root the relative `path` joins onto.
    pub source_uri: String,
    pub path: String,
    pub media: MediaType,
    pub format: String,
    pub content_hash: Option<ContentHash>,
}

/// Derived image signals the analysis pass persists (tech-spec 05 §5, §6). Passed as primitives so the
/// store never depends on `dam-media`.
pub struct ImageAnalysis {
    pub phash: u64,
    pub tileability: f32,
    pub repeat_period: Option<i64>,
    pub tile_class: String,
    pub dominant_colors: Vec<String>,
    pub class: String,
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
                    updated_at = ?9, flags = flags & -2 WHERE id = ?1",
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
                    "SELECT duration_ms, sample_rate, bit_depth, channels, codec, container, class FROM audio_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        Ok(AudioAttributes {
                            duration_ms: r.get(0)?,
                            sample_rate: r.get(1)?,
                            bit_depth: r.get(2)?,
                            channels: r.get(3)?,
                            codec: r.get(4)?,
                            container: r.get(5)?,
                            class: r.get(6)?,
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
                    "SELECT width, height, color_depth, has_alpha, color_space,
                            phash, tileability, repeat_period, tile_class, dominant_colors, class
                     FROM image_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        let phash: Option<Vec<u8>> = r.get(5)?;
                        let dominant: Option<String> = r.get(9)?;
                        Ok(ImageAttributes {
                            width: r.get(0)?,
                            height: r.get(1)?,
                            color_depth: r.get(2)?,
                            has_alpha: r.get::<_, Option<i64>>(3)?.map(|v| v != 0),
                            color_space: r.get(4)?,
                            phash: phash.and_then(|b| <[u8; 8]>::try_from(b.as_slice()).ok())
                                .map(|b| format!("{:016x}", u64::from_le_bytes(b))),
                            tileability: r.get::<_, Option<f64>>(6)?.map(|v| v as f32),
                            repeat_period: r.get(7)?,
                            tile_class: r.get(8)?,
                            dominant_colors: dominant
                                .and_then(|s| serde_json::from_str(&s).ok())
                                .unwrap_or_default(),
                            class: r.get(10)?,
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
                            has_rig, has_animation, has_uv, class FROM model_attr WHERE asset_id = ?1",
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
                            class: r.get(8)?,
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
        // Attach tags (suggested + confirmed + rejected) and surface confirmed ones on the summary.
        asset.tags = Self::load_tags(&conn, id.as_bytes());
        asset.summary.top_tags = asset
            .tags
            .iter()
            .filter(|t| t.state == "confirmed")
            .map(|t| t.name.clone())
            .collect();
        // Attach the manual collections this asset belongs to (inspector membership, §6.4).
        drop(conn);
        asset.collections = self.collections_for_asset(id)?;
        Ok(asset)
    }

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
        let id = CollectionId::from_bytes(<[u8; 16]>::try_from(r.get::<_, Vec<u8>>(0)?.as_slice()).unwrap_or([0; 16]));
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
        let mut stmt = conn
            .prepare(
                "SELECT asset.id, filename, media_type, format, size_bytes, license_id, license_status,
                        image_attr.width, image_attr.height, audio_attr.duration_ms, model_attr.triangle_count
                 FROM collection_member cm
                 JOIN asset ON asset.id = cm.asset_id
                 LEFT JOIN image_attr ON image_attr.asset_id = asset.id
                 LEFT JOIN audio_attr ON audio_attr.asset_id = asset.id
                 LEFT JOIN model_attr ON model_attr.asset_id = asset.id
                 WHERE cm.collection_id = ?1
                 ORDER BY cm.added_at DESC, asset.id ASC LIMIT ?2",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(
                params![id.as_bytes().to_vec(), limit.min(QUERY_MAX_LIMIT) as i64],
                Self::row_to_summary,
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

    /// Shared row → `AssetSummary` mapper for the grid SELECT shape.
    fn row_to_summary(r: &rusqlite::Row) -> rusqlite::Result<AssetSummary> {
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

    /// Every asset id matching a query's text + filters, ordered by name — the unbounded id set an
    /// export or smart-folder resolution walks (no pagination). Ignores `page`/`sort`/`facets`.
    pub fn query_asset_ids(&self, req: &QueryRequest) -> Result<Vec<AssetId>, LibError> {
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds: Vec<Value> = Vec::new();
        if let Some(text) = req.text.as_ref().filter(|t| !t.is_empty()) {
            where_sql.push_str(" AND filename LIKE ?");
            binds.push(Value::Text(format!("%{}%", escape_like(text))));
        }
        for f in &req.filters {
            apply_filter(f, &mut where_sql, &mut binds)?;
        }
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

    // ── analysis / automation (tech-spec 05, phase 3) ───────────────────────

    /// The assets an analysis pass should process: everything behind `current_version` (the incremental
    /// Plan gate, §1.2/§7.2), or `force`-all, or a specific `ids` set. Joins the source so the runner can
    /// resolve each file. Skips offline/federated sources (no bytes to decode).
    pub fn list_analysis_targets(
        &self,
        current_version: i64,
        force: bool,
        ids: &[AssetId],
    ) -> Result<Vec<AnalysisTarget>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT a.id, s.connection, a.path, a.media_type, a.format, a.content_hash
             FROM asset a JOIN source s ON s.id = a.source_id
             WHERE s.kind = 'local_fs'",
        );
        if !force {
            sql.push_str(&format!(" AND a.analysis_version < {current_version}"));
        }
        let mut binds: Vec<Value> = Vec::new();
        if !ids.is_empty() {
            let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND a.id IN ({ph})"));
            for id in ids {
                binds.push(Value::Blob(id.as_bytes().to_vec()));
            }
        }
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                let id = blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?);
                let connection: String = r.get(1)?;
                let path: String = r.get(2)?;
                let media_s: String = r.get(3)?;
                let format: String = r.get(4)?;
                let hash: Option<Vec<u8>> = r.get(5)?;
                // local_fs display URI is the (canonical) source root the analyzer joins onto.
                let source_uri = parse_connection(&connection)
                    .map(|c| c.display_uri())
                    .unwrap_or_default();
                Ok(AnalysisTarget {
                    id,
                    source_uri,
                    path,
                    media: MediaType::parse(&media_s).unwrap_or(MediaType::Image),
                    format,
                    content_hash: hash
                        .and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok())
                        .map(ContentHash),
                })
            })
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(internal)?);
        }
        Ok(out)
    }

    /// Persist the derived image signals (§5, §6) into the existing `image_attr` row. The row is created
    /// at scan (cheap tier), so this is an UPDATE; if absent (e.g. a directly-analysed asset), upsert.
    pub fn set_image_analysis(&self, id: &AssetId, a: &ImageAnalysis) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let key = id.as_bytes().to_vec();
        let phash_blob = a.phash.to_le_bytes().to_vec();
        let colors = serde_json::to_string(&a.dominant_colors).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT INTO image_attr (asset_id, phash, tileability, repeat_period, tile_class, dominant_colors, class)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(asset_id) DO UPDATE SET
                phash=excluded.phash, tileability=excluded.tileability,
                repeat_period=excluded.repeat_period, tile_class=excluded.tile_class,
                dominant_colors=excluded.dominant_colors, class=excluded.class",
            params![key, phash_blob, a.tileability as f64, a.repeat_period, a.tile_class, colors, a.class],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Persist an auto-category guess onto an audio/model attr row (§4, §5).
    pub fn set_media_class(&self, id: &AssetId, media: MediaType, class: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let key = id.as_bytes().to_vec();
        // Column set is identical across the three attr tables; pick the table for the media type.
        let sql = match media {
            MediaType::Audio => "INSERT INTO audio_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Model => "INSERT INTO model_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Image => "INSERT INTO image_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
        };
        conn.execute(sql, params![key, class]).map_err(internal)?;
        Ok(())
    }

    /// Upsert an asset's embedding for one space (§2.1, §3.1). `vec` must already be L2-normalised.
    pub fn set_embedding(
        &self,
        id: &AssetId,
        space_id: &str,
        media: MediaType,
        vec: &[f32],
        extractor: &str,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let mut bytes = Vec::with_capacity(vec.len() * 4);
        for f in vec {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        conn.execute(
            "INSERT INTO embedding (asset_id, space_id, media_type, dim, vec, extractor, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(asset_id, space_id) DO UPDATE SET
                dim=excluded.dim, vec=excluded.vec, extractor=excluded.extractor, created_at=excluded.created_at",
            params![
                id.as_bytes().to_vec(),
                space_id,
                media.as_str(),
                vec.len() as i64,
                bytes,
                extractor,
                now_ms(),
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Record that an asset is now analysed at `version` (the Plan gate reads this, §7.2).
    pub fn mark_analysed(&self, id: &AssetId, version: i64) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE asset SET analysis_version = ?2, analysed_at = ?3, updated_at = ?3 WHERE id = ?1",
            params![id.as_bytes().to_vec(), version, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    // ── tags / suggestions (§1.4) ─────────────────────────────────────────────

    fn load_tags(conn: &Connection, id_blob: &[u8]) -> Vec<TagRef> {
        let mut stmt = match conn.prepare(
            "SELECT t.name, at.state, at.source, at.confidence
             FROM asset_tag at JOIN tag t ON t.id = at.tag_id
             WHERE at.asset_id = ?1 ORDER BY at.state, t.name",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![id_blob], |r| {
            Ok(TagRef {
                name: r.get(0)?,
                state: r.get(1)?,
                source: r.get(2)?,
                confidence: r.get::<_, Option<f64>>(3)?.map(|v| v as f32),
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Intern a tag name, returning its id (case-insensitive unique).
    fn intern_tag(conn: &Connection, name: &str) -> Result<Vec<u8>, LibError> {
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
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let tag_id = Self::intern_tag(&conn, name)?;
        conn.execute(
            "INSERT INTO asset_tag (asset_id, tag_id, state, source, confidence, extractor, created_at)
             VALUES (?1, ?2, 'suggested', 'auto', ?3, ?4, ?5)
             ON CONFLICT(asset_id, tag_id) DO NOTHING",
            params![id.as_bytes().to_vec(), tag_id, confidence as f64, extractor, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Accept (`confirmed`) or reject (`rejected`) a suggested tag by name (§1.4). Reversible.
    pub fn set_tag_state(&self, id: &AssetId, name: &str, state: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let tag_id = Self::intern_tag(&conn, name)?;
        let n = conn
            .execute(
                "UPDATE asset_tag SET state = ?3 WHERE asset_id = ?1 AND tag_id = ?2",
                params![id.as_bytes().to_vec(), tag_id, state],
            )
            .map_err(internal)?;
        if n == 0 {
            // No prior suggestion (e.g. a user confirming a tag directly): create it as user-sourced.
            conn.execute(
                "INSERT INTO asset_tag (asset_id, tag_id, state, source, created_at)
                 VALUES (?1, ?2, ?3, 'user', ?4)
                 ON CONFLICT(asset_id, tag_id) DO UPDATE SET state = excluded.state",
                params![id.as_bytes().to_vec(), tag_id, state, now_ms()],
            )
            .map_err(internal)?;
        }
        Ok(())
    }

    // ── similarity + dedup (§3, §4) ───────────────────────────────────────────

    /// Cosine-nearest neighbours of `id` within its media's embedding space (§3.2). Brute-force exact
    /// scan over the space (v1; HNSW is the scale follow-up, §3.1). Facet `filters` are post-applied
    /// (§3.3). Returns `(summary, score)` sorted by descending cosine, self dropped, capped at `k`.
    pub fn similar(
        &self,
        id: &AssetId,
        k: u32,
        filters: &[Filter],
    ) -> Result<Vec<(AssetSummary, f32)>, LibError> {
        let conn = self.conn.lock().unwrap();
        // Query vector + its space.
        let query: Option<(String, Vec<u8>)> = conn
            .query_row(
                "SELECT space_id, vec FROM embedding WHERE asset_id = ?1",
                params![id.as_bytes().to_vec()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        let Some((space_id, qbytes)) = query else {
            return Ok(Vec::new()); // not embedded yet (§1.3)
        };
        let qvec = bytes_to_f32(&qbytes);

        // Score every other vector in the same space.
        let mut stmt = conn
            .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
            .map_err(internal)?;
        let self_blob = id.as_bytes().to_vec();
        let rows = stmt
            .query_map(params![space_id], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(internal)?;
        let mut scored: Vec<(AssetId, f32)> = Vec::new();
        for r in rows {
            let (id_blob, vbytes) = r.map_err(internal)?;
            if id_blob == self_blob {
                continue;
            }
            let score = cosine(&qvec, &bytes_to_f32(&vbytes));
            scored.push((blob_to_asset_id(&id_blob), score));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Over-fetch, then post-filter against the facet predicate and fetch summaries (§3.3).
        let overfetch = (k as usize * 4).max(k as usize + 16);
        let candidate_ids: Vec<AssetId> =
            scored.iter().take(overfetch).map(|(a, _)| *a).collect();
        let summaries = Self::summaries_for_ids(&conn, &candidate_ids, filters)?;
        let mut out = Vec::new();
        for (aid, score) in scored {
            if out.len() >= k as usize {
                break;
            }
            if let Some(sum) = summaries.get(&aid) {
                out.push((sum.clone(), score));
            }
        }
        Ok(out)
    }

    /// Duplicate groups for the review view (§4). `Exact` groups by content hash; `Near` groups by
    /// embedding cosine ≥ threshold within a media space (union-find over the pairwise relation, §4.3).
    pub fn duplicates(&self, req: &DupRequest) -> Result<Vec<DupGroup>, LibError> {
        const NEAR_COS: f32 = 0.92; // conservative "strong near-dup" band (§4.2; tuned later, §8)
        let conn = self.conn.lock().unwrap();
        let mut groups: Vec<DupGroup> = Vec::new();

        match req.kind {
            DupKind::Exact => {
                let mut media_pred = String::new();
                if let Some(m) = req.media {
                    media_pred = format!(" AND media_type = '{}'", m.as_str());
                }
                let sql = format!(
                    "SELECT lower(hex(content_hash)) h, group_concat(lower(hex(id))) ids, COUNT(*) n
                     FROM asset WHERE content_hash IS NOT NULL{media_pred}
                     GROUP BY content_hash HAVING n > 1 ORDER BY n DESC LIMIT {}",
                    req.limit
                );
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(1)?,)))
                    .map_err(internal)?;
                for r in rows {
                    let (ids_csv,) = r.map_err(internal)?;
                    let ids = parse_hex_ids(&ids_csv);
                    if let Some(g) = Self::build_dup_group(&conn, DupKind::Exact, &ids, "identical bytes (same content hash)")? {
                        groups.push(g);
                    }
                }
            }
            DupKind::Near => {
                // Load embeddings for the requested media (or all), union-find over cosine ≥ threshold.
                let mut sql = String::from(
                    "SELECT e.asset_id, e.vec FROM embedding e JOIN asset a ON a.id = e.asset_id",
                );
                if let Some(m) = req.media {
                    sql.push_str(&format!(" WHERE e.media_type = '{}'", m.as_str()));
                }
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)))
                    .map_err(internal)?;
                let mut ids: Vec<AssetId> = Vec::new();
                let mut vecs: Vec<Vec<f32>> = Vec::new();
                for r in rows {
                    let (id_blob, vbytes) = r.map_err(internal)?;
                    ids.push(blob_to_asset_id(&id_blob));
                    vecs.push(bytes_to_f32(&vbytes));
                }
                let mut uf = UnionFind::new(ids.len());
                for i in 0..vecs.len() {
                    for j in (i + 1)..vecs.len() {
                        if cosine(&vecs[i], &vecs[j]) >= NEAR_COS {
                            uf.union(i, j);
                        }
                    }
                }
                for comp in uf.components() {
                    if comp.len() < 2 {
                        continue;
                    }
                    if groups.len() >= req.limit as usize {
                        break;
                    }
                    let member_ids: Vec<AssetId> = comp.iter().map(|&i| ids[i]).collect();
                    if let Some(g) = Self::build_dup_group(
                        &conn,
                        DupKind::Near,
                        &member_ids,
                        &format!("embedding cosine ≥ {NEAR_COS:.2}"),
                    )? {
                        groups.push(g);
                    }
                }
            }
        }
        Ok(groups)
    }

    /// Build a `DupGroup` from member ids: load summaries, pick the suggested keep (largest bytes,
    /// then highest pixel count for images). Skips groups that collapse to <2 resolvable members.
    fn build_dup_group(
        conn: &Connection,
        kind: DupKind,
        ids: &[AssetId],
        signal: &str,
    ) -> Result<Option<DupGroup>, LibError> {
        let map = Self::summaries_for_ids(conn, ids, &[])?;
        let mut members: Vec<AssetSummary> = ids.iter().filter_map(|i| map.get(i).cloned()).collect();
        if members.len() < 2 {
            return Ok(None);
        }
        // Suggested keep: the biggest file (a decent proxy for highest fidelity, §4.3).
        members.sort_by_key(|b| std::cmp::Reverse(b.size));
        let suggested_keep = members[0].id;
        let media = members[0].media;
        Ok(Some(DupGroup {
            kind,
            media,
            members,
            signal: signal.to_string(),
            suggested_keep,
        }))
    }

    /// Fetch summaries for a set of ids, applying the same faceted filters as text search (§3.3).
    /// Returns a map so callers can preserve their own ordering (similarity score / dup grouping).
    fn summaries_for_ids(
        conn: &Connection,
        ids: &[AssetId],
        filters: &[Filter],
    ) -> Result<std::collections::HashMap<AssetId, AssetSummary>, LibError> {
        let mut map = std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds: Vec<Value> = Vec::new();
        for f in filters {
            apply_filter(f, &mut where_sql, &mut binds)?;
        }
        let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        where_sql.push_str(&format!(" AND asset.id IN ({ph})"));
        for id in ids {
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        let sql = format!(
            "SELECT asset.id, filename, media_type, format, size_bytes, license_id, license_status,
                    image_attr.width, image_attr.height, audio_attr.duration_ms, model_attr.triangle_count
             FROM asset
             LEFT JOIN image_attr ON image_attr.asset_id = asset.id
             LEFT JOIN audio_attr ON audio_attr.asset_id = asset.id
             LEFT JOIN model_attr ON model_attr.asset_id = asset.id
             {where_sql}"
        );
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
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
        for r in rows {
            let s = r.map_err(internal)?;
            map.insert(s.id, s);
        }
        Ok(map)
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

/// Decode a little-endian f32 blob (an embedding row's `vec`).
fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity. Vectors are stored L2-normalised, so this is a dot product; we still divide by
/// the norms defensively in case a legacy/zero vector slips in.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Parse a `group_concat(lower(hex(id)))` CSV of 32-hex-char UUIDs back into ids.
fn parse_hex_ids(csv: &str) -> Vec<AssetId> {
    csv.split(',')
        .filter_map(|h| {
            let bytes = (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            (bytes.len() == 16).then(|| blob_to_asset_id(&bytes))
        })
        .collect()
}

/// Tiny union-find for near-dup connected components (§4.3).
struct UnionFind {
    parent: Vec<usize>,
}
impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
    fn components(&mut self) -> Vec<Vec<usize>> {
        let mut map: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
        for i in 0..self.parent.len() {
            let root = self.find(i);
            map.entry(root).or_default().push(i);
        }
        map.into_values().collect()
    }
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
