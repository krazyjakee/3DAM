//! Analysis targets, tags/suggestions, similarity and dedup — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;
use std::collections::BTreeMap;

/// Planner pages stay small enough for flat memory and prompt cancellation while amortizing the
/// SQLite query and producer/consumer hand-off.
pub const ANALYSIS_PLAN_BATCH_MAX: usize = 512;
const NEAR_DUP_COSINE: f32 = 0.92;
const NEAR_DUP_CANDIDATE_MAX: usize = 2_000;
type NearCandidate = (AssetId, String, String, Vec<f32>);
type NearComponent = (String, String, Vec<AssetId>);
type NearPartitions = BTreeMap<(String, String, usize), Vec<(AssetId, Vec<f32>)>>;

struct DupGroupSeed<'a> {
    kind: DupKind,
    ids: &'a [AssetId],
    total_members: u32,
    group: Option<String>,
    members_cursor: Option<Cursor>,
    signal: &'a str,
    review: &'a str,
}

#[derive(Clone, Copy)]
enum PlanKind {
    Analysis { current_version: i64, force: bool },
    Derivative { current_version: i64 },
}

impl PlanKind {
    fn due_column(self) -> &'static str {
        match self {
            Self::Analysis { .. } => "a.analysis_version",
            Self::Derivative { .. } => "a.derivative_version",
        }
    }

    fn orders_by_version(self) -> bool {
        !matches!(self, Self::Analysis { force: true, .. })
    }

    fn order_columns(self, direction: &str) -> String {
        if self.orders_by_version() {
            format!(
                "{} {direction},a.source_id {direction},a.id {direction}",
                self.due_column()
            )
        } else {
            format!("a.source_id {direction},a.id {direction}")
        }
    }

    fn planner_index(self) -> &'static str {
        match self {
            Self::Analysis { force: true, .. } => "idx_asset_force_planner",
            Self::Analysis { force: false, .. } => "idx_asset_analysis_planner",
            Self::Derivative { .. } => "idx_asset_derivative_pending",
        }
    }
}

struct PlanRow {
    target: AnalysisPlanTarget,
    due_version: i64,
}

fn plan_where(kind: PlanKind, ids: &[AssetId]) -> (String, Vec<Value>) {
    let mut sql = String::from("WHERE s.kind <> 'federated'");
    match kind {
        PlanKind::Analysis {
            current_version,
            force,
        } => {
            if !force {
                sql.push_str(&format!(" AND a.analysis_version < {current_version}"));
            }
        }
        PlanKind::Derivative { current_version } => {
            sql.push_str(&format!(" AND a.derivative_version < {current_version}"));
            sql.push_str(" AND a.media_type IN ('image','video','model')");
        }
    }
    let mut binds = Vec::new();
    if !ids.is_empty() {
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND a.id IN ({placeholders})"));
        binds.extend(ids.iter().map(|id| Value::Blob(id.as_bytes().to_vec())));
    }
    (sql, binds)
}

fn push_plan_cursor(
    sql: &mut String,
    binds: &mut Vec<Value>,
    kind: PlanKind,
    cursor: AnalysisPlanCursor,
    inclusive_end: bool,
) {
    let comparison = if inclusive_end { "<=" } else { ">" };
    if kind.orders_by_version() {
        sql.push_str(&format!(
            " AND ({},a.source_id,a.id) {comparison} (?,?,?)",
            kind.due_column()
        ));
        binds.push(Value::Integer(cursor.due_version));
    } else {
        sql.push_str(&format!(" AND (a.source_id,a.id) {comparison} (?,?)"));
    }
    binds.push(Value::Blob(cursor.source_id.as_bytes().to_vec()));
    binds.push(Value::Blob(cursor.asset_id.as_bytes().to_vec()));
}

impl Store {
    // ── analysis / automation (tech-spec 05, phase 3) ───────────────────────

    /// The assets an analysis pass should process: everything behind `current_version` (the incremental
    /// Plan gate, §1.2/§7.2), or `force`-all, or a specific `ids` set. Carries each asset's source
    /// *connection* so the runner can rebuild the backend and fetch bytes through it.
    ///
    /// Only **federated** sources are excluded: a peer yields catalog rows, not bytes, so there is
    /// nothing local to decode (its derived data belongs to the peer that owns it). SFTP/SMB assets
    /// are targets like any other and reach their bytes through `FileSource::fetch` — issue #48.
    /// Before that they were filtered out here by `s.kind = 'local_fs'`, which meant a remote asset
    /// was never even *planned* for analysis: no embedding, no derived signals, no auto-tags, and no
    /// error either, because nothing had gone wrong — it simply was not on the list.
    ///
    /// A row whose stored connection cannot be parsed is dropped rather than defaulted: an
    /// unparseable connection cannot be fetched from, and a target that can never succeed is worse
    /// than one that was never listed (it would fail every pass, forever, at whatever version gate).
    pub fn list_analysis_targets(
        &self,
        current_version: i64,
        force: bool,
        ids: &[AssetId],
    ) -> Result<Vec<AnalysisTarget>, LibError> {
        let mut cursor = None;
        let mut targets = Vec::new();
        loop {
            let batch = self.analysis_target_batch(
                current_version,
                force,
                ids,
                cursor,
                None,
                ANALYSIS_PLAN_BATCH_MAX,
            )?;
            let sources: BTreeMap<_, _> = batch
                .sources
                .into_iter()
                .filter_map(|source| {
                    self.get_source_connection(&source.source_id)
                        .ok()
                        .map(|connection| (source.source_id, connection))
                })
                .collect();
            targets.extend(batch.targets.into_iter().filter_map(|target| {
                let connection = sources.get(&target.source_id)?;
                Some(AnalysisTarget {
                    id: target.id,
                    source_id: target.source_id,
                    connection: connection.clone(),
                    path: target.path,
                    media: target.media,
                    format: target.format,
                    content_hash: target.content_hash,
                })
            }));
            cursor = batch.next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(targets)
    }

    /// Count due analysis rows and their source attribution without hydrating any asset targets.
    pub fn analysis_plan_summary(
        &self,
        current_version: i64,
        force: bool,
        ids: &[AssetId],
    ) -> Result<AnalysisPlanSummary, LibError> {
        let conn = self.read()?;
        let (where_sql, binds) = plan_where(
            PlanKind::Analysis {
                current_version,
                force,
            },
            ids,
        );
        let total = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM asset a JOIN source s ON s.id=a.source_id {where_sql}"
                ),
                rusqlite::params_from_iter(binds.iter()),
                |row| row.get::<_, i64>(0),
            )
            .map_err(internal)? as u64;
        let mut statement = conn
            .prepare(&format!(
                "SELECT DISTINCT a.source_id FROM asset a JOIN source s ON s.id=a.source_id
                 {where_sql} ORDER BY a.source_id"
            ))
            .map_err(internal)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok(blob_to_source_id(&row.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        let end = conn
            .query_row(
                &format!(
                    "SELECT {},a.source_id,a.id FROM asset a INDEXED BY {} JOIN source s ON s.id=a.source_id
                     {where_sql} ORDER BY {} LIMIT 1",
                    PlanKind::Analysis {
                        current_version,
                        force
                    }
                    .due_column(),
                    PlanKind::Analysis {
                        current_version,
                        force
                    }
                    .planner_index(),
                    PlanKind::Analysis {
                        current_version,
                        force
                    }
                    .order_columns("DESC")
                ),
                rusqlite::params_from_iter(binds.iter()),
                |row| {
                    Ok(AnalysisPlanCursor {
                        due_version: row.get(0)?,
                        source_id: blob_to_source_id(&row.get::<_, Vec<u8>>(1)?),
                        asset_id: blob_to_asset_id(&row.get::<_, Vec<u8>>(2)?),
                    })
                },
            )
            .optional()
            .map_err(internal)?;
        Ok(AnalysisPlanSummary {
            total,
            sources: rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(internal)?,
            end,
        })
    }

    /// Fetch one stable keyset page of analysis-due work.
    pub fn analysis_target_batch(
        &self,
        current_version: i64,
        force: bool,
        ids: &[AssetId],
        after: Option<AnalysisPlanCursor>,
        through: Option<AnalysisPlanCursor>,
        limit: usize,
    ) -> Result<AnalysisPlanBatch, LibError> {
        self.plan_target_batch(
            PlanKind::Analysis {
                current_version,
                force,
            },
            ids,
            after,
            through,
            limit,
        )
    }

    /// Fetch only derivative-pending work. The V23 marker makes this proportional to backlog,
    /// including after a restart, rather than a force-list of the entire catalog.
    pub fn derivative_target_batch(
        &self,
        current_version: i64,
        after: Option<AnalysisPlanCursor>,
        limit: usize,
    ) -> Result<AnalysisPlanBatch, LibError> {
        self.plan_target_batch(
            PlanKind::Derivative { current_version },
            &[],
            after,
            None,
            limit,
        )
    }

    fn plan_target_batch(
        &self,
        kind: PlanKind,
        ids: &[AssetId],
        after: Option<AnalysisPlanCursor>,
        through: Option<AnalysisPlanCursor>,
        limit: usize,
    ) -> Result<AnalysisPlanBatch, LibError> {
        let limit = limit.clamp(1, ANALYSIS_PLAN_BATCH_MAX);
        let conn = self.read()?;
        let (mut where_sql, mut binds) = plan_where(kind, ids);
        if let Some(cursor) = after {
            push_plan_cursor(&mut where_sql, &mut binds, kind, cursor, false);
        }
        if let Some(cursor) = through {
            push_plan_cursor(&mut where_sql, &mut binds, kind, cursor, true);
        }
        binds.push(Value::Integer((limit + 1) as i64));
        let sql = format!(
            "SELECT a.id, a.source_id, a.path, a.media_type, a.format, a.content_hash,
                    {}
               FROM asset a INDEXED BY {} JOIN source s ON s.id=a.source_id
               {where_sql}
              ORDER BY {} LIMIT ?",
            kind.due_column(),
            kind.planner_index(),
            kind.order_columns("ASC")
        );
        let mut statement = conn.prepare(&sql).map_err(internal)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                let hash: Option<Vec<u8>> = row.get(5)?;
                Ok(PlanRow {
                    target: AnalysisPlanTarget {
                        id: blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                        source_id: blob_to_source_id(&row.get::<_, Vec<u8>>(1)?),
                        path: row.get(2)?,
                        media: MediaType::parse(&row.get::<_, String>(3)?)
                            .unwrap_or(MediaType::Image),
                        format: row.get(4)?,
                        content_hash: hash
                            .and_then(|value| <[u8; 32]>::try_from(value.as_slice()).ok())
                            .map(ContentHash),
                    },
                    due_version: row.get(6)?,
                })
            })
            .map_err(internal)?;
        let mut rows = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next = has_more.then(|| {
            let last = &rows[rows.len() - 1].target;
            AnalysisPlanCursor {
                due_version: rows[rows.len() - 1].due_version,
                source_id: last.source_id,
                asset_id: last.id,
            }
        });
        let mut sources = std::collections::BTreeSet::new();
        for row in &rows {
            sources.insert(row.target.source_id);
        }
        Ok(AnalysisPlanBatch {
            targets: rows.into_iter().map(|row| row.target).collect(),
            sources: sources
                .into_iter()
                .map(|source_id| AnalysisPlanSource { source_id })
                .collect(),
            next,
        })
    }

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
        let json = serde_json::to_string(peaks).map_err(internal)?;
        let conn = self.write();
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
        // Invalidate any cached ANN index (M6): the space's vectors just changed.
        self.embed_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Drop an asset's embedding in one space. The counterpart to [`Self::set_embedding`] for the
    /// case where re-analysis produces *no* vector (a document whose new revision has no readable
    /// text): leaving the previous one indexed would keep ranking the asset on content it no longer
    /// has. A no-op when there was nothing there.
    pub fn clear_embedding(&self, id: &AssetId, space_id: &str) -> Result<(), LibError> {
        let conn = self.write();
        let n = conn
            .execute(
                "DELETE FROM embedding WHERE asset_id = ?1 AND space_id = ?2",
                params![id.as_bytes().to_vec(), space_id],
            )
            .map_err(internal)?;
        if n > 0 {
            self.embed_gen
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    /// Record that an asset is now analysed at `version` (the Plan gate reads this, §7.2).
    pub fn mark_analysed(&self, id: &AssetId, version: i64) -> Result<(), LibError> {
        let conn = self.write();
        conn.execute(
            "UPDATE asset SET analysis_version = ?2, analysed_at = ?3, updated_at = ?3 WHERE id = ?1",
            params![id.as_bytes().to_vec(), version, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    // ── tags / suggestions (§1.4) ─────────────────────────────────────────────

    pub(crate) fn load_tags(conn: &Connection, id_blob: &[u8]) -> Vec<TagRef> {
        let mut stmt = match conn.prepare(
            "SELECT t.name, at.state, at.source, at.confidence, at.explanation
             FROM asset_tag at JOIN tag t ON t.id = at.tag_id
             WHERE at.asset_id = ?1 ORDER BY at.state, t.name",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![id_blob], |r| {
            Ok(TagRef {
                name: r.get(0)?,
                state: match r.get::<_, String>(1)?.as_str() {
                    "confirmed" => SuggestionState::Confirmed,
                    "rejected" => SuggestionState::Rejected,
                    _ => SuggestionState::Pending,
                },
                source: r.get(2)?,
                confidence: r.get::<_, Option<f64>>(3)?.map(|v| v as f32),
                why: r.get(4)?,
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Rewrite an asset's `tags` FTS column to confirmed tag names only. Pending automation remains
    /// discoverable in Inspector but cannot silently power full-text results before review.
    /// Best-effort: an FTS hiccup must never sink the tag write that triggered it.
    fn reindex_asset_tags(conn: &Connection, id: &AssetId) {
        let _ = conn.execute(
            "UPDATE asset_fts SET tags = COALESCE((
                SELECT group_concat(t.name, ' ') FROM asset_tag at
                JOIN tag t ON t.id = at.tag_id
                WHERE at.asset_id = ?1 AND at.state = 'confirmed'), '')
             WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
            params![id.as_bytes().to_vec()],
        );
    }

    /// Write a document's extracted body text into the `text` FTS column (schema V10).
    ///
    /// The text is stored **only** in the index, never in a base-table column: it can be a megabyte
    /// per asset, nothing but search reads it, and keeping it out of `asset` keeps the row width
    /// (and every `SELECT *`-shaped query) unchanged. The cost of that choice is that the value has
    /// to be stashed and restored on any future FTS rebuild — exactly as `tokens` and `tags`
    /// already are, and as V10's own migration does.
    pub fn set_document_text(&self, id: &AssetId, text: &str) -> Result<(), LibError> {
        let conn = self.write();
        conn.execute(
            "UPDATE asset_fts SET text = ?2
             WHERE rowid = (SELECT rowid FROM asset WHERE id = ?1)",
            params![id.as_bytes().to_vec(), text],
        )
        .map_err(internal)?;
        Ok(())
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
        explanation: &str,
    ) -> Result<(), LibError> {
        let conn = self.write();
        let tag_id = Self::intern_tag(&conn, name)?;
        conn.execute(
            "INSERT INTO asset_tag
                (asset_id, tag_id, state, source, confidence, extractor, created_at, explanation)
             VALUES (?1, ?2, 'suggested', 'auto', ?3, ?4, ?5, ?6)
             ON CONFLICT(asset_id, tag_id) DO UPDATE SET
                confidence=excluded.confidence,
                extractor=excluded.extractor,
                explanation=excluded.explanation
             WHERE asset_tag.state = 'suggested' AND asset_tag.source = 'auto'",
            params![
                id.as_bytes().to_vec(),
                tag_id,
                confidence.clamp(0.0, 1.0) as f64,
                extractor,
                now_ms(),
                explanation
            ],
        )
        .map_err(internal)?;
        Self::reindex_asset_tags(&conn, id);
        Ok(())
    }

    /// Apply one valid state transition to an existing automatic suggestion. This deliberately
    /// cannot manufacture a user tag or review an arbitrary name: `Undo` returns a decided row to
    /// pending, while accept/reject operate only on pending rows.
    pub fn review_suggestion(
        &self,
        id: &AssetId,
        name: &str,
        action: ReviewAction,
    ) -> Result<(), LibError> {
        let conn = self.write();
        let current: Option<String> = conn
            .query_row(
                "SELECT at.state FROM asset_tag at JOIN tag t ON t.id = at.tag_id
                 WHERE at.asset_id = ?1 AND t.name = ?2 COLLATE NOCASE AND at.source = 'auto'",
                params![id.as_bytes().to_vec(), name],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?;
        let current =
            current.ok_or_else(|| LibError::NotFound(format!("automatic suggestion {name:?}")))?;
        let next = match (action, current.as_str()) {
            (ReviewAction::Accept, "suggested") => "confirmed",
            (ReviewAction::Reject, "suggested") => "rejected",
            (ReviewAction::Undo, "confirmed" | "rejected") => "suggested",
            _ => {
                return Err(LibError::BadRequest(format!(
                    "cannot {action:?} a {current} suggestion"
                )))
            }
        };
        conn.execute(
            "UPDATE asset_tag SET state = ?3 WHERE asset_id = ?1
             AND tag_id = (SELECT id FROM tag WHERE name = ?2 COLLATE NOCASE)",
            params![id.as_bytes().to_vec(), name, next],
        )
        .map_err(internal)?;
        Self::reindex_asset_tags(&conn, id);
        Ok(())
    }

    /// The subset of explicit ids reachable through one visibility ceiling. Used by bulk writes to
    /// distinguish unreadable targets from readable-but-read-only targets without disclosing either.
    pub fn visible_asset_ids(
        &self,
        ids: &[AssetId],
        vis: &Visibility,
    ) -> Result<std::collections::HashSet<AssetId>, LibError> {
        if ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let conn = self.read()?;
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds = Vec::new();
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        where_sql.push_str(&format!(" AND asset.id IN ({placeholders})"));
        binds.extend(ids.iter().map(|id| Value::Blob(id.as_bytes().to_vec())));
        let mut stmt = conn
            .prepare(&format!("SELECT asset.id FROM asset {where_sql}"))
            .map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()
            .map_err(internal)
    }

    pub fn list_tags(
        &self,
        prefix: Option<&str>,
        limit: u32,
        vis: &Visibility,
    ) -> Result<Vec<TagInfo>, LibError> {
        let conn = self.read()?;
        // Whole-source grants can be answered exactly by the maintained source×tag rows. Manual
        // collection grants remain on the visibility-join path below because a collection may
        // overlap a source grant and the union must count an asset once.
        if vis
            .restricted()
            .is_none_or(|scope| scope.collections.is_empty())
        {
            let mut binds = Vec::new();
            let (from, count, manual) = if let Some(scope) = vis.restricted() {
                if scope.sources.is_empty() {
                    return Ok(Vec::new());
                }
                let placeholders = scope
                    .sources
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                binds.extend(
                    scope
                        .sources
                        .iter()
                        .map(|id| Value::Blob(id.as_bytes().to_vec())),
                );
                (
                    format!(
                        "source_tag_stat ats JOIN tag t ON t.id = ats.tag_id \
                         WHERE ats.source_id IN ({placeholders})"
                    ),
                    "SUM(ats.asset_count)",
                    "SUM(ats.manual_count)",
                )
            } else {
                (
                    "tag_stat ats JOIN tag t ON t.id = ats.tag_id WHERE 1=1".into(),
                    "ats.asset_count",
                    "ats.manual_count",
                )
            };
            let mut prefix_sql = String::new();
            if let Some(prefix) = prefix.filter(|prefix| !prefix.is_empty()) {
                prefix_sql.push_str(" AND t.name LIKE ? ESCAPE '\\'");
                binds.push(Value::Text(format!("{}%", escape_like(prefix))));
            }
            binds.push(Value::Integer(limit.clamp(1, 50) as i64));
            let sql = format!(
                "SELECT t.name, {count} n, {manual} manual FROM {from}{prefix_sql}
                 GROUP BY t.id HAVING n > 0 ORDER BY n DESC, t.name ASC LIMIT ?"
            );
            let mut stmt = conn.prepare(&sql).map_err(internal)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    Ok(TagInfo {
                        name: row.get(0)?,
                        count: row.get::<_, i64>(1)?.max(0) as u64,
                        manual: row.get::<_, i64>(2)? > 0,
                    })
                })
                .map_err(internal)?;
            return rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal);
        }
        let mut where_sql = String::from(" WHERE at.state = 'confirmed'");
        let mut binds = Vec::new();
        if let Some(prefix) = prefix.filter(|prefix| !prefix.is_empty()) {
            where_sql.push_str(" AND t.name LIKE ? ESCAPE '\\'");
            binds.push(Value::Text(format!("{}%", escape_like(prefix))));
        }
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let sql = format!(
            "SELECT t.name, COUNT(*) n,
                    MAX(CASE WHEN at.source = 'user' THEN 1 ELSE 0 END) manual
             FROM asset_tag at JOIN tag t ON t.id = at.tag_id
             JOIN asset ON asset.id = at.asset_id {where_sql}
             GROUP BY t.id ORDER BY n DESC, t.name ASC LIMIT ?"
        );
        binds.push(Value::Integer(limit.clamp(1, 50) as i64));
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok(TagInfo {
                    name: row.get(0)?,
                    count: row.get::<_, i64>(1)?.max(0) as u64,
                    manual: row.get::<_, i64>(2)? != 0,
                })
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// Preview/apply manual tag deltas in one transaction. Removing is intentionally restricted to
    /// `source = 'user'`: automatic suggestions keep their separate accept/reject lifecycle.
    pub fn edit_manual_tags(
        &self,
        ids: &[AssetId],
        add: &[String],
        remove: &[String],
        dry_run: bool,
    ) -> Result<ManualTagEditOutcome, LibError> {
        let mut conn = self.write();
        let tx = conn.transaction().map_err(internal)?;
        let mut result = TagEditResult {
            matched: ids.len() as u64,
            ..TagEditResult::default()
        };
        let mut changed_assets = Vec::new();

        for id in ids {
            let mut changed = false;
            for name in add {
                let tag_id = Self::intern_tag(&tx, name)?;
                let existing: Option<(String, String)> = tx
                    .query_row(
                        "SELECT at.state, at.source FROM asset_tag at
                         WHERE at.asset_id = ?1 AND at.tag_id = ?2",
                        params![id.as_bytes().to_vec(), &tag_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(internal)?;
                if existing
                    .as_ref()
                    .is_some_and(|(state, source)| state == "confirmed" && source == "user")
                {
                    continue;
                }
                tx.execute(
                    "INSERT INTO asset_tag
                       (asset_id, tag_id, state, source, confidence, extractor, created_at)
                     VALUES (?1, ?2, 'confirmed', 'user', NULL, NULL, ?3)
                     ON CONFLICT(asset_id, tag_id) DO UPDATE SET
                       state = 'confirmed', source = 'user', confidence = NULL, extractor = NULL,
                       explanation = NULL",
                    params![id.as_bytes().to_vec(), tag_id, now_ms()],
                )
                .map_err(internal)?;
                result.additions += 1;
                changed = true;
            }
            for name in remove {
                let removed = tx
                    .execute(
                        "DELETE FROM asset_tag WHERE asset_id = ?1 AND source = 'user'
                         AND tag_id = (SELECT id FROM tag WHERE name = ?2 COLLATE NOCASE)",
                        params![id.as_bytes().to_vec(), name],
                    )
                    .map_err(internal)?;
                if removed > 0 {
                    result.removals += 1;
                    changed = true;
                }
            }
            if changed {
                result.changed += 1;
                Self::reindex_asset_tags(&tx, id);
                let source = tx
                    .query_row(
                        "SELECT source_id FROM asset WHERE id = ?1",
                        params![id.as_bytes().to_vec()],
                        |row| Ok(blob_to_source_id(&row.get::<_, Vec<u8>>(0)?)),
                    )
                    .optional()
                    .map_err(internal)?;
                changed_assets.push((*id, source));
            }
        }
        if !dry_run {
            tx.commit().map_err(internal)?;
        }
        Ok(ManualTagEditOutcome {
            result,
            changed_assets,
        })
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
        vis: &Visibility,
    ) -> Result<(String, Vec<(AssetSummary, f32)>), LibError> {
        let conn = self.read()?;
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
            return Ok((String::new(), Vec::new())); // not embedded yet (§1.3)
        };
        let qvec = bytes_to_f32(&qbytes);
        // Over-fetch nearest neighbours (self excluded) so the facet post-filter still leaves ≥ k.
        let overfetch = (k as usize * 4).max(k as usize + 16);

        // Nearest neighbours in the space, descending cosine. The `ann` feature (M6) serves this from
        // a cached HNSW index; the default build does the exact brute-force scan (correct and the
        // ground truth the ANN parity test checks against). Both currently run under the read guard:
        // a cold `ann_for_space` builds the whole HNSW while a WAL snapshot is pinned, which is the
        // CPU hand-off issue #137 leaves for a later step (it at least no longer blocks the writer).
        #[cfg(feature = "ann")]
        let scored: Vec<(AssetId, f32)> =
            self.ann_scored(&conn, &space_id, &qvec, id, overfetch)?;
        #[cfg(not(feature = "ann"))]
        let scored: Vec<(AssetId, f32)> = {
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
            scored
        };

        // Post-filter against the facet predicate + the visibility ceiling and fetch summaries
        // (§3.3). The ceiling applies to the *candidates* — a neighbour from an unshared source
        // leaks both its existence and its content-likeness (issue #42 leak audit).
        let candidate_ids: Vec<AssetId> = scored.iter().take(overfetch).map(|(a, _)| *a).collect();
        let summaries = Self::summaries_for_ids(&conn, &candidate_ids, filters, vis)?;
        let mut out = Vec::new();
        for (aid, score) in scored {
            if out.len() >= k as usize {
                break;
            }
            if let Some(sum) = summaries.get(&aid) {
                out.push((sum.clone(), score));
            }
        }
        Ok((space_id, out))
    }

    /// The stored embedding for one asset — `(space_id, vector)`, or `None` when not yet analysed.
    /// The federated fan-out ships this vector to matched-space peers (phase 6, issue #40).
    pub fn embedding_for(&self, id: &AssetId) -> Result<Option<(String, Vec<f32>)>, LibError> {
        let conn = self.read()?;
        let row: Option<(String, Vec<u8>)> = conn
            .query_row(
                "SELECT space_id, vec FROM embedding WHERE asset_id = ?1",
                params![id.as_bytes().to_vec()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        Ok(row.map(|(s, v)| (s, bytes_to_f32(&v))))
    }

    /// Cosine-nearest neighbours of an arbitrary query vector in `space_id` — the serving side of
    /// cross-peer similarity (phase 6, issue #40). A space this catalog has no vectors in — or a
    /// dimension mismatch — is a `BadRequest`, not a silent empty page, so the caller's exact-match
    /// space gate stays honest. Same over-fetch + facet post-filter as [`Self::similar`]; no
    /// self-drop (the query vector has no local identity).
    pub fn similar_by_vector(
        &self,
        space_id: &str,
        qvec: &[f32],
        k: u32,
        filters: &[Filter],
        vis: &Visibility,
    ) -> Result<Vec<(AssetSummary, f32)>, LibError> {
        let conn = self.read()?;
        let dim: Option<i64> = conn
            .query_row(
                "SELECT LENGTH(vec) / 4 FROM embedding WHERE space_id = ?1 LIMIT 1",
                params![space_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        match dim {
            None => {
                return Err(LibError::BadRequest(format!(
                    "embedding space {space_id:?} is not served by this catalog"
                )))
            }
            Some(d) if d as usize != qvec.len() => {
                return Err(LibError::BadRequest(format!(
                    "query vector has {} dims; space {space_id:?} has {d}",
                    qvec.len()
                )))
            }
            Some(_) => {}
        }
        let overfetch = (k as usize * 4).max(k as usize + 16);
        let scored = self.nearest_in_space(&conn, space_id, qvec, overfetch)?;
        let candidate_ids: Vec<AssetId> = scored.iter().map(|(a, _)| *a).collect();
        let summaries = Self::summaries_for_ids(&conn, &candidate_ids, filters, vis)?;
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

    /// Nearest neighbours of an arbitrary query vector in `space_id`, `(id, cosine)` desc, capped at
    /// `k`. This is the text→asset entry point (semantic-search M4/M5): the engine encodes a query
    /// string into the model's shared space, then this finds the closest assets — no reference asset
    /// needed. Uses the ANN index under the `ann` feature, else an exact scan. Empty space → empty.
    pub(crate) fn nearest_in_space(
        &self,
        conn: &Connection,
        space_id: &str,
        qvec: &[f32],
        k: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        #[cfg(feature = "ann")]
        {
            match self.ann_for_space(conn, space_id) {
                Ok(index) => Ok(index.nearest(qvec, k)),
                Err(_) => Ok(Vec::new()), // empty/absent space
            }
        }
        #[cfg(not(feature = "ann"))]
        {
            let mut stmt = conn
                .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
                .map_err(internal)?;
            let rows = stmt
                .query_map(params![space_id], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(internal)?;
            let mut scored: Vec<(AssetId, f32)> = Vec::new();
            for r in rows {
                let (idb, vb) = r.map_err(internal)?;
                scored.push((blob_to_asset_id(&idb), cosine(qvec, &bytes_to_f32(&vb))));
            }
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(k);
            Ok(scored)
        }
    }

    /// ANN nearest neighbours of `qvec` in `space_id` (M6), self excluded, `(id, cosine)` desc.
    #[cfg(feature = "ann")]
    fn ann_scored(
        &self,
        conn: &Connection,
        space_id: &str,
        qvec: &[f32],
        self_id: &AssetId,
        k: usize,
    ) -> Result<Vec<(AssetId, f32)>, LibError> {
        let index = self.ann_for_space(conn, space_id)?;
        let mut out: Vec<(AssetId, f32)> = index
            .nearest(qvec, k + 1)
            .into_iter()
            .filter(|(id, _)| id != self_id)
            .collect();
        out.truncate(k);
        Ok(out)
    }

    /// Get (or lazily build + cache) the HNSW index for a space (M6). Rebuilt when an embedding
    /// write has bumped `embed_gen` since the cached copy. `conn` is the already-held lock.
    #[cfg(feature = "ann")]
    fn ann_for_space(
        &self,
        conn: &Connection,
        space_id: &str,
    ) -> Result<std::sync::Arc<crate::ann::AnnIndex>, LibError> {
        use std::sync::atomic::Ordering;
        let generation = self.embed_gen.load(Ordering::Relaxed);
        if let Some((g, idx)) = self.ann_cache.lock().unwrap().get(space_id) {
            if *g == generation {
                return Ok(idx.clone());
            }
        }
        // (Re)build from the space's current vectors.
        let mut stmt = conn
            .prepare("SELECT asset_id, vec FROM embedding WHERE space_id = ?1")
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![space_id], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(internal)?;
        let mut items: Vec<(AssetId, Vec<f32>)> = Vec::new();
        for r in rows {
            let (idb, vb) = r.map_err(internal)?;
            items.push((blob_to_asset_id(&idb), bytes_to_f32(&vb)));
        }
        let idx = std::sync::Arc::new(
            crate::ann::AnnIndex::build(items)
                .ok_or_else(|| LibError::Internal("empty embedding space".into()))?,
        );
        self.ann_cache
            .lock()
            .unwrap()
            .insert(space_id.to_string(), (generation, idx.clone()));
        Ok(idx)
    }

    /// Duplicate groups for the review view (§4). `Exact` groups by content hash; `Near` groups by
    /// embedding cosine ≥ threshold within a media space (union-find over the pairwise relation, §4.3).
    pub fn duplicates(
        &self,
        req: &DupRequest,
        vis: &Visibility,
    ) -> Result<Page<DupGroup>, LibError> {
        let mut groups: Vec<DupGroup> = Vec::new();
        let limit = req.limit.clamp(1, DUP_GROUP_PAGE_MAX) as usize;
        let has_more;
        let mut next_cursor = None;
        let mut partial = dam_api::PartialStatus::default();

        match req.kind {
            DupKind::Exact => {
                let conn = self.read()?;
                let after = decode_exact_dup_cursor(req.after.as_ref())?;
                let mut where_sql = String::from(" WHERE content_hash IS NOT NULL");
                let mut binds: Vec<Value> = Vec::new();
                if let Some(m) = req.media {
                    where_sql.push_str(" AND media_type = ?");
                    binds.push(Value::Text(m.as_str().to_string()));
                }
                push_visibility(vis, "asset", &mut where_sql, &mut binds);
                let mut having = String::from(" HAVING n > 1");
                if let Some((count, hash)) = after {
                    having.push_str(" AND (n < ? OR (n = ? AND content_hash > ?))");
                    binds.push(Value::Integer(count as i64));
                    binds.push(Value::Integer(count as i64));
                    binds.push(Value::Blob(hash));
                }
                let sql = format!(
                    "SELECT content_hash, COUNT(*) n FROM asset {where_sql}
                     GROUP BY content_hash {having}
                     ORDER BY n DESC, content_hash ASC LIMIT ?"
                );
                binds.push(Value::Integer((limit + 1) as i64));
                let mut stmt = conn.prepare(&sql).map_err(internal)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u32>(1)?))
                    })
                    .map_err(internal)?;
                let mut exact: Vec<(Vec<u8>, u32)> = rows
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(internal)?;
                has_more = exact.len() > limit;
                exact.truncate(limit);
                if has_more {
                    next_cursor = exact
                        .last()
                        .map(|(hash, count)| Cursor(format!("exact:{count}:{}", encode_hex(hash))));
                }

                if !exact.is_empty() {
                    // Rank within every selected hash in SQL, then cap before summary hydration.
                    // The whole review page is hydrated by the single summaries query below.
                    let mut ranked_where = String::from(" WHERE asset.content_hash IS NOT NULL");
                    let mut ranked_binds: Vec<Value> = Vec::new();
                    push_visibility(vis, "asset", &mut ranked_where, &mut ranked_binds);
                    let hash_ph = exact.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                    let member_sql = format!(
                        "WITH ranked AS (
                           SELECT asset.id, asset.content_hash,
                                  COALESCE(asset.size_bytes, 0) member_size,
                                  ROW_NUMBER() OVER (
                                    PARTITION BY asset.content_hash
                                    ORDER BY COALESCE(asset.size_bytes, 0) DESC, asset.id ASC
                                  ) member_rank
                           FROM asset {ranked_where}
                         )
                         SELECT id, content_hash, member_size FROM ranked
                         WHERE member_rank <= ? AND content_hash IN ({hash_ph})
                         ORDER BY content_hash ASC, member_rank ASC"
                    );
                    ranked_binds.push(Value::Integer(DUP_GROUP_MEMBER_MAX as i64));
                    for (hash, _) in &exact {
                        ranked_binds.push(Value::Blob(hash.clone()));
                    }
                    let mut member_stmt = conn.prepare(&member_sql).map_err(internal)?;
                    let member_rows = member_stmt
                        .query_map(rusqlite::params_from_iter(ranked_binds.iter()), |r| {
                            Ok((
                                r.get::<_, Vec<u8>>(1)?,
                                blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?),
                                r.get::<_, i64>(2)?.max(0) as u64,
                            ))
                        })
                        .map_err(internal)?;
                    let mut ids_by_hash: std::collections::HashMap<Vec<u8>, Vec<AssetId>> =
                        std::collections::HashMap::new();
                    let mut size_by_id = std::collections::HashMap::new();
                    let mut all_ids = Vec::new();
                    for row in member_rows {
                        let (hash, id, size) = row.map_err(internal)?;
                        ids_by_hash.entry(hash).or_default().push(id);
                        size_by_id.insert(id, size);
                        all_ids.push(id);
                    }
                    let member_details = Self::duplicate_members_for_ids(&conn, &all_ids, vis)?;
                    for (hash, total_members) in exact {
                        let ids = ids_by_hash.remove(&hash).unwrap_or_default();
                        let group_key = encode_hex(&hash);
                        let members_cursor = (ids.len() < total_members as usize)
                            .then(|| member_cursor(&ids, &size_by_id))
                            .flatten();
                        if let Some(group) = Self::build_dup_group_from_summaries(
                            DupGroupSeed {
                                kind: DupKind::Exact,
                                ids: &ids,
                                total_members,
                                group: Some(group_key),
                                members_cursor,
                                signal: "identical bytes (same content hash)",
                                review: &format!("exact:{}", encode_hex(&hash)),
                            },
                            &member_details,
                        ) {
                            if let Some(group) =
                                Self::apply_duplicate_review(&conn, group, req.review)?
                            {
                                groups.push(group);
                            }
                        }
                    }
                }
            }
            DupKind::Near => {
                let offset = decode_near_dup_cursor(req.after.as_ref(), NEAR_DUP_CANDIDATE_MAX)?;
                // Compare only embeddings from the same declared space and media. The old flat
                // candidate list compared unrelated dimensions (for example image stats against
                // document text) whenever "All media" was selected, manufacturing groups from a
                // signal that had no meaning. A media appears in near review only if it has rows in
                // a real embedding space; exact review remains independent and covers all five.
                let mut sql = String::from(
                    "SELECT e.asset_id, e.space_id, e.media_type, e.vec
                       FROM embedding e JOIN asset a ON a.id = e.asset_id",
                );
                let mut where_sql = String::from(" WHERE 1=1");
                let mut binds: Vec<Value> = Vec::new();
                if let Some(m) = req.media {
                    where_sql.push_str(" AND e.media_type = ?");
                    binds.push(Value::Text(m.as_str().to_string()));
                }
                push_visibility(vis, "a", &mut where_sql, &mut binds);
                sql.push_str(&where_sql);
                sql.push_str(" ORDER BY e.media_type, e.space_id, e.asset_id LIMIT ?");
                binds.push(Value::Integer((NEAR_DUP_CANDIDATE_MAX + 1) as i64));
                // Scoped: the pairwise union-find below is CPU work over up to
                // `NEAR_DUP_CANDIDATE_MAX` vectors, and a read guard pins a WAL snapshot for as long
                // as it lives. The candidates are owned by then, so the connection goes back to the
                // pool first and a second guard hydrates the surviving members.
                let mut candidates = Vec::new();
                {
                    let conn = self.read()?;
                    let mut stmt = conn.prepare(&sql).map_err(internal)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                            Ok((
                                r.get::<_, Vec<u8>>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, Vec<u8>>(3)?,
                            ))
                        })
                        .map_err(internal)?;
                    for r in rows {
                        let (id_blob, space, media, vbytes) = r.map_err(internal)?;
                        candidates.push((
                            blob_to_asset_id(&id_blob),
                            space,
                            media,
                            bytes_to_f32(&vbytes),
                        ));
                    }
                }
                if candidates.len() > NEAR_DUP_CANDIDATE_MAX {
                    candidates.truncate(NEAR_DUP_CANDIDATE_MAX);
                    partial.complete = false;
                    partial.warnings.push(dam_api::ItemWarning {
                        subject: "near-duplicates".into(),
                        code: "duplicate_candidates_capped".into(),
                        message: format!(
                            "near-duplicate analysis is capped at {NEAR_DUP_CANDIDATE_MAX} candidates"
                        ),
                    });
                }

                let computed = near_components(candidates);
                let remaining = computed.get(offset..).unwrap_or_default();
                let all_ids: Vec<AssetId> = remaining
                    .iter()
                    .flat_map(|(_, _, ids)| ids.iter().take(DUP_GROUP_MEMBER_MAX).copied())
                    .collect();
                let conn = self.read()?;
                let member_details = Self::duplicate_members_for_ids(&conn, &all_ids, vis)?;
                let mut consumed = 0usize;
                for (_media, space, ids) in remaining {
                    consumed += 1;
                    let member_ids: Vec<AssetId> =
                        ids.iter().take(DUP_GROUP_MEMBER_MAX).copied().collect();
                    let review = near_review_id(space, ids);
                    if let Some(g) = Self::build_dup_group_from_summaries(
                        DupGroupSeed {
                            kind: DupKind::Near,
                            ids: &member_ids,
                            total_members: ids.len() as u32,
                            group: None,
                            members_cursor: None,
                            signal: &format!("{space} embedding cosine ≥ {NEAR_DUP_COSINE:.2}"),
                            review: &review,
                        },
                        &member_details,
                    ) {
                        if let Some(group) = Self::apply_duplicate_review(&conn, g, req.review)? {
                            groups.push(group);
                            if groups.len() == limit {
                                break;
                            }
                        }
                    }
                }
                let next_offset = offset.saturating_add(consumed);
                has_more = next_offset < computed.len();
                if has_more {
                    next_cursor = Some(Cursor(format!("near:{next_offset}")));
                }
            }
        }
        let mut page = Page::new(groups, next_cursor);
        page.partial = partial;
        Ok(page)
    }

    /// Persist review metadata and any requested catalog-only removals in one SQLite transaction.
    /// Blocking remains content-addressed: it removes every catalog row with the same hash and
    /// records that hash for future scans, but never opens or deletes a source file.
    pub fn review_duplicate(
        &self,
        req: &DupReviewRequest,
    ) -> Result<DuplicateReviewOutcome, LibError> {
        if req.review.len() > 96
            || !(req.review.starts_with("exact:") || req.review.starts_with("near:"))
        {
            return Err(LibError::BadRequest("invalid duplicate review id".into()));
        }
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;

        let group_members = duplicate_review_members(&tx, &req.review)?;

        if let Some(keep) = req.keep {
            if !group_members.contains(&keep) {
                return Err(LibError::BadRequest(format!(
                    "keep asset {keep} is not a member of {}",
                    req.review
                )));
            }
        }

        let mut removed = BTreeMap::<AssetId, SourceId>::new();
        for removal in &req.removals {
            if !group_members.contains(&removal.asset) {
                return Err(LibError::BadRequest(format!(
                    "removal asset {} is not a member of {}",
                    removal.asset, req.review
                )));
            }
            let row: Option<(Option<Vec<u8>>, String, Vec<u8>)> = tx
                .query_row(
                    "SELECT content_hash, filename, source_id FROM asset WHERE id = ?1",
                    params![removal.asset.as_bytes().to_vec()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(internal)?;
            let (hash, filename, source_blob) =
                row.ok_or_else(|| LibError::NotFound(format!("asset {}", removal.asset)))?;

            if removal.block {
                if let Some(hash) = hash {
                    let mut stmt = tx
                        .prepare("SELECT id, source_id FROM asset WHERE content_hash = ?1")
                        .map_err(internal)?;
                    let rows = stmt
                        .query_map(params![hash.clone()], |row| {
                            Ok((
                                blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                                blob_to_source_id(&row.get::<_, Vec<u8>>(1)?),
                            ))
                        })
                        .map_err(internal)?;
                    for row in rows {
                        let (id, source) = row.map_err(internal)?;
                        removed.insert(id, source);
                    }
                    drop(stmt);
                    tx.execute(
                        "DELETE FROM asset WHERE content_hash = ?1",
                        params![hash.clone()],
                    )
                    .map_err(internal)?;
                    tx.execute(
                        "INSERT INTO blocklist (content_hash, label, blocked_at)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(content_hash) DO UPDATE SET label = excluded.label",
                        params![hash, filename, now_ms()],
                    )
                    .map_err(internal)?;
                    continue;
                }
            }

            let source = <[u8; 16]>::try_from(source_blob.as_slice())
                .ok()
                .map(SourceId::from_bytes)
                .ok_or_else(|| LibError::Internal("invalid source id in asset row".into()))?;
            tx.execute(
                "DELETE FROM asset WHERE id = ?1",
                params![removal.asset.as_bytes().to_vec()],
            )
            .map_err(internal)?;
            removed.insert(removal.asset, source);
        }

        let state = match req.state {
            DupReviewState::Pending => "pending",
            DupReviewState::Resolved => "resolved",
            DupReviewState::Dismissed => "dismissed",
        };
        tx.execute(
            "INSERT INTO duplicate_review (review_key, state, chosen_keep, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(review_key) DO UPDATE SET
                 state = excluded.state,
                 chosen_keep = COALESCE(excluded.chosen_keep, duplicate_review.chosen_keep),
                 updated_at = excluded.updated_at",
            params![
                req.review,
                state,
                req.keep.map(|id| id.as_bytes().to_vec()),
                now_ms()
            ],
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)?;
        Ok(DuplicateReviewOutcome {
            removed_assets: removed.into_iter().collect(),
        })
    }

    /// Set-based exact-duplicate membership for the currently retained browse rows. The request is
    /// bounded by the service before reaching SQLite and this query returns no asset summaries.
    pub fn duplicate_membership(
        &self,
        ids: &[AssetId],
        vis: &Visibility,
    ) -> Result<Vec<DupMembership>, LibError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read()?;
        let requested_ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut requested_where =
            format!(" WHERE asset.content_hash IS NOT NULL AND asset.id IN ({requested_ph})");
        let mut requested_binds: Vec<Value> = ids
            .iter()
            .map(|id| Value::Blob(id.as_bytes().to_vec()))
            .collect();
        push_visibility(vis, "asset", &mut requested_where, &mut requested_binds);
        let mut count_where = String::from(" WHERE asset.content_hash IS NOT NULL");
        let mut count_binds = Vec::new();
        push_visibility(vis, "asset", &mut count_where, &mut count_binds);
        requested_binds.extend(count_binds);
        let sql = format!(
            "WITH requested AS (
               SELECT asset.id, asset.content_hash FROM asset {requested_where}
             ), counts AS (
               SELECT asset.content_hash, COUNT(*) n FROM asset
               JOIN (SELECT DISTINCT content_hash FROM requested) wanted
                 ON wanted.content_hash = asset.content_hash
               {count_where} GROUP BY asset.content_hash
             )
             SELECT requested.id, lower(hex(requested.content_hash)), counts.n
             FROM requested JOIN counts USING (content_hash) WHERE counts.n > 1"
        );
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(requested_binds.iter()), |row| {
                Ok(DupMembership {
                    asset: blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                    group: row.get(1)?,
                    count: row.get(2)?,
                })
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// One exact group for Inspector. Only one group is hydrated and its summaries are hard-capped.
    pub fn duplicate_group(
        &self,
        id: &AssetId,
        vis: &Visibility,
    ) -> Result<Option<DupGroup>, LibError> {
        let Some(membership) = self.duplicate_membership(&[*id], vis)?.into_iter().next() else {
            return Ok(None);
        };
        let hash = decode_hash_hex(&membership.group)?;
        let conn = self.read()?;
        let mut where_sql = String::from(" WHERE asset.content_hash = ?");
        let mut binds = vec![Value::Blob(hash)];
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let sql = format!(
            "SELECT asset.id, COALESCE(asset.size_bytes, 0) FROM asset {where_sql}
             ORDER BY COALESCE(asset.size_bytes, 0) DESC, asset.id ASC LIMIT ?"
        );
        binds.push(Value::Integer(DUP_GROUP_MEMBER_MAX as i64));
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok((
                    blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                    row.get::<_, i64>(1)?.max(0) as u64,
                ))
            })
            .map_err(internal)?;
        let ordered = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;
        let ids: Vec<AssetId> = ordered.iter().map(|(id, _)| *id).collect();
        let members_cursor = (ids.len() < membership.count as usize)
            .then(|| {
                ordered
                    .last()
                    .map(|(id, size)| Cursor(format!("members:{size}:{id}")))
            })
            .flatten();
        let member_details = Self::duplicate_members_for_ids(&conn, &ids, vis)?;
        let group = Self::build_dup_group_from_summaries(
            DupGroupSeed {
                kind: DupKind::Exact,
                ids: &ids,
                total_members: membership.count,
                group: Some(membership.group.clone()),
                members_cursor,
                signal: "identical bytes (same content hash)",
                review: &format!("exact:{}", membership.group),
            },
            &member_details,
        );
        group
            .map(|group| Self::apply_duplicate_review(&conn, group, DupReviewFilter::All))
            .transpose()
            .map(Option::flatten)
    }

    /// Continue one exact group's member summaries with a stable `(size, id)` keyset.
    pub fn duplicate_group_members(
        &self,
        req: &DupGroupMembersRequest,
        vis: &Visibility,
    ) -> Result<Page<DupMember>, LibError> {
        let hash = decode_hash_hex(&req.group)?;
        let after = decode_dup_member_cursor(req.after.as_ref())?;
        let limit = req.limit.clamp(1, DUP_GROUP_PAGE_MAX) as usize;
        let conn = self.read()?;
        let mut where_sql = String::from(" WHERE asset.content_hash = ?");
        let mut binds = vec![Value::Blob(hash)];
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        if let Some((size, id)) = after {
            where_sql.push_str(
                " AND (COALESCE(asset.size_bytes, 0) < ? OR
                       (COALESCE(asset.size_bytes, 0) = ? AND asset.id > ?))",
            );
            binds.push(Value::Integer(size as i64));
            binds.push(Value::Integer(size as i64));
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        let sql = format!(
            "SELECT asset.id, COALESCE(asset.size_bytes, 0) FROM asset {where_sql}
             ORDER BY COALESCE(asset.size_bytes, 0) DESC, asset.id ASC LIMIT ?"
        );
        binds.push(Value::Integer((limit + 1) as i64));
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok((
                    blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                    row.get::<_, i64>(1)?.max(0) as u64,
                ))
            })
            .map_err(internal)?;
        let mut ordered = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;
        let has_more = ordered.len() > limit;
        ordered.truncate(limit);
        let ids: Vec<AssetId> = ordered.iter().map(|(id, _)| *id).collect();
        let members = Self::duplicate_members_for_ids(&conn, &ids, vis)?;
        let items: Vec<DupMember> = ids
            .iter()
            .filter_map(|id| members.get(id).cloned())
            .collect();
        let cursor = has_more
            .then(|| {
                ordered
                    .last()
                    .map(|(id, size)| Cursor(format!("members:{size}:{id}")))
            })
            .flatten();
        Ok(Page::new(items, cursor))
    }

    /// Build a `DupGroup` from member ids: load review-specific comparison metadata and pick the
    /// suggested keep by license, size, and modification time. Skips groups that collapse to fewer
    /// than two resolvable members —
    /// which also re-forms visibility-filtered groups (issue #42 leak audit): a duplicate pair
    /// spanning a shared and an unshared source collapses to one visible member, and a group of one
    /// is not a duplicate, so the hidden file's existence never shows.
    fn build_dup_group_from_summaries(
        seed: DupGroupSeed<'_>,
        map: &std::collections::HashMap<AssetId, DupMember>,
    ) -> Option<DupGroup> {
        let mut members: Vec<DupMember> = seed
            .ids
            .iter()
            .filter_map(|id| map.get(id).cloned())
            .collect();
        if members.len() < 2 {
            return None;
        }
        // Near components arrive in signal order; make their presentation stable and fidelity-led.
        if seed.kind == DupKind::Near {
            members.sort_by_key(|member| (std::cmp::Reverse(member.asset.size), member.asset.id));
        }
        let suggested_keep = members
            .iter()
            .max_by_key(|member| {
                (
                    license_rank(member.asset.license.status),
                    member.asset.size,
                    member.modified_at.unwrap_or(i64::MIN),
                    member.asset.id,
                )
            })
            .expect("duplicate group has members")
            .asset
            .id;
        let winner = members
            .iter()
            .find(|member| member.asset.id == suggested_keep)
            .expect("suggested keep is a member");
        let suggested_keep_reason = format!(
            "Best license ({}); ties prefer the larger file, newer modified date, then stable catalog order.",
            winner.asset.license.status.as_str()
        );
        let media = members[0].asset.media;
        Some(DupGroup {
            kind: seed.kind,
            media,
            group: seed.group,
            review: seed.review.to_string(),
            review_state: DupReviewState::Pending,
            members,
            total_members: seed.total_members,
            members_cursor: seed.members_cursor,
            signal: seed.signal.to_string(),
            suggested_keep,
            suggested_keep_reason,
            chosen_keep: None,
        })
    }

    fn apply_duplicate_review(
        conn: &Connection,
        mut group: DupGroup,
        filter: DupReviewFilter,
    ) -> Result<Option<DupGroup>, LibError> {
        let saved: Option<(String, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT state, chosen_keep FROM duplicate_review WHERE review_key = ?1",
                params![group.review],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        if let Some((state, keep)) = saved {
            group.review_state = match state.as_str() {
                "resolved" => DupReviewState::Resolved,
                "dismissed" => DupReviewState::Dismissed,
                _ => DupReviewState::Pending,
            };
            group.chosen_keep = keep.and_then(|bytes| {
                <[u8; 16]>::try_from(bytes.as_slice())
                    .ok()
                    .map(AssetId::from_bytes)
            });
        }
        let included = match filter {
            DupReviewFilter::All => true,
            DupReviewFilter::Pending => group.review_state == DupReviewState::Pending,
            DupReviewFilter::Resolved => group.review_state == DupReviewState::Resolved,
            DupReviewFilter::Dismissed => group.review_state == DupReviewState::Dismissed,
        };
        Ok(included.then_some(group))
    }

    /// Hydrate the metadata whose only consumer is duplicate review. Keeping these columns off
    /// `AssetSummary` avoids adding path/source/timestamps to every ordinary browse row.
    fn duplicate_members_for_ids(
        conn: &Connection,
        ids: &[AssetId],
        vis: &Visibility,
    ) -> Result<std::collections::HashMap<AssetId, DupMember>, LibError> {
        let mut map = std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds = Vec::new();
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        where_sql.push_str(&format!(" AND asset.id IN ({placeholders})"));
        binds.extend(ids.iter().map(|id| Value::Blob(id.as_bytes().to_vec())));
        let sql = format!(
            "{GRID_SELECT}, asset.path, source.name, asset.source_modified_at, asset.analysed_at
               FROM asset JOIN source ON source.id = asset.source_id {ATTR_JOINS} {where_sql}"
        );
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok(DupMember {
                    asset: row_to_summary(row)?,
                    path: row.get(16)?,
                    source: row.get(17)?,
                    modified_at: row.get(18)?,
                    analyzed_at: row.get(19)?,
                })
            })
            .map_err(internal)?;
        for row in rows {
            let member = row.map_err(internal)?;
            map.insert(member.asset.id, member);
        }
        Ok(map)
    }

    /// Fetch summaries for a set of ids, applying the same faceted filters as text search (§3.3)
    /// plus the visibility ceiling — the shared choke point of the similarity, hybrid-search, and
    /// dedup candidate paths, so an unreachable asset drops out of all of them in one place.
    /// Returns a map so callers can preserve their own ordering (similarity score / dup grouping).
    pub(crate) fn summaries_for_ids(
        conn: &Connection,
        ids: &[AssetId],
        filters: &[Filter],
        vis: &Visibility,
    ) -> Result<std::collections::HashMap<AssetId, AssetSummary>, LibError> {
        let mut map = std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds: Vec<Value> = Vec::new();
        push_visibility(vis, "asset", &mut where_sql, &mut binds);
        for f in filters {
            apply_filter(f, &mut where_sql, &mut binds)?;
        }
        let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        where_sql.push_str(&format!(" AND asset.id IN ({ph})"));
        for id in ids {
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        let sql = format!("{GRID_SELECT} FROM asset {ATTR_JOINS} {where_sql}");
        let mut stmt = conn.prepare(&sql).map_err(internal)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), row_to_summary)
            .map_err(internal)?;
        for r in rows {
            let s = r.map_err(internal)?;
            map.insert(s.id, s);
        }
        Ok(map)
    }
}

/// Decode a little-endian f32 blob (an embedding row's `vec`).
pub(crate) fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn license_rank(status: LicenseStatus) -> u8 {
    match status {
        LicenseStatus::Permissive => 3,
        LicenseStatus::Attribution => 2,
        LicenseStatus::Unknown => 1,
        LicenseStatus::Restricted => 0,
    }
}

fn near_components(candidates: Vec<NearCandidate>) -> Vec<NearComponent> {
    let mut partitions = NearPartitions::new();
    for (id, space, media, vector) in candidates {
        // Empty vectors carry no signal and must not become a partition whose cosine
        // implementation treats every pair as zero-length neighbours.
        if !vector.is_empty() {
            partitions
                .entry((media, space, vector.len()))
                .or_default()
                .push((id, vector));
        }
    }

    let mut computed = Vec::new();
    for ((media, space, _dimension), partition) in partitions {
        let mut uf = UnionFind::new(partition.len());
        for i in 0..partition.len() {
            for j in (i + 1)..partition.len() {
                if cosine(&partition[i].1, &partition[j].1) >= NEAR_DUP_COSINE {
                    uf.union(i, j);
                }
            }
        }
        for component in uf
            .components()
            .into_iter()
            .filter(|component| component.len() >= 2)
        {
            let mut ids: Vec<AssetId> = component
                .into_iter()
                .map(|index| partition[index].0)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            if ids.len() >= 2 {
                computed.push((media.clone(), space.clone(), ids));
            }
        }
    }
    computed.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    computed
}

fn near_review_id(space: &str, ids: &[AssetId]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(space.as_bytes());
    for id in ids {
        hash.update(id.as_bytes());
    }
    format!("near:{}", hash.finalize().to_hex())
}

/// Resolve a review id back to its current server-computed membership. Mutations must never trust
/// caller-supplied asset ids: an exact review proves the content hash, while a near review is
/// reconstructed from the same bounded, media/space/dimension-partitioned signal set as listing.
fn duplicate_review_members(
    conn: &Connection,
    review: &str,
) -> Result<std::collections::BTreeSet<AssetId>, LibError> {
    if let Some(encoded_hash) = review.strip_prefix("exact:") {
        let hash = decode_hash_hex(encoded_hash)?;
        if hash.len() != 32 {
            return Err(LibError::BadRequest("invalid duplicate review id".into()));
        }
        let mut stmt = conn
            .prepare("SELECT id FROM asset WHERE content_hash = ?1 ORDER BY id")
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![hash], |row| {
                Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
            })
            .map_err(internal)?;
        let members = rows
            .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()
            .map_err(internal)?;
        if members.len() < 2 {
            return Err(LibError::BadRequest(
                "duplicate review group is no longer available".into(),
            ));
        }
        return Ok(members);
    }

    if !review.starts_with("near:") || review.len() != "near:".len() + 64 {
        return Err(LibError::BadRequest("invalid duplicate review id".into()));
    }
    let mut stmt = conn
        .prepare(
            "SELECT e.asset_id, e.space_id, e.media_type, e.vec
             FROM embedding e JOIN asset a ON a.id = e.asset_id
             ORDER BY e.media_type, e.space_id, e.asset_id LIMIT ?1",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![(NEAR_DUP_CANDIDATE_MAX + 1) as i64], |row| {
            Ok((
                blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?),
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                bytes_to_f32(&row.get::<_, Vec<u8>>(3)?),
            ))
        })
        .map_err(internal)?;
    let mut candidates = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    candidates.truncate(NEAR_DUP_CANDIDATE_MAX);
    for (_media, space, ids) in near_components(candidates) {
        if near_review_id(&space, &ids) == review {
            return Ok(ids.into_iter().collect());
        }
    }
    Err(LibError::BadRequest(
        "duplicate review group is no longer available".into(),
    ))
}

fn decode_exact_dup_cursor(cursor: Option<&Cursor>) -> Result<Option<(u32, Vec<u8>)>, LibError> {
    let Some(Cursor(raw)) = cursor else {
        return Ok(None);
    };
    let mut parts = raw.split(':');
    let valid_prefix = parts.next() == Some("exact");
    let count = parts.next().and_then(|part| part.parse::<u32>().ok());
    let hash = parts.next().and_then(|part| decode_hash_hex(part).ok());
    if !valid_prefix
        || count.is_none()
        || hash.as_ref().is_none_or(|hash| hash.len() != 32)
        || parts.next().is_some()
    {
        return Err(LibError::BadRequest(
            "invalid exact-duplicate cursor".into(),
        ));
    }
    Ok(Some((count.unwrap(), hash.unwrap())))
}

fn decode_hash_hex(raw: &str) -> Result<Vec<u8>, LibError> {
    if raw.len() != 64 || !raw.is_ascii() {
        return Err(LibError::BadRequest("invalid duplicate hash".into()));
    }
    (0..raw.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&raw[index..index + 2], 16)
                .map_err(|_| LibError::BadRequest("invalid duplicate hash".into()))
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn member_cursor(
    ids: &[AssetId],
    sizes: &std::collections::HashMap<AssetId, u64>,
) -> Option<Cursor> {
    let id = *ids.last()?;
    let size = sizes.get(&id)?;
    Some(Cursor(format!("members:{size}:{id}")))
}

fn decode_near_dup_cursor(cursor: Option<&Cursor>, max: usize) -> Result<usize, LibError> {
    let Some(Cursor(raw)) = cursor else {
        return Ok(0);
    };
    let Some(offset) = raw
        .strip_prefix("near:")
        .and_then(|part| part.parse::<usize>().ok())
        .filter(|offset| *offset <= max)
    else {
        return Err(LibError::BadRequest("invalid near-duplicate cursor".into()));
    };
    Ok(offset)
}

fn decode_dup_member_cursor(cursor: Option<&Cursor>) -> Result<Option<(u64, AssetId)>, LibError> {
    let Some(Cursor(raw)) = cursor else {
        return Ok(None);
    };
    let mut parts = raw.split(':');
    let valid_prefix = parts.next() == Some("members");
    let size = parts
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .filter(|size| *size <= i64::MAX as u64);
    let id = parts.next().and_then(|part| part.parse::<AssetId>().ok());
    if !valid_prefix || size.is_none() || id.is_none() || parts.next().is_some() {
        return Err(LibError::BadRequest(
            "invalid duplicate-member cursor".into(),
        ));
    }
    Ok(Some((size.unwrap(), id.unwrap())))
}

/// Cosine similarity. Vectors are stored L2-normalised, so this is a dot product; we still divide by
/// the norms defensively in case a legacy/zero vector slips in.
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
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
        let mut map: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for i in 0..self.parent.len() {
            let root = self.find(i);
            map.entry(root).or_default().push(i);
        }
        let mut components: Vec<Vec<usize>> = map.into_values().collect();
        // `HashMap` iteration is intentionally random; a cursor page needs a repeatable component
        // order for the same candidate snapshot.
        components.sort_by_key(|component| component.first().copied().unwrap_or(usize::MAX));
        components
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::service::VisibilityScope;
    use dam_sources::{FederatedConfig, SftpConfig, SourceConnection};

    fn duplicate_store(groups: usize, members_per_group: usize) -> Store {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/duplicate-test".into(),
                },
                "duplicates",
                false,
            )
            .unwrap();
        for group in 0..groups {
            let mut hash = [0_u8; 32];
            hash[..8].copy_from_slice(&(group as u64).to_be_bytes());
            for member in 0..members_per_group {
                store
                    .upsert_asset(&NewAsset {
                        source_id: source,
                        path: format!("{group}/{member}.png"),
                        filename: format!("{member}.png"),
                        content_hash: Some(ContentHash(hash)),
                        size_bytes: Some(member as i64 + 1),
                        source_modified_at: None,
                        scanned_at: now_ms(),
                        media_type: MediaType::Image,
                        format: "png".into(),
                    })
                    .unwrap();
            }
        }
        store
    }

    fn first_asset(store: &Store) -> AssetId {
        let conn = store.read().unwrap();
        conn.query_row("SELECT id FROM asset LIMIT 1", [], |row| {
            Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
        })
        .unwrap()
    }

    fn duplicate_request(kind: DupKind, media: Option<MediaType>) -> DupRequest {
        DupRequest {
            kind,
            media,
            limit: 20,
            after: None,
            review: DupReviewFilter::Pending,
        }
    }

    fn insert_test_asset(
        store: &Store,
        source_id: SourceId,
        path: &str,
        media_type: MediaType,
        content_hash: Option<ContentHash>,
    ) -> AssetId {
        store
            .upsert_asset(&NewAsset {
                source_id,
                path: path.into(),
                filename: path.into(),
                content_hash,
                size_bytes: Some(path.len() as i64),
                source_modified_at: Some(1_700_000_000_000),
                scanned_at: now_ms(),
                media_type,
                format: path.rsplit('.').next().unwrap_or("bin").into(),
            })
            .unwrap()
            .0
    }

    #[test]
    fn manual_tag_edit_preview_is_reversible_idempotent_and_preserves_suggestions() {
        let store = duplicate_store(1, 1);
        let id = first_asset(&store);
        store
            .suggest_tag(&id, "automatic", 0.8, "test@1", "test reason")
            .unwrap();

        let preview = store
            .edit_manual_tags(&[id], &["manual".into()], &["automatic".into()], true)
            .unwrap()
            .result;
        assert_eq!(
            (preview.changed, preview.additions, preview.removals),
            (1, 1, 0)
        );
        let tags = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes())
        };
        assert_eq!(
            tags.len(),
            1,
            "dry-run must roll back tag rows and FTS changes"
        );
        assert_eq!(tags[0].source, "auto");

        let added = store
            .edit_manual_tags(&[id], &["manual".into()], &[], false)
            .unwrap()
            .result;
        assert_eq!((added.changed, added.additions), (1, 1));
        let again = store
            .edit_manual_tags(&[id], &["manual".into()], &[], false)
            .unwrap()
            .result;
        assert_eq!((again.changed, again.additions), (0, 0));

        let removed = store
            .edit_manual_tags(&[id], &[], &["manual".into(), "automatic".into()], false)
            .unwrap()
            .result;
        assert_eq!((removed.changed, removed.removals), (1, 1));
        let tags = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes())
        };
        assert_eq!(tags.len(), 1);
        assert_eq!(
            tags[0].name, "automatic",
            "manual remove cannot remove auto tags"
        );
    }

    #[test]
    fn manually_adding_an_auto_tag_converts_authorship_and_vocabulary_is_confirmed_only() {
        let store = duplicate_store(1, 1);
        let id = first_asset(&store);
        store
            .suggest_tag(&id, "convert-me", 0.9, "test@1", "test reason")
            .unwrap();
        assert!(
            store
                .list_tags(None, 20, &Visibility::Full)
                .unwrap()
                .is_empty(),
            "suggestions are not manual-tag autocomplete vocabulary"
        );

        store
            .edit_manual_tags(&[id], &["convert-me".into()], &[], false)
            .unwrap();
        let tags = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes())
        };
        assert_eq!(tags[0].source, "user");
        assert_eq!(tags[0].state, SuggestionState::Confirmed);
        assert_eq!(tags[0].confidence, None);
        assert_eq!(tags[0].why, None);
        let vocabulary = store
            .list_tags(Some("convert"), 20, &Visibility::Full)
            .unwrap();
        assert_eq!(vocabulary.len(), 1);
        assert!(vocabulary[0].manual);
    }

    #[test]
    fn suggestion_decisions_survive_reanalysis_and_undo_reopens_pending_metadata() {
        let store = duplicate_store(1, 1);
        let id = first_asset(&store);
        store
            .suggest_tag(&id, "texture", 0.6, "image@1", "v1 visual classifier")
            .unwrap();
        store
            .review_suggestion(&id, "texture", ReviewAction::Accept)
            .unwrap();

        // A newer extractor may refresh undecided evidence, but cannot overwrite a human decision.
        store
            .suggest_tag(&id, "texture", 0.95, "image@2", "v2 visual classifier")
            .unwrap();
        let decided = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes()).remove(0)
        };
        assert_eq!(decided.state, SuggestionState::Confirmed);
        assert_eq!(decided.confidence, Some(0.6));
        assert_eq!(decided.why.as_deref(), Some("v1 visual classifier"));

        store
            .review_suggestion(&id, "texture", ReviewAction::Undo)
            .unwrap();
        store
            .suggest_tag(&id, "texture", 0.95, "image@2", "v2 visual classifier")
            .unwrap();
        let reopened = {
            let conn = store.read().unwrap();
            Store::load_tags(&conn, id.as_bytes()).remove(0)
        };
        assert_eq!(reopened.state, SuggestionState::Pending);
        assert_eq!(reopened.confidence, Some(0.95));
        assert_eq!(reopened.why.as_deref(), Some("v2 visual classifier"));

        assert!(matches!(
            store.review_suggestion(&id, "not-proposed", ReviewAction::Accept),
            Err(LibError::NotFound(_))
        ));
        store
            .review_suggestion(&id, "texture", ReviewAction::Reject)
            .unwrap();
        assert!(matches!(
            store.review_suggestion(&id, "texture", ReviewAction::Reject),
            Err(LibError::BadRequest(_))
        ));
    }

    #[test]
    fn tag_vocabulary_escapes_prefix_wildcards_and_applies_visibility() {
        let store = Store::open_in_memory().unwrap();
        let visible_source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/visible".into(),
                },
                "visible",
                false,
            )
            .unwrap();
        let hidden_source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/hidden".into(),
                },
                "hidden",
                false,
            )
            .unwrap();
        let insert = |source_id, path: &str| {
            store
                .upsert_asset(&NewAsset {
                    source_id,
                    path: path.into(),
                    filename: path.into(),
                    content_hash: Some(ContentHash([path.as_bytes()[0]; 32])),
                    size_bytes: Some(1),
                    source_modified_at: None,
                    scanned_at: now_ms(),
                    media_type: MediaType::Image,
                    format: "png".into(),
                })
                .unwrap()
                .0
        };
        let visible = insert(visible_source, "visible.png");
        let hidden = insert(hidden_source, "hidden.png");
        store
            .edit_manual_tags(&[visible], &["100%real".into(), "100x".into()], &[], false)
            .unwrap();
        store
            .edit_manual_tags(&[hidden], &["100%secret".into()], &[], false)
            .unwrap();
        let visibility = Visibility::Restricted(VisibilityScope {
            sources: [visible_source].into_iter().collect(),
            ..VisibilityScope::default()
        });
        let tags = store.list_tags(Some("100%"), 20, &visibility).unwrap();
        assert_eq!(
            tags.len(),
            1,
            "LIKE wildcards must be escaped and hidden tags omitted"
        );
        assert_eq!(tags[0].name, "100%real");
        assert_eq!(tags[0].count, 1);
    }

    #[test]
    fn exact_duplicate_pages_use_a_bounded_keyset_cursor() {
        let store = duplicate_store(DUP_GROUP_PAGE_MAX as usize + 5, 2);
        let first = store
            .duplicates(
                &DupRequest {
                    kind: DupKind::Exact,
                    media: None,
                    limit: u32::MAX,
                    after: None,
                    review: DupReviewFilter::Pending,
                },
                &Visibility::Full,
            )
            .unwrap();
        assert_eq!(first.items.len(), DUP_GROUP_PAGE_MAX as usize);
        assert!(first.items.iter().all(|group| group.members.len() == 2));
        let cursor = first.cursor.expect("five groups remain");
        assert!(cursor.0.starts_with("exact:2:"));

        let second = store
            .duplicates(
                &DupRequest {
                    kind: DupKind::Exact,
                    media: None,
                    limit: u32::MAX,
                    after: Some(cursor),
                    review: DupReviewFilter::Pending,
                },
                &Visibility::Full,
            )
            .unwrap();
        assert_eq!(second.items.len(), 5);
        assert!(second.cursor.is_none());

        let invalid = store.duplicates(
            &DupRequest {
                kind: DupKind::Exact,
                media: None,
                limit: 10,
                after: Some(Cursor("999999999999999999999".into())),
                review: DupReviewFilter::Pending,
            },
            &Visibility::Full,
        );
        assert!(matches!(invalid, Err(LibError::BadRequest(_))));
    }

    #[test]
    fn exact_duplicate_review_covers_every_catalogued_media_type() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/all-media".into(),
                },
                "all media",
                false,
            )
            .unwrap();
        let media = [
            (MediaType::Image, "png"),
            (MediaType::Audio, "wav"),
            (MediaType::Model, "glb"),
            (MediaType::Video, "mp4"),
            (MediaType::Document, "pdf"),
        ];
        for (index, (kind, extension)) in media.into_iter().enumerate() {
            let hash = ContentHash([(index + 1) as u8; 32]);
            insert_test_asset(
                &store,
                source,
                &format!("{index}-a.{extension}"),
                kind,
                Some(hash),
            );
            insert_test_asset(
                &store,
                source,
                &format!("{index}-b.{extension}"),
                kind,
                Some(hash),
            );

            let page = store
                .duplicates(
                    &duplicate_request(DupKind::Exact, Some(kind)),
                    &Visibility::Full,
                )
                .unwrap();
            assert_eq!(page.items.len(), 1, "missing exact {kind:?} duplicate");
            assert_eq!(page.items[0].media, kind);
            assert_eq!(page.items[0].members.len(), 2);
        }
    }

    #[test]
    fn near_duplicate_review_only_compares_valid_media_space_signals() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-signals".into(),
                },
                "near signals",
                false,
            )
            .unwrap();
        let image = insert_test_asset(&store, source, "image-a.png", MediaType::Image, None);
        let document = insert_test_asset(&store, source, "document.pdf", MediaType::Document, None);
        let other_space =
            insert_test_asset(&store, source, "image-other.png", MediaType::Image, None);
        store
            .set_embedding(&image, "shared", MediaType::Image, &[1.0, 0.0], "test@1")
            .unwrap();
        store
            .set_embedding(
                &document,
                "shared",
                MediaType::Document,
                &[1.0, 0.0],
                "test@1",
            )
            .unwrap();
        store
            .set_embedding(
                &other_space,
                "other",
                MediaType::Image,
                &[1.0, 0.0],
                "test@1",
            )
            .unwrap();
        assert!(store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap()
            .items
            .is_empty());

        let matching = insert_test_asset(&store, source, "image-b.png", MediaType::Image, None);
        store
            .set_embedding(
                &matching,
                "shared",
                MediaType::Image,
                &[0.99, 0.01],
                "test@1",
            )
            .unwrap();
        let first = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        let second = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].review, second.items[0].review);
        let ids: std::collections::BTreeSet<_> = first.items[0]
            .members
            .iter()
            .map(|member| member.asset.id)
            .collect();
        assert_eq!(ids, [image, matching].into_iter().collect());
    }

    #[test]
    fn duplicate_review_state_persists_and_rejects_cross_group_targets_atomically() {
        let store = duplicate_store(2, 2);
        let pending = store
            .duplicates(&duplicate_request(DupKind::Exact, None), &Visibility::Full)
            .unwrap();
        let group = &pending.items[0];
        let keep = group.members[0].asset.id;
        let remove = group.members[1].asset.id;
        let outside = pending.items[1].members[0].asset.id;

        let failed = store.review_duplicate(&DupReviewRequest {
            review: group.review.clone(),
            state: DupReviewState::Resolved,
            keep: Some(keep),
            removals: vec![
                DupReviewRemoval {
                    asset: remove,
                    block: false,
                },
                DupReviewRemoval {
                    asset: outside,
                    block: false,
                },
            ],
        });
        assert!(matches!(failed, Err(LibError::BadRequest(_))));
        assert!(
            store.get_asset(&remove).is_ok(),
            "failed decision must roll back deletion"
        );

        store
            .review_duplicate(&DupReviewRequest {
                review: group.review.clone(),
                state: DupReviewState::Resolved,
                keep: Some(keep),
                removals: Vec::new(),
            })
            .unwrap();
        assert!(store
            .duplicates(&duplicate_request(DupKind::Exact, None), &Visibility::Full)
            .unwrap()
            .items
            .iter()
            .all(|item| item.review != group.review));
        let resolved = store
            .duplicates(
                &DupRequest {
                    review: DupReviewFilter::Resolved,
                    ..duplicate_request(DupKind::Exact, None)
                },
                &Visibility::Full,
            )
            .unwrap();
        let saved = resolved
            .items
            .iter()
            .find(|item| item.review == group.review)
            .expect("resolved group survives navigation/refetch");
        assert_eq!(saved.chosen_keep, Some(keep));

        store
            .review_duplicate(&DupReviewRequest {
                review: group.review.clone(),
                state: DupReviewState::Pending,
                keep: Some(keep),
                removals: vec![DupReviewRemoval {
                    asset: remove,
                    block: false,
                }],
            })
            .unwrap();
        assert!(matches!(
            store.get_asset(&remove),
            Err(LibError::NotFound(_))
        ));
    }

    #[test]
    fn near_review_pagination_consumes_filtered_groups_without_repeating_cursor() {
        let store = Store::open_in_memory().unwrap();
        let source = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/near-pages".into(),
                },
                "near pages",
                false,
            )
            .unwrap();
        for space in ["a", "b", "c"] {
            for member in 0..2 {
                let id = insert_test_asset(
                    &store,
                    source,
                    &format!("{space}-{member}.png"),
                    MediaType::Image,
                    None,
                );
                store
                    .set_embedding(&id, space, MediaType::Image, &[1.0, 0.0], "test@1")
                    .unwrap();
            }
        }
        let all = store
            .duplicates(&duplicate_request(DupKind::Near, None), &Visibility::Full)
            .unwrap();
        assert_eq!(all.items.len(), 3);
        for group in all.items.iter().take(2) {
            store
                .review_duplicate(&DupReviewRequest {
                    review: group.review.clone(),
                    state: DupReviewState::Resolved,
                    keep: Some(group.suggested_keep),
                    removals: Vec::new(),
                })
                .unwrap();
        }

        let page = store
            .duplicates(
                &DupRequest {
                    limit: 1,
                    ..duplicate_request(DupKind::Near, None)
                },
                &Visibility::Full,
            )
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].review, all.items[2].review);
        assert!(page.cursor.is_none(), "all raw components were consumed");
    }

    #[test]
    fn duplicate_group_hydration_has_a_hard_member_cap() {
        let store = duplicate_store(1, DUP_GROUP_MEMBER_MAX + 7);
        let id = {
            let conn = store.read().unwrap();
            conn.query_row("SELECT id FROM asset LIMIT 1", [], |row| {
                Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?))
            })
            .unwrap()
        };
        let membership = store
            .duplicate_membership(&[id], &Visibility::Full)
            .unwrap();
        let group = store
            .duplicate_group(&membership[0].asset, &Visibility::Full)
            .unwrap()
            .unwrap();
        assert_eq!(group.total_members as usize, DUP_GROUP_MEMBER_MAX + 7);
        assert_eq!(group.members.len(), DUP_GROUP_MEMBER_MAX);
        let group_key = group.group.clone().unwrap();
        let mut seen: Vec<AssetId> = group.members.iter().map(|member| member.asset.id).collect();
        let mut cursor = group.members_cursor;
        while let Some(after) = cursor {
            let page = store
                .duplicate_group_members(
                    &DupGroupMembersRequest {
                        group: group_key.clone(),
                        after: Some(after),
                        limit: 17,
                    },
                    &Visibility::Full,
                )
                .unwrap();
            seen.extend(page.items.iter().map(|member| member.asset.id));
            cursor = page.cursor;
        }
        let expected: Vec<AssetId> = {
            let conn = store.read().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM asset
                     ORDER BY COALESCE(size_bytes, 0) DESC, id ASC",
                )
                .unwrap();
            stmt.query_map([], |row| Ok(blob_to_asset_id(&row.get::<_, Vec<u8>>(0)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            seen, expected,
            "member pages must have no gaps or duplicates"
        );
        let overflowing_cursor = store.duplicate_group_members(
            &DupGroupMembersRequest {
                group: group_key,
                after: Some(Cursor(format!("members:{}:{id}", u64::MAX))),
                limit: 10,
            },
            &Visibility::Full,
        );
        assert!(matches!(overflowing_cursor, Err(LibError::BadRequest(_))));
    }

    fn planner_scale_store(asset_count: usize, due_every: usize) -> Store {
        let store = Store::open_in_memory().unwrap();
        let sources: Vec<_> = (0..4)
            .map(|index| {
                store
                    .add_source(
                        &SourceConnection::LocalFs {
                            root: format!("/planner-{index}"),
                        },
                        &format!("planner-{index}"),
                        false,
                    )
                    .unwrap()
            })
            .collect();
        let mut conn = store.write();
        let transaction = conn.transaction().unwrap();
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO asset(id,content_hash,source_id,path,filename,size_bytes,
                        scanned_at,media_type,format,analysis_version,derivative_version,
                        created_at,updated_at)
                     VALUES(?,?,?,?,?,1,0,'image','png',?,?,0,0)",
                )
                .unwrap();
            for index in 0..asset_count {
                let mut id = [0_u8; 16];
                id[8..].copy_from_slice(&(index as u64).to_be_bytes());
                let due = if index % due_every == 0 { 0_i64 } else { 3_i64 };
                insert
                    .execute(params![
                        id.to_vec(),
                        vec![(index % 251) as u8; 32],
                        sources[index % sources.len()].as_bytes().to_vec(),
                        format!("deep/{index:09}.png"),
                        format!("{index:09}.png"),
                        due,
                        due,
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        drop(conn);
        store
    }

    fn assert_scale_plan(asset_count: usize) {
        const DUE_EVERY: usize = 100;
        const PAGE: usize = 37;
        let store = planner_scale_store(asset_count, DUE_EVERY);
        let expected = asset_count.div_ceil(DUE_EVERY);
        let summary = store.analysis_plan_summary(3, false, &[]).unwrap();
        assert_eq!(summary.total as usize, expected);
        // A row arriving after the summary belongs to the next resumable plan, not this job's
        // already-recorded total. Its UUIDv7 sorts beyond the deterministic fixture ids.
        store
            .upsert_asset(&NewAsset {
                source_id: *summary.sources.last().unwrap(),
                path: "late-arrival.png".into(),
                filename: "late-arrival.png".into(),
                content_hash: Some(ContentHash([255; 32])),
                size_bytes: Some(1),
                source_modified_at: None,
                scanned_at: now_ms(),
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();

        let mut cursor = None;
        let mut planned = 0usize;
        let mut max_resident_targets = 0usize;
        let mut seen = std::collections::HashSet::new();
        loop {
            let batch = store
                .analysis_target_batch(3, false, &[], cursor, summary.end, PAGE)
                .unwrap();
            max_resident_targets = max_resident_targets.max(batch.targets.len());
            assert!(batch.targets.len() <= PAGE);
            let distinct_sources: std::collections::HashSet<_> = batch
                .targets
                .iter()
                .map(|target| target.source_id)
                .collect();
            assert_eq!(batch.sources.len(), distinct_sources.len());
            for target in batch.targets {
                assert!(seen.insert(target.id), "keyset page repeated an asset");
                planned += 1;
            }
            cursor = batch.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(planned, expected, "planner work must equal the due backlog");
        assert!(
            max_resident_targets <= PAGE,
            "planner memory grew past one page"
        );

        let query_plans = {
            let conn = store.read().unwrap();
            [
                "EXPLAIN QUERY PLAN SELECT a.id FROM asset a INDEXED BY idx_asset_derivative_pending JOIN source s ON s.id=a.source_id
                 WHERE s.kind <> 'federated' AND a.derivative_version < 1
                   AND a.media_type IN ('image','video','model')
                 ORDER BY a.derivative_version,a.source_id,a.id LIMIT 37",
                "EXPLAIN QUERY PLAN SELECT a.id FROM asset a INDEXED BY idx_asset_analysis_planner JOIN source s ON s.id=a.source_id
                 WHERE s.kind <> 'federated' AND a.analysis_version < 3
                 ORDER BY a.analysis_version,a.source_id,a.id LIMIT 37",
                "EXPLAIN QUERY PLAN SELECT a.id FROM asset a INDEXED BY idx_asset_force_planner JOIN source s ON s.id=a.source_id
                 WHERE s.kind <> 'federated'
                 ORDER BY a.source_id,a.id LIMIT 37",
            ]
            .map(|sql| {
                let mut statement = conn.prepare(sql).unwrap();
                statement
                    .query_map([], |row| row.get::<_, String>(3))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .join(" | ")
            })
        };
        assert!(
            query_plans[0].contains("idx_asset_derivative_pending"),
            "derivative backlog did not use its index: {}",
            query_plans[0]
        );
        assert!(
            query_plans[1].contains("idx_asset_analysis_planner"),
            "analysis backlog did not use its index: {}",
            query_plans[1]
        );
        assert!(
            query_plans[2].contains("idx_asset_force_planner"),
            "force planner did not use its index: {}",
            query_plans[2]
        );
        assert!(
            query_plans
                .iter()
                .all(|plan| !plan.contains("USE TEMP B-TREE")),
            "planner required a backlog-sized temporary ordering: {query_plans:?}"
        );
    }

    #[test]
    fn planner_memory_is_flat_and_work_tracks_20k_backlog() {
        assert_scale_plan(20_000);
    }

    #[test]
    #[ignore = "million-row planner scale fixture"]
    fn planner_memory_is_flat_on_one_million_assets() {
        assert_scale_plan(1_000_000);
    }

    /// Manual scale benchmark for issue #142. Even with ten thousand groups the returned page and
    /// hydrated summaries remain fixed-size; run with `cargo test -p dam-store -- --ignored`.
    #[test]
    #[ignore = "large-catalog duplicate benchmark"]
    fn duplicate_page_large_catalog_benchmark() {
        let store = duplicate_store(10_000, 2);
        let started = std::time::Instant::now();
        let page = store
            .duplicates(&DupRequest::default(), &Visibility::Full)
            .unwrap();
        eprintln!("10k duplicate groups: {:?}", started.elapsed());
        assert_eq!(page.items.len(), 24);
        assert!(page.cursor.is_some());
        assert!(
            page.items
                .iter()
                .map(|group| group.members.len())
                .sum::<usize>()
                <= 24 * DUP_GROUP_MEMBER_MAX
        );
    }

    fn sftp_conn() -> SourceConnection {
        SourceConnection::Sftp(SftpConfig {
            host: "example.invalid".into(),
            port: 22,
            username: "u".into(),
            base_path: "/assets".into(),
            password: Some("p".into()),
            private_key: None,
            passphrase: None,
            credential_ref: None,
        })
    }

    /// Add one source and one asset on it; return the store and the asset id.
    fn store_with_asset_on(conn: &SourceConnection, kind_name: &str) -> (Store, AssetId) {
        let store = Store::open_in_memory().unwrap();
        let mut stored = conn.clone();
        let _ = stored.take_credentials();
        let src = store.add_source(&stored, kind_name, false).unwrap();
        let (id, _) = store
            .upsert_asset(&NewAsset {
                source_id: src,
                path: "textures/brick.png".into(),
                filename: "brick.png".into(),
                content_hash: None,
                size_bytes: Some(1),
                source_modified_at: None,
                scanned_at: now_ms(),
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        (store, id)
    }

    /// The bug behind issue #48: `WHERE s.kind = 'local_fs'` meant an SFTP/SMB asset was never even
    /// *planned* for analysis. It produced no error, because nothing failed — the asset simply was
    /// not on the list, so it sat at `analysis_version = 0` forever with no embedding, no derived
    /// signals, and no auto-tags, while the job it should have been part of reported success.
    #[test]
    fn remote_assets_are_planned_for_analysis() {
        let (store, id) = store_with_asset_on(&sftp_conn(), "remote");
        let targets = store.list_analysis_targets(1, false, &[]).unwrap();
        assert_eq!(targets.len(), 1, "an SFTP asset must be an analysis target");
        assert_eq!(targets[0].id, id);
        assert!(
            matches!(targets[0].connection, SourceConnection::Sftp(_)),
            "the target carries the connection the runner fetches through"
        );
    }

    #[test]
    fn derivative_backlog_advances_and_content_changes_reopen_it() {
        let (store, id) = store_with_asset_on(
            &SourceConnection::LocalFs {
                root: "/derivative".into(),
            },
            "derivative",
        );
        let source = store.asset_source(&id).unwrap().unwrap();
        store
            .upsert_asset(&NewAsset {
                source_id: source,
                path: "textures/brick.png".into(),
                filename: "brick.png".into(),
                content_hash: Some(ContentHash([1; 32])),
                size_bytes: Some(1),
                source_modified_at: Some(1),
                scanned_at: now_ms(),
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        let first = store.derivative_target_batch(1, None, 10).unwrap();
        assert_eq!(first.targets.len(), 1);
        assert!(!store
            .mark_derivative_ready(&id, 1, Some(ContentHash([2; 32])))
            .unwrap());
        assert_eq!(
            store
                .derivative_target_batch(1, None, 10)
                .unwrap()
                .targets
                .len(),
            1,
            "a stale render incorrectly cleared newer content"
        );
        store
            .mark_derivative_ready(&id, 1, Some(ContentHash([1; 32])))
            .unwrap();
        assert!(
            store
                .derivative_target_batch(1, None, 10)
                .unwrap()
                .targets
                .is_empty(),
            "successful warm did not clear the backlog"
        );
        store
            .upsert_asset(&NewAsset {
                source_id: source,
                path: "textures/brick.png".into(),
                filename: "brick.png".into(),
                content_hash: Some(ContentHash([7; 32])),
                size_bytes: Some(2),
                source_modified_at: Some(2),
                scanned_at: now_ms(),
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        assert_eq!(
            store
                .derivative_target_batch(1, None, 10)
                .unwrap()
                .targets
                .len(),
            1,
            "content change did not reopen derivative work"
        );
        assert!(store
            .mark_derivative_ready(&id, 1, Some(ContentHash([7; 32])))
            .unwrap());
        store.mark_derivatives_pending(&[id]).unwrap();
        assert_eq!(
            store
                .derivative_target_batch(1, None, 10)
                .unwrap()
                .targets
                .len(),
            1,
            "explicit thumbnail reset did not reopen work"
        );
        assert!(store
            .mark_derivative_ready(&id, 1, Some(ContentHash([7; 32])))
            .unwrap());
        store.mark_all_derivatives_pending().unwrap();
        assert_eq!(
            store
                .derivative_target_batch(1, None, 10)
                .unwrap()
                .targets
                .len(),
            1,
            "cache-wide reset did not reopen work"
        );
    }

    /// The one exclusion that must survive: a federated peer yields catalog rows, not bytes. There
    /// is nothing local to decode, and its derived data belongs to the peer that owns it — so
    /// widening the filter for SFTP/SMB must not accidentally sweep peers back in.
    #[test]
    fn federated_assets_are_never_analysis_targets() {
        let (store, _) = store_with_asset_on(
            &SourceConnection::Federated(FederatedConfig {
                endpoint: "http://peer.invalid:7878".into(),
                token: None,
                credential_ref: None,
            }),
            "peer",
        );
        assert!(
            store
                .list_analysis_targets(1, false, &[])
                .unwrap()
                .is_empty(),
            "a federated asset has no local bytes and must not be planned"
        );
    }

    /// A row whose stored connection blob cannot be parsed is dropped, not defaulted. A target that
    /// can never be fetched would fail every pass forever; leaving it off the list is the honest
    /// outcome, and keeps the warning count meaningful.
    #[test]
    fn an_unparseable_connection_drops_the_target() {
        let (store, _) = store_with_asset_on(&sftp_conn(), "remote");
        {
            let conn = store.write();
            conn.execute("UPDATE source SET connection = 'not json'", [])
                .unwrap();
        }
        assert!(
            store
                .list_analysis_targets(1, false, &[])
                .unwrap()
                .is_empty(),
            "an unfetchable target must not be planned"
        );
    }
}
