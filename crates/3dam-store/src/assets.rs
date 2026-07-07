//! Asset upsert, per-media attributes, and full fetch — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
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
        let (id, is_new) = if let Some(id_blob) = existing {
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
            (blob_to_asset_id(&id_blob), false)
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
            (id, true)
        };
        // Enrich the FTS row with filename-derived tokens (M2) so an embedded term like the `ak47`
        // in `ak47_lowpoly.fbx` is searchable immediately at scan — the `asset_fts` row itself was
        // created by the insert trigger with the raw filename. Best-effort: a token failure never
        // sinks the ingest.
        let tokens = crate::search::tokenize_name(&a.filename).join(" ");
        let _ = conn.execute(
            "UPDATE asset_fts SET tokens = ?2 WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
            params![id.as_bytes().to_vec(), tokens],
        );
        Ok((id, is_new))
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
                        material_count, texture_count, dependency_bytes, has_rig, has_animation, has_uv)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     ON CONFLICT(asset_id) DO UPDATE SET
                        vertex_count=excluded.vertex_count, triangle_count=excluded.triangle_count,
                        mesh_count=excluded.mesh_count, material_count=excluded.material_count,
                        texture_count=excluded.texture_count, dependency_bytes=excluded.dependency_bytes,
                        has_rig=excluded.has_rig,
                        has_animation=excluded.has_animation, has_uv=excluded.has_uv",
                    params![
                        key,
                        m.vertex_count,
                        m.triangle_count,
                        m.mesh_count,
                        m.material_count,
                        m.texture_count,
                        m.dependency_bytes,
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
                            dependency_bytes, has_rig, has_animation, has_uv, class
                     FROM model_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        Ok(ModelAttributes {
                            vertex_count: r.get(0)?,
                            triangle_count: r.get(1)?,
                            mesh_count: r.get(2)?,
                            material_count: r.get(3)?,
                            texture_count: r.get(4)?,
                            dependency_bytes: r.get(5)?,
                            has_rig: r.get::<_, Option<i64>>(6)?.map(|v| v != 0),
                            has_animation: r.get::<_, Option<i64>>(7)?.map(|v| v != 0),
                            has_uvs: r.get::<_, Option<i64>>(8)?.map(|v| v != 0),
                            class: r.get(9)?,
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
        // `row_to_asset` sets `size` to the mesh container alone; fold in a model's external
        // companion files so the inspector shows the whole-asset size the grid also reports.
        if let MediaAttributes::Model(m) = &asset.attributes {
            asset.summary.size += m.dependency_bytes.unwrap_or(0).max(0) as u64;
        }
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
}
