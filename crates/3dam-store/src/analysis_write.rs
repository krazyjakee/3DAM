//! Persisting what one analysis pass derived: the per-media signal rows, the embeddings, and the
//! version stamps that close an asset's analysis and derivative backlog entries. Part of the
//! `Store` impl.
//!
//! Every writer comes in a pair — a `self` method that takes the writer connection for a single
//! statement, and a `_in` twin on a caller-owned [`Connection`] so issue #138's analysis batch can
//! run the whole of one asset's derivation inside a single item savepoint.
use super::*;

impl Store {
    /// Advance one successfully generated content-keyed derivative slice.
    pub fn mark_derivative_ready(
        &self,
        id: &AssetId,
        derivative_version: i64,
        expected_hash: Option<ContentHash>,
    ) -> Result<bool, LibError> {
        let conn = self.write();
        conn.execute(
            "UPDATE asset SET derivative_version=?2 WHERE id=?1 AND content_hash IS ?3",
            params![
                id.as_bytes().to_vec(),
                derivative_version,
                expected_hash.map(|hash| hash.as_bytes().to_vec())
            ],
        )
        .map(|changed| changed == 1)
        .map_err(internal)
    }

    /// Reset explicitly purged derivative slices to pending.
    pub fn mark_derivatives_pending(&self, ids: &[AssetId]) -> Result<(), LibError> {
        let conn = self.write();
        let mut statement = conn
            .prepare("UPDATE asset SET derivative_version=0 WHERE id=?1")
            .map_err(internal)?;
        for id in ids {
            statement
                .execute(params![id.as_bytes().to_vec()])
                .map_err(internal)?;
        }
        Ok(())
    }

    /// Reset all local derivative slices after an operator clears a cache tier.
    pub fn mark_all_derivatives_pending(&self) -> Result<(), LibError> {
        let conn = self.write();
        conn.execute("UPDATE asset SET derivative_version=0", [])
            .map_err(internal)?;
        Ok(())
    }

    /// Persist the derived image signals (§5, §6) into the existing `image_attr` row. The row is created
    /// at scan (cheap tier), so this is an UPDATE; if absent (e.g. a directly-analysed asset), upsert.
    pub fn set_image_analysis(&self, id: &AssetId, a: &ImageAnalysis) -> Result<(), LibError> {
        let conn = self.write();
        Self::set_image_analysis_in(&conn, id, a)
    }

    /// The image-signal upsert on a caller-owned connection (issue #138's analysis batch runs it
    /// inside one item savepoint alongside the rest of that asset's derivation).
    pub(crate) fn set_image_analysis_in(
        conn: &Connection,
        id: &AssetId,
        a: &ImageAnalysis,
    ) -> Result<(), LibError> {
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
    pub fn set_media_class(
        &self,
        id: &AssetId,
        media: MediaType,
        class: &str,
    ) -> Result<(), LibError> {
        let conn = self.write();
        Self::set_media_class_in(&conn, id, media, class)
    }

    /// The class upsert on a caller-owned connection.
    pub(crate) fn set_media_class_in(
        conn: &Connection,
        id: &AssetId,
        media: MediaType,
        class: &str,
    ) -> Result<(), LibError> {
        let key = id.as_bytes().to_vec();
        // Every attr table carries the same `class` column; pick the table for the media type.
        let sql = match media {
            MediaType::Audio => "INSERT INTO audio_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Model => "INSERT INTO model_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Image => "INSERT INTO image_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Video => "INSERT INTO video_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
            MediaType::Document => "INSERT INTO document_attr (asset_id, class) VALUES (?1, ?2) ON CONFLICT(asset_id) DO UPDATE SET class=excluded.class",
        };
        conn.execute(sql, params![key, class]).map_err(internal)?;
        Ok(())
    }

    /// Persist the continuous audio acoustic features from the analyze pass (issue #61). Upserts the
    /// `audio_attr` row (the cheap-tier metadata may not have created it yet).
    pub fn set_audio_features(
        &self,
        id: &AssetId,
        loudness_lufs: f32,
        brightness: f32,
        harmonicity: f32,
    ) -> Result<(), LibError> {
        let conn = self.write();
        Self::set_audio_features_in(&conn, id, loudness_lufs, brightness, harmonicity)
    }

    /// The acoustic-feature upsert on a caller-owned connection.
    pub(crate) fn set_audio_features_in(
        conn: &Connection,
        id: &AssetId,
        loudness_lufs: f32,
        brightness: f32,
        harmonicity: f32,
    ) -> Result<(), LibError> {
        conn.execute(
            "INSERT INTO audio_attr (asset_id, loudness_lufs, brightness, harmonicity)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(asset_id) DO UPDATE SET
                loudness_lufs=excluded.loudness_lufs, brightness=excluded.brightness,
                harmonicity=excluded.harmonicity",
            params![
                id.as_bytes().to_vec(),
                loudness_lufs,
                brightness,
                harmonicity
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Persist the server-computed inspector waveform peaks (issue #73) as a JSON `[f32,…]` string, so
    /// both clients draw the bars without decoding the audio. Upsert (the cheap-tier row exists from
    /// scan); a no-op-safe part of the analysis pass.
    pub fn set_audio_peaks(&self, id: &AssetId, peaks: &[f32]) -> Result<(), LibError> {
        let conn = self.write();
        Self::set_audio_peaks_in(&conn, id, peaks)
    }

    /// The waveform-peak upsert on a caller-owned connection.
    pub(crate) fn set_audio_peaks_in(
        conn: &Connection,
        id: &AssetId,
        peaks: &[f32],
    ) -> Result<(), LibError> {
        let json = serde_json::to_string(peaks).map_err(internal)?;
        conn.execute(
            "INSERT INTO audio_attr (asset_id, waveform_peaks) VALUES (?1, ?2)
             ON CONFLICT(asset_id) DO UPDATE SET waveform_peaks=excluded.waveform_peaks",
            params![id.as_bytes().to_vec(), json],
        )
        .map_err(internal)?;
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
        let conn = self.write();
        Self::set_embedding_in(&conn, id, space_id, media, vec, extractor)?;
        self.wake_ann(true);
        Ok(())
    }

    /// The vector upsert on a caller-owned connection. Like [`Self::upsert_asset_in`] it leaves
    /// lifecycle alone: the batch wakes it once, after its transaction has actually committed.
    pub(crate) fn set_embedding_in(
        conn: &Connection,
        id: &AssetId,
        space_id: &str,
        media: MediaType,
        vec: &[f32],
        extractor: &str,
    ) -> Result<(), LibError> {
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

    /// Drop an asset's embedding in one space. The counterpart to [`Self::set_embedding`] for the
    /// case where re-analysis produces *no* vector (a document whose new revision has no readable
    /// text): leaving the previous one indexed would keep ranking the asset on content it no longer
    /// has. A no-op when there was nothing there.
    pub fn clear_embedding(&self, id: &AssetId, space_id: &str) -> Result<(), LibError> {
        let conn = self.write();
        let changed = Self::clear_embedding_in(&conn, id, space_id)?;
        self.wake_ann(changed);
        Ok(())
    }

    /// The delete on a caller-owned connection; `true` when a vector actually went away, which is
    /// the caller's cue to wake the lifecycle once its transaction commits.
    pub(crate) fn clear_embedding_in(
        conn: &Connection,
        id: &AssetId,
        space_id: &str,
    ) -> Result<bool, LibError> {
        let n = conn
            .execute(
                "DELETE FROM embedding WHERE asset_id = ?1 AND space_id = ?2",
                params![id.as_bytes().to_vec(), space_id],
            )
            .map_err(internal)?;
        Ok(n > 0)
    }

    /// Record that an asset is now analysed at `version` (the Plan gate reads this, §7.2).
    pub fn mark_analysed(&self, id: &AssetId, version: i64) -> Result<(), LibError> {
        let conn = self.write();
        Self::mark_analysed_in(&conn, id, version)
    }

    /// The version stamp on a caller-owned connection — the last statement of an analysis batch
    /// item, so that a failure anywhere above it leaves the asset legitimately due for re-analysis.
    pub(crate) fn mark_analysed_in(
        conn: &Connection,
        id: &AssetId,
        version: i64,
    ) -> Result<(), LibError> {
        conn.execute(
            "UPDATE asset SET analysis_version = ?2, analysed_at = ?3, updated_at = ?3 WHERE id = ?1",
            params![id.as_bytes().to_vec(), version, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }
}
