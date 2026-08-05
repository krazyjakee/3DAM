//! Planning an analysis or derivative pass: which catalog rows are due, and how a runner walks that
//! backlog in stable, resumable, bounded pages (tech-spec 05 §1.2/§7.2). Part of the `Store` impl.
use super::*;
use crate::helpers::*;
use std::collections::BTreeMap;

/// Planner pages stay small enough for flat memory and prompt cancellation while amortizing the
/// SQLite query and producer/consumer hand-off.
pub const ANALYSIS_PLAN_BATCH_MAX: usize = 512;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_sources::{FederatedConfig, SftpConfig, SourceConnection};

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
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
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
