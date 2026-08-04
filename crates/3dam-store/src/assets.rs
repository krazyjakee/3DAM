//! Asset upsert, per-media attributes, and full fetch — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    // ── assets ─────────────────────────────────────────────────────────────

    /// Insert a new asset or reconcile an existing `(source_id, path)` row (delta re-scan).
    /// Returns the id and whether it was newly inserted.
    pub fn upsert_asset(&self, a: &NewAsset) -> Result<(AssetId, bool), LibError> {
        let conn = self.write();
        // The previous hash and media type come back with the id: both decide whether the derived
        // layer this row already carries is still about the same file (see below).
        let existing: Option<(Vec<u8>, Option<Vec<u8>>, String)> = conn
            .query_row(
                "SELECT id, content_hash, media_type FROM asset WHERE source_id = ?1 AND path = ?2",
                params![a.source_id.as_bytes().to_vec(), a.path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(internal)?;
        let hash_blob = a.content_hash.map(|h| h.as_bytes().to_vec());
        let now = now_ms();
        let (id, is_new) = if let Some((id_blob, prev_hash, prev_media)) = existing {
            // Same path, different bytes: everything the analyse pass derived (embedding, class,
            // indexed document text) describes the *old* file. Only a Some→Some change counts —
            // a row that simply had no hash before is not evidence the file was edited, and
            // resetting unconditionally would re-analyse the whole library on every scan.
            let content_changed = matches!((&prev_hash, &hash_blob), (Some(p), Some(n)) if p != n);
            // Derivative paths are keyed by hash with an asset-id fallback, so even learning a
            // previously absent hash changes the cache key and must reopen V23's warm-up gate.
            let derivative_key_changed = prev_hash != hash_blob;
            // A reclassification (the content probe finding an audio-only `.mp4`, or ffprobe
            // becoming available between scans) is the same problem plus one: the derived rows are
            // in the wrong tables entirely.
            let media_changed = prev_media != a.media_type.as_str();
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
            if media_changed {
                // Drop the attr row(s) the old classification owned, so exactly one of the joined
                // tables stays non-NULL (`GRID_SELECT`'s invariant) — a video that turned out to be
                // audio-only must not keep reporting the video row's duration — and the embeddings,
                // which are all in the old media's space and can never be re-ranked against the new
                // one anyway. The FTS body text goes with them for the same reason.
                for media in MediaType::ALL.iter().filter(|m| **m != a.media_type) {
                    conn.execute(
                        &format!("DELETE FROM {} WHERE asset_id = ?1", attr_table(*media)),
                        params![id_blob],
                    )
                    .map_err(internal)?;
                }
                conn.execute(
                    "DELETE FROM embedding WHERE asset_id = ?1",
                    params![id_blob],
                )
                .map_err(internal)?;
                self.embed_gen
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                conn.execute(
                    "UPDATE asset_fts SET text = '' WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
                    params![id_blob],
                )
                .map_err(internal)?;
            }
            if content_changed || media_changed {
                // Re-open the analyse gate (`analysis_version < PIPELINE_VERSION`, §7.2). The scan
                // has already refreshed the cheap tier; without this the expensive tier would keep
                // the stale derivation forever, because the version alone still looks current.
                conn.execute(
                    "UPDATE asset SET analysis_version = 0, analysed_at = NULL WHERE id = ?1",
                    params![id_blob],
                )
                .map_err(internal)?;
            }
            if derivative_key_changed || media_changed {
                conn.execute(
                    "UPDATE asset SET derivative_version = 0 WHERE id = ?1",
                    params![id_blob],
                )
                .map_err(internal)?;
            }
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
        // created by the insert trigger with the raw filename — plus the tokenised directory
        // segments above it (issue #66), so the hierarchy an artist filed the asset under is
        // findable by name and not only by walking the tree. Both are pure functions of data the
        // row already carries, so this is the right place: a moved file is a new `(source_id, path)`
        // and comes back through here with its new folder. Best-effort: a token failure never sinks
        // the ingest.
        let tokens = crate::search::tokenize_name(&a.filename).join(" ");
        let folder = crate::search::folder_terms(&a.path);
        let _ = conn.execute(
            "UPDATE asset_fts SET tokens = ?2, folder = ?3
             WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
            params![id.as_bytes().to_vec(), tokens, folder],
        );
        Ok((id, is_new))
    }

    /// Persist the cheap-tier media attributes into the per-type attr table (tech-spec 02 §3.2,
    /// 04 §5). Idempotent upsert keyed by `asset_id`; called after each `upsert_asset` during a scan.
    /// Set or clear an asset's favourite mark (issue #63) — bit 1 of the `flags` bitset, left
    /// untouched by scan upserts (which only ever touch bit 0). A no-op on a missing id.
    pub fn set_favorite(&self, id: &AssetId, on: bool) -> Result<(), LibError> {
        let conn = self.write();
        let flag = crate::helpers::FAVORITE_FLAG;
        // `flags | flag` sets the bit; `flags & ~flag` clears just that bit, preserving the rest.
        let sql = if on {
            format!("UPDATE asset SET flags = flags | {flag}, updated_at = ?2 WHERE id = ?1")
        } else {
            format!("UPDATE asset SET flags = flags & ~{flag}, updated_at = ?2 WHERE id = ?1")
        };
        conn.execute(&sql, params![id.as_bytes().to_vec(), now_ms()])
            .map_err(internal)?;
        Ok(())
    }

    // ── notes (issue #81) ──────────────────────────────────────────────────

    /// The asset's free-text note, or `None` if it has never had one (or it was cleared).
    pub fn get_note(&self, id: &AssetId) -> Result<Option<Note>, LibError> {
        let conn = self.write();
        Self::read_note(&conn, id.as_bytes())
    }

    /// Set or clear an asset's note, returning the stored value (`None` once cleared).
    ///
    /// A blank body is a **clear**, not an empty note: the row is deleted so `asset_note` never
    /// accumulates rows that mean nothing, and the FTS `note` column is emptied in the same
    /// transaction so a cleared note stops matching immediately. A stale index entry here would be
    /// a real bug — the text has no other home to fall back on.
    ///
    /// Returns `NotFound` for an unknown asset rather than silently writing an orphan row: the FK
    /// would reject it anyway, and a caller deserves the specific error.
    pub fn set_note(
        &self,
        id: &AssetId,
        body: &str,
        by: Option<&str>,
    ) -> Result<Option<Note>, LibError> {
        let mut conn = self.write();
        let key = id.as_bytes().to_vec();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM asset WHERE id = ?1",
                params![key],
                |_| Ok(()),
            )
            .optional()
            .map_err(internal)?
            .is_some();
        if !exists {
            return Err(LibError::NotFound(format!("asset {id}")));
        }
        let body = body.trim();
        let now = now_ms();
        let tx = conn.transaction().map_err(internal)?;
        if body.is_empty() {
            tx.execute("DELETE FROM asset_note WHERE asset_id = ?1", params![key])
                .map_err(internal)?;
        } else {
            tx.execute(
                "INSERT INTO asset_note (asset_id, body, updated_at, updated_by)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(asset_id) DO UPDATE SET
                    body = excluded.body, updated_at = excluded.updated_at,
                    updated_by = excluded.updated_by",
                params![key, body, now, by],
            )
            .map_err(internal)?;
        }
        // Keep the index in lockstep from the write path. `asset_note` is a different table from
        // the one the FTS triggers watch, and the note text lives *only* in the index — the same
        // arrangement `analysis.rs` already uses for tags.
        tx.execute(
            "UPDATE asset_fts SET note = ?2 WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
            params![key, body],
        )
        .map_err(internal)?;
        // A note is user-authored catalog state; touching `updated_at` keeps "last changed" honest
        // for anything that sorts or syncs on it.
        tx.execute(
            "UPDATE asset SET updated_at = ?2 WHERE id = ?1",
            params![key, now],
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)?;
        Ok((!body.is_empty()).then(|| Note {
            body: body.to_string(),
            updated_at: now,
            updated_by: by.map(str::to_string),
        }))
    }

    fn read_note(conn: &Connection, key: &[u8]) -> Result<Option<Note>, LibError> {
        conn.query_row(
            "SELECT body, updated_at, updated_by FROM asset_note WHERE asset_id = ?1",
            params![key.to_vec()],
            |r| {
                Ok(Note {
                    body: r.get(0)?,
                    updated_at: r.get(1)?,
                    updated_by: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(internal)
    }

    // ── discussion threads (issue #82) ─────────────────────────────────────

    /// Every message on an asset, oldest first. Tombstones are included with an empty body — the
    /// thread has to stay coherent for anyone who replied to a since-deleted message.
    pub fn list_comments(&self, asset: &AssetId) -> Result<Vec<Comment>, LibError> {
        let conn = self.write();
        // UUIDv7 ids sort chronologically, so the primary key is the timeline.
        let mut stmt = conn
            .prepare(
                "SELECT comment_id, asset_id, author, body, created_at, edited_at, deleted_at,
                        reply_to
                 FROM asset_comment WHERE asset_id = ?1 ORDER BY comment_id ASC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![asset.as_bytes().to_vec()], row_to_comment)
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// One message, or `NotFound`. Used by the edit/delete guards, which need the author and the
    /// owning asset before they can decide anything.
    pub fn get_comment(&self, id: &CommentId) -> Result<Comment, LibError> {
        let conn = self.write();
        conn.query_row(
            "SELECT comment_id, asset_id, author, body, created_at, edited_at, deleted_at, reply_to
             FROM asset_comment WHERE comment_id = ?1",
            params![id.as_bytes().to_vec()],
            row_to_comment,
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| LibError::NotFound(format!("comment {id}")))
    }

    /// Append a message. `reply_to` is validated to belong to the same asset — a reply pointing at
    /// another asset's thread would render as a dangling quote and leak that a message exists.
    pub fn add_comment(
        &self,
        asset: &AssetId,
        author: &str,
        body: &str,
        reply_to: Option<CommentId>,
    ) -> Result<Comment, LibError> {
        let conn = self.write();
        let asset_blob = asset.as_bytes().to_vec();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM asset WHERE id = ?1",
                params![asset_blob],
                |_| Ok(()),
            )
            .optional()
            .map_err(internal)?
            .is_some();
        if !exists {
            return Err(LibError::NotFound(format!("asset {asset}")));
        }
        if let Some(parent) = reply_to {
            let same: bool = conn
                .query_row(
                    "SELECT 1 FROM asset_comment WHERE comment_id = ?1 AND asset_id = ?2",
                    params![parent.as_bytes().to_vec(), asset_blob],
                    |_| Ok(()),
                )
                .optional()
                .map_err(internal)?
                .is_some();
            if !same {
                return Err(LibError::BadRequest(
                    "reply_to must name a message on the same asset".into(),
                ));
            }
        }
        let id = CommentId::new();
        let now = now_ms();
        conn.execute(
            "INSERT INTO asset_comment (comment_id, asset_id, author, body, created_at, reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.as_bytes().to_vec(),
                asset_blob,
                author,
                body,
                now,
                reply_to.map(|r| r.as_bytes().to_vec()),
            ],
        )
        .map_err(internal)?;
        Ok(Comment {
            id,
            asset: *asset,
            author: CommentAuthor {
                id: author.to_string(),
                display: None,
            },
            body: body.to_string(),
            created_at: now,
            edited_at: None,
            deleted_at: None,
            reply_to,
        })
    }

    /// Replace a message's text and stamp `edited_at`. Refuses a tombstone: editing a deleted
    /// message would resurrect it without anyone having posted anything.
    pub fn edit_comment(&self, id: &CommentId, body: &str) -> Result<(), LibError> {
        let conn = self.write();
        let n = conn
            .execute(
                "UPDATE asset_comment SET body = ?2, edited_at = ?3
                 WHERE comment_id = ?1 AND deleted_at IS NULL",
                params![id.as_bytes().to_vec(), body, now_ms()],
            )
            .map_err(internal)?;
        if n == 0 {
            return Err(LibError::NotFound(format!("comment {id}")));
        }
        Ok(())
    }

    /// Soft-delete: blank the body, stamp `deleted_at`, keep the row so replies keep their parent.
    /// Idempotent — deleting an already-deleted message is a no-op, not an error.
    pub fn delete_comment(&self, id: &CommentId) -> Result<(), LibError> {
        let conn = self.write();
        conn.execute(
            "UPDATE asset_comment SET body = '', deleted_at = ?2
             WHERE comment_id = ?1 AND deleted_at IS NULL",
            params![id.as_bytes().to_vec(), now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    pub fn set_media_attrs(&self, id: &AssetId, attrs: &MediaAttributes) -> Result<(), LibError> {
        let conn = self.write();
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
                    "INSERT INTO image_attr (asset_id, width, height, color_depth, has_alpha,
                                             color_space, texture_format, mip_levels)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(asset_id) DO UPDATE SET
                        width=excluded.width, height=excluded.height, color_depth=excluded.color_depth,
                        has_alpha=excluded.has_alpha, color_space=excluded.color_space,
                        texture_format=excluded.texture_format, mip_levels=excluded.mip_levels",
                    params![
                        key,
                        i.width,
                        i.height,
                        i.color_depth,
                        i.has_alpha.map(|b| b as i64),
                        i.color_space,
                        i.texture_format,
                        i.mip_levels,
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
            MediaAttributes::Video(v) => {
                conn.execute(
                    "INSERT INTO video_attr (asset_id, duration_ms, width, height, fps, codec,
                        container, bitrate, has_audio)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(asset_id) DO UPDATE SET
                        duration_ms=excluded.duration_ms, width=excluded.width, height=excluded.height,
                        fps=excluded.fps, codec=excluded.codec, container=excluded.container,
                        bitrate=excluded.bitrate, has_audio=excluded.has_audio",
                    params![
                        key,
                        v.duration_ms,
                        v.width,
                        v.height,
                        v.fps.map(|f| f as f64),
                        v.codec,
                        v.container,
                        v.bitrate,
                        v.has_audio.map(|b| b as i64),
                    ],
                )
                .map_err(internal)?;
            }
            MediaAttributes::Document(d) => {
                conn.execute(
                    "INSERT INTO document_attr (asset_id, page_count, word_count, title, author,
                        encoding, excerpt)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(asset_id) DO UPDATE SET
                        page_count=excluded.page_count, word_count=excluded.word_count,
                        title=excluded.title, author=excluded.author, encoding=excluded.encoding,
                        excerpt=excluded.excerpt",
                    params![
                        key,
                        d.page_count,
                        d.word_count,
                        d.title,
                        d.author,
                        d.encoding,
                        d.excerpt,
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
                    "SELECT duration_ms, sample_rate, bit_depth, channels, codec, container, class,
                            loudness_lufs, brightness, harmonicity, waveform_peaks
                     FROM audio_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        let peaks_json: Option<String> = r.get(10)?;
                        Ok(AudioAttributes {
                            duration_ms: r.get(0)?,
                            sample_rate: r.get(1)?,
                            bit_depth: r.get(2)?,
                            channels: r.get(3)?,
                            codec: r.get(4)?,
                            container: r.get(5)?,
                            class: r.get(6)?,
                            loudness_lufs: r.get(7)?,
                            harmonicity: r.get(9)?,
                            brightness: r.get(8)?,
                            peaks: peaks_json.and_then(|j| serde_json::from_str(&j).ok()),
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
                            phash, tileability, repeat_period, tile_class, dominant_colors, class,
                            texture_format, mip_levels
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
                            texture_format: r.get(11)?,
                            mip_levels: r.get(12)?,
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
            MediaType::Video => conn
                .query_row(
                    "SELECT duration_ms, width, height, fps, codec, container, bitrate, has_audio, class
                     FROM video_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        Ok(VideoAttributes {
                            duration_ms: r.get(0)?,
                            width: r.get(1)?,
                            height: r.get(2)?,
                            fps: r.get::<_, Option<f64>>(3)?.map(|v| v as f32),
                            codec: r.get(4)?,
                            container: r.get(5)?,
                            bitrate: r.get(6)?,
                            has_audio: r.get::<_, Option<i64>>(7)?.map(|v| v != 0),
                            class: r.get(8)?,
                        })
                    },
                )
                .optional()
                .ok()
                .flatten()
                .map(MediaAttributes::Video)
                .unwrap_or(MediaAttributes::None),
            MediaType::Document => conn
                .query_row(
                    "SELECT page_count, word_count, title, author, encoding, excerpt, class
                     FROM document_attr WHERE asset_id = ?1",
                    params![id_blob],
                    |r| {
                        Ok(DocumentAttributes {
                            page_count: r.get(0)?,
                            word_count: r.get(1)?,
                            title: r.get(2)?,
                            author: r.get(3)?,
                            encoding: r.get(4)?,
                            excerpt: r.get(5)?,
                            class: r.get(6)?,
                        })
                    },
                )
                .optional()
                .ok()
                .flatten()
                .map(MediaAttributes::Document)
                .unwrap_or(MediaAttributes::None),
        }
    }

    /// Just the source an asset belongs to — a one-column point read, no attribute joins.
    ///
    /// Exists so an event emitter can attribute an `AssetChanged`/`AssetRemoved` without paying for
    /// a whole [`Self::get_asset`], and so a *removal* can capture the source **before** the row
    /// disappears (issue #42). `None` for an unknown id.
    pub fn asset_source(&self, id: &AssetId) -> Result<Option<SourceId>, LibError> {
        let conn = self.write();
        conn.query_row(
            "SELECT source_id FROM asset WHERE id = ?1",
            params![id.as_bytes().to_vec()],
            |r| Ok(blob_to_source_id(&r.get::<_, Vec<u8>>(0)?)),
        )
        .optional()
        .map_err(internal)
    }

    pub fn get_asset(&self, id: &AssetId) -> Result<Asset, LibError> {
        let conn = self.write();
        let asset = conn
            .query_row(
                "SELECT id, content_hash, source_id, path, filename, size_bytes,
                        source_created_at, source_modified_at, scanned_at, analysed_at,
                        media_type, format, license_id, license_status, license_provenance,
                        rights_commercial, rights_modify, rights_redistribute, rights_attribution,
                        attribution_holder, attribution_credit, license_url, created_at, flags
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
        // Attach the user's note (issue #81) — authored state, so it rides along with every read
        // rather than needing a second round-trip from the inspector.
        asset.note = Self::read_note(&conn, id.as_bytes())?;
        // Attach tags (suggested + confirmed + rejected) and surface confirmed ones on the summary.
        asset.tags = Self::load_tags(&conn, id.as_bytes());
        asset.summary.top_tags = asset
            .tags
            .iter()
            .filter(|t| t.state == SuggestionState::Confirmed)
            .map(|t| t.name.clone())
            .collect();
        Ok(asset)
    }

    /// The inspector form of [`Self::get_asset`]: the record plus the manual collections this asset
    /// belongs to (§6.4), **filtered by the caller's ceiling**. Split from `get_asset` so the
    /// engine's internal reads (convert, thumbnails, background grind) — which want the record, not
    /// the membership — never have to invent a visibility argument, and so the one client-facing
    /// path that does surface membership is forced to name a ceiling.
    pub fn get_asset_detail(&self, id: &AssetId, vis: &Visibility) -> Result<Asset, LibError> {
        let mut asset = self.get_asset(id)?;
        asset.collections = self.collections_for_asset(id, vis)?;
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
        let flags: i64 = r.get(23)?;

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
            favorite: flags & crate::helpers::FAVORITE_FLAG != 0,
            source_id: Some(source_id),
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
            note: None,
        })
    }
}

/// Row → [`Comment`] for the shared column order the queries above use. `display` is always `None`
/// here: resolving an account id to a name means reading `server.db`, which this crate cannot see.
fn row_to_comment(r: &rusqlite::Row) -> rusqlite::Result<Comment> {
    let id = CommentId(uuid_from_slice(&r.get::<_, Vec<u8>>(0)?));
    let asset = blob_to_asset_id(&r.get::<_, Vec<u8>>(1)?);
    let reply_to: Option<Vec<u8>> = r.get(7)?;
    Ok(Comment {
        id,
        asset,
        author: CommentAuthor {
            id: r.get(2)?,
            display: None,
        },
        body: r.get(3)?,
        created_at: r.get(4)?,
        edited_at: r.get(5)?,
        deleted_at: r.get(6)?,
        reply_to: reply_to.map(|b| CommentId(uuid_from_slice(&b))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_sources::SourceConnection;

    fn scanned(src: SourceId, path: &str, media: MediaType, hash: u8) -> NewAsset {
        NewAsset {
            source_id: src,
            path: path.to_string(),
            filename: path.rsplit('/').next().unwrap().to_string(),
            content_hash: Some(ContentHash([hash; 32])),
            size_bytes: Some(1),
            source_modified_at: None,
            scanned_at: now_ms(),
            media_type: media,
            format: "mp4".into(),
        }
    }

    fn store_and_source() -> (Store, SourceId) {
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        (store, src)
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.write().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// The analyse gate is `analysis_version < PIPELINE_VERSION`, and a re-scan of an edited file
    /// writes a new content hash but the *same* version — so without the reset the expensive tier
    /// would keep a derivation of bytes that no longer exist (a document's indexed body text being
    /// the visible case). Equally, resetting on every scan would re-analyse the whole library
    /// nightly, so only a genuine hash change counts.
    #[test]
    fn rescan_reopens_the_analyse_gate_only_when_the_bytes_changed() {
        let (store, src) = store_and_source();
        let (id, is_new) = store
            .upsert_asset(&scanned(src, "docs/spec.md", MediaType::Document, 1))
            .unwrap();
        assert!(is_new);
        store.mark_analysed(&id, 3).unwrap();
        let due = || store.list_analysis_targets(3, false, &[]).unwrap().len();
        assert_eq!(due(), 0, "just analysed");

        // An unchanged file re-scanned: same hash, nothing to redo.
        store
            .upsert_asset(&scanned(src, "docs/spec.md", MediaType::Document, 1))
            .unwrap();
        assert_eq!(due(), 0, "an unchanged rescan must not re-analyse");

        // Edited in place — same path, new bytes.
        store
            .upsert_asset(&scanned(src, "docs/spec.md", MediaType::Document, 2))
            .unwrap();
        assert_eq!(due(), 1, "an edited file must be re-analysed");
    }

    /// A media-type flip (the content probe finding an audio-only `.mp4`, or ffprobe appearing
    /// between scans) leaves derived rows in the wrong tables. `GRID_SELECT` COALESCEs duration
    /// across `audio_attr`/`video_attr` assuming exactly one is non-NULL, so a surviving row shows
    /// up as a stale duration on the wrong asset.
    #[test]
    fn reclassification_drops_the_previous_media_s_derived_rows() {
        let (store, src) = store_and_source();
        let (id, _) = store
            .upsert_asset(&scanned(src, "clip.mp4", MediaType::Video, 1))
            .unwrap();
        store
            .set_media_attrs(
                &id,
                &MediaAttributes::Video(VideoAttributes {
                    duration_ms: Some(1234),
                    ..Default::default()
                }),
            )
            .unwrap();
        store
            .set_embedding(&id, "video-stats-v1", MediaType::Video, &[1.0, 0.0], "t@1")
            .unwrap();
        store.mark_analysed(&id, 3).unwrap();

        // The probe settles it as audio-only on the next scan; the bytes are identical.
        store
            .upsert_asset(&scanned(src, "clip.mp4", MediaType::Audio, 1))
            .unwrap();

        assert_eq!(count(&store, "SELECT COUNT(*) FROM video_attr"), 0);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM embedding"),
            0,
            "the old vector is in a space the new media can never be ranked in"
        );
        assert_eq!(
            store.list_analysis_targets(3, false, &[]).unwrap().len(),
            1,
            "the new media type has to derive its own attributes"
        );
    }
}
