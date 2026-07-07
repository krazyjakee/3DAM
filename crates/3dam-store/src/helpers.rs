//! Shared free helpers for the `Store` submodules: SQL filter building, blob↔id
//! conversions, cursor decoding, and small row-attribute shaping.
use super::*;

/// Deserialize a persisted `source.connection` blob into the typed connection model.
pub(crate) fn parse_connection(blob: &str) -> Result<SourceConnection, LibError> {
    serde_json::from_str(blob)
        .map_err(|e| LibError::Internal(format!("corrupt source connection: {e}")))
}

/// The SELECT column list every grid/summary query shares — thirteen columns in the exact order
/// `row_to_summary` reads them. Callers append their own `FROM …`, `{ATTR_JOINS}`, WHERE and ORDER.
pub(crate) const GRID_SELECT: &str = "SELECT asset.id, filename, media_type, format,
        size_bytes + COALESCE(model_attr.dependency_bytes, 0), license_id, license_status,
        image_attr.width, image_attr.height, audio_attr.duration_ms, model_attr.triangle_count,
        audio_attr.class, asset.flags";

/// The per-media attribute LEFT JOINs the grid select depends on (dimensions / duration / tris).
pub(crate) const ATTR_JOINS: &str = "LEFT JOIN image_attr ON image_attr.asset_id = asset.id
         LEFT JOIN audio_attr ON audio_attr.asset_id = asset.id
         LEFT JOIN model_attr ON model_attr.asset_id = asset.id";

/// Row → `AssetSummary` mapper for the [`GRID_SELECT`] column shape. Shared by the paged query,
/// collection listing, and similarity/dedup summary fetch so the column contract lives in one place.
pub(crate) fn row_to_summary(r: &rusqlite::Row) -> rusqlite::Result<AssetSummary> {
    let id = blob_to_asset_id(&r.get::<_, Vec<u8>>(0)?);
    let media = MediaType::parse(&r.get::<_, String>(2)?).unwrap_or(MediaType::Image);
    let width: Option<i64> = r.get(7)?;
    let height: Option<i64> = r.get(8)?;
    let duration_ms: Option<i64> = r.get(9)?;
    let tri_count: Option<i64> = r.get(10)?;
    let audio_class: Option<String> = r.get(11)?;
    // Favourite is bit 1 of the asset `flags` bitset (bit 0 is the scan-derived "missing" mark).
    let flags: i64 = r.get(12)?;
    Ok(AssetSummary {
        id,
        name: r.get(1)?,
        media,
        format: r.get(3)?,
        size: r.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
        license: LicenseBadge {
            id: r.get(5)?,
            status: LicenseStatus::parse(&r.get::<_, String>(6)?),
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
        favorite: flags & FAVORITE_FLAG != 0,
    })
}

/// Asset `flags` bit reserved for the user favourite mark (issue #63). Bit 0 (`1`) is the
/// scan-derived "missing" mark; this is bit 1 so the two never collide, and the favourite survives
/// the re-scan upserts that clear/set bit 0.
pub(crate) const FAVORITE_FLAG: i64 = 2;

/// The couple of cheap per-media attributes shown on a grid tile / table row: dimensions for
/// images, duration (+ a `loop` marker when the analysis classed it so) for audio, triangle count
/// for models. Cheap and best-effort.
pub(crate) fn grid_key_attrs(
    media: MediaType,
    width: Option<i64>,
    height: Option<i64>,
    duration_ms: Option<i64>,
    tri_count: Option<i64>,
    audio_class: Option<&str>,
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
            // The DSP classifier (analysis §4.2) labels audio one_shot | loop | music | sfx — surface
            // it so the table/grid can show the type alongside the duration.
            if let Some(class) = audio_class.filter(|c| !c.is_empty()) {
                m.insert("type".into(), class.replace('_', "-"));
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

pub(crate) fn blob_to_asset_id(b: &[u8]) -> AssetId {
    AssetId(uuid_from_slice(b))
}
pub(crate) fn blob_to_source_id(b: &[u8]) -> SourceId {
    SourceId(uuid_from_slice(b))
}
pub(crate) fn blob_to_job_id(b: &[u8]) -> JobId {
    JobId(uuid_from_slice(b))
}
pub(crate) fn uuid_from_slice(b: &[u8]) -> Uuid {
    <[u8; 16]>::try_from(b)
        .map(Uuid::from_bytes)
        .unwrap_or(Uuid::nil())
}

pub(crate) fn decode_offset(c: Option<&Cursor>) -> Result<usize, LibError> {
    match c {
        None => Ok(0),
        Some(Cursor(s)) => s
            .parse::<usize>()
            .map_err(|_| LibError::BadRequest("invalid cursor".into())),
    }
}

pub(crate) fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Build the shared `WHERE` clause (FTS text match + facet filters) and its bind values from a query
/// request — the common prefix of both `query_assets` (paged) and `query_asset_ids` (unbounded).
///
/// Text search hits the `asset_fts` inverted index (M1), widened by the synonym map (M3), expressed
/// as a composable `asset.rowid IN (…)` subquery so it drops into both the JOINed page query and the
/// bare `COUNT(*) FROM asset`. A `filename LIKE` OR-arm is kept so in-word substrings the tokenizer
/// can't reach (e.g. a partial `k47`) never regress below the old scan's recall.
pub(crate) fn build_where(
    req: &QueryRequest,
    syn: &crate::search::SynonymMap,
) -> Result<(String, Vec<Value>), LibError> {
    let mut where_sql = String::from(" WHERE 1=1");
    let mut binds: Vec<Value> = Vec::new();
    if let Some(text) = req.text.as_ref().filter(|t| !t.is_empty()) {
        if let Some(m) = crate::search::fts_match_expr(text, syn) {
            where_sql.push_str(
                " AND (asset.rowid IN (SELECT rowid FROM asset_fts WHERE asset_fts MATCH ?) \
                 OR filename LIKE ?)",
            );
            binds.push(Value::Text(m));
            binds.push(Value::Text(format!("%{}%", escape_like(text))));
        } else {
            // No usable FTS token (all punctuation) — fall back to the plain substring scan.
            where_sql.push_str(" AND filename LIKE ?");
            binds.push(Value::Text(format!("%{}%", escape_like(text))));
        }
    }
    for f in &req.filters {
        apply_filter(f, &mut where_sql, &mut binds)?;
    }
    Ok((where_sql, binds))
}

pub(crate) fn apply_filter(
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
        // `license_status` lives on the asset row itself, so it filters inline like media/format.
        License => {
            eq_or_in(f, "license_status", where_sql, binds)?;
        }
        // A usage right is a granted-permission flag (`rights_*` = 1) on the asset row. The value
        // names which right; presence of the filter means "must be granted".
        UsageRight => {
            let col = match &f.value {
                FilterValue::Str(s) => match s.as_str() {
                    "commercial" => "rights_commercial",
                    "modify" => "rights_modify",
                    "redistribute" => "rights_redistribute",
                    "attribution" => "rights_attribution",
                    _ => return Err(LibError::BadRequest(format!("unknown usage right {s:?}"))),
                },
                _ => {
                    return Err(LibError::BadRequest(
                        "usage_right filter wants a right name string".into(),
                    ))
                }
            };
            where_sql.push_str(&format!(" AND {col} = 1"));
        }
        // Tag/attr facets live in side tables. Express them as correlated subqueries on `asset.id`
        // rather than relying on the JOINs `query_assets` adds — the COUNT(*) query filters bare
        // `FROM asset`, so a joined-column reference there would fail to resolve.
        Tag => match (&f.op, &f.value) {
            (FilterOp::Eq | FilterOp::Contains, FilterValue::Str(s)) => {
                where_sql.push_str(
                    " AND asset.id IN (SELECT at.asset_id FROM asset_tag at \
                     JOIN tag t ON t.id = at.tag_id \
                     WHERE t.name = ? COLLATE NOCASE AND at.state <> 'rejected')",
                );
                binds.push(Value::Text(s.clone()));
            }
            (FilterOp::In, FilterValue::List(items)) => {
                let placeholders = items.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                where_sql.push_str(&format!(
                    " AND asset.id IN (SELECT at.asset_id FROM asset_tag at \
                     JOIN tag t ON t.id = at.tag_id \
                     WHERE t.name COLLATE NOCASE IN ({placeholders}) AND at.state <> 'rejected')"
                ));
                for it in items {
                    if let FilterValue::Str(s) = it {
                        binds.push(Value::Text(s.clone()));
                    } else {
                        return Err(LibError::BadRequest("tag IN list wants strings".into()));
                    }
                }
            }
            _ => return Err(LibError::BadRequest("unsupported tag filter".into())),
        },
        Width => attr_num_filter(f, "image_attr", "width", where_sql, binds)?,
        Height => attr_num_filter(f, "image_attr", "height", where_sql, binds)?,
        Bpm => attr_num_filter(f, "audio_attr", "bpm", where_sql, binds)?,
        TriCount => attr_num_filter(f, "model_attr", "triangle_count", where_sql, binds)?,
        // A boolean flag on the asset row itself — presence of the filter means "favourites only".
        // `Eq false` inverts it (everything not favourited), which keeps the op meaningful.
        Favorite => {
            let want = !matches!(f.value, FilterValue::Bool(false));
            let test = if want { "!= 0" } else { "= 0" };
            where_sql.push_str(&format!(" AND (flags & {FAVORITE_FLAG}) {test}"));
        }
    }
    Ok(())
}

/// A numeric comparison against a per-media attribute column (`image_attr.width`,
/// `audio_attr.bpm`, …), emitted as a correlated `asset.id IN (…)` subquery so it composes with the
/// bare-`FROM asset` COUNT(*) query as well as the JOINed page query. Values bind as REAL — SQLite's
/// numeric comparison treats `100 = 100.0` as equal, so integer columns compare correctly too.
fn attr_num_filter(
    f: &Filter,
    table: &str,
    col: &str,
    where_sql: &mut String,
    binds: &mut Vec<Value>,
) -> Result<(), LibError> {
    let cond = match (&f.op, &f.value) {
        (FilterOp::Eq, FilterValue::Num(n)) => {
            binds.push(Value::Real(*n));
            format!("{col} = ?")
        }
        (FilterOp::Gt, FilterValue::Num(n)) => {
            binds.push(Value::Real(*n));
            format!("{col} > ?")
        }
        (FilterOp::Gte, FilterValue::Num(n)) => {
            binds.push(Value::Real(*n));
            format!("{col} >= ?")
        }
        (FilterOp::Lt, FilterValue::Num(n)) => {
            binds.push(Value::Real(*n));
            format!("{col} < ?")
        }
        (FilterOp::Lte, FilterValue::Num(n)) => {
            binds.push(Value::Real(*n));
            format!("{col} <= ?")
        }
        (FilterOp::Range, FilterValue::Range(lo, hi)) => {
            binds.push(Value::Real(*lo));
            binds.push(Value::Real(*hi));
            format!("{col} BETWEEN ? AND ?")
        }
        _ => return Err(LibError::BadRequest(format!("unsupported filter on {col}"))),
    };
    where_sql.push_str(&format!(
        " AND asset.id IN (SELECT asset_id FROM {table} WHERE {cond})"
    ));
    Ok(())
}

pub(crate) fn eq_or_in(
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

pub(crate) fn push_cmp(
    where_sql: &mut String,
    binds: &mut Vec<Value>,
    col: &str,
    op: &str,
    n: f64,
) {
    where_sql.push_str(&format!(" AND {col} {op} ?"));
    binds.push(Value::Integer(n as i64));
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::dto::{FacetField, Filter, FilterOp, FilterValue, QueryRequest};

    /// A well-formed filter for every `FacetField` variant. If a variant is added without a query
    /// arm, the exhaustive `match` below stops compiling — a nudge to wire it before the API can
    /// express a filter the store can't run (issue #37).
    fn representative(field: FacetField) -> Filter {
        use FacetField::*;
        let (op, value) = match field {
            MediaType => (FilterOp::Eq, FilterValue::Str("image".into())),
            Format => (FilterOp::Eq, FilterValue::Str("png".into())),
            Source => (
                FilterOp::Eq,
                FilterValue::Str("00000000-0000-0000-0000-000000000000".into()),
            ),
            Tag => (FilterOp::Eq, FilterValue::Str("brick".into())),
            SizeBytes => (FilterOp::Gt, FilterValue::Num(1024.0)),
            License => (FilterOp::Eq, FilterValue::Str("permissive".into())),
            UsageRight => (FilterOp::Eq, FilterValue::Str("commercial".into())),
            Width => (FilterOp::Gte, FilterValue::Num(512.0)),
            Height => (FilterOp::Lte, FilterValue::Num(512.0)),
            Bpm => (FilterOp::Range, FilterValue::Range(90.0, 130.0)),
            TriCount => (FilterOp::Lt, FilterValue::Num(50_000.0)),
            Favorite => (FilterOp::Eq, FilterValue::Bool(true)),
        };
        Filter { field, op, value }
    }

    /// Acceptance for #37: no `FacetField` variant reachable from the API is `Unsupported`, and each
    /// builds a WHERE clause that a live SQLite catalog accepts.
    #[test]
    fn every_facet_field_builds_and_runs() {
        let fields = [
            FacetField::MediaType,
            FacetField::Format,
            FacetField::Source,
            FacetField::Tag,
            FacetField::SizeBytes,
            FacetField::License,
            FacetField::UsageRight,
            FacetField::Width,
            FacetField::Height,
            FacetField::Bpm,
            FacetField::TriCount,
            FacetField::Favorite,
        ];

        // The schema the store runs against — enough for SQLite to plan each filter's subquery.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for step in crate::schema::MIGRATIONS {
            conn.execute_batch(step).unwrap();
        }

        for field in fields {
            let req = QueryRequest {
                filters: vec![representative(field)],
                ..Default::default()
            };
            let syn = crate::search::SynonymMap::default();
            let (where_sql, binds) = match build_where(&req, &syn) {
                Ok(v) => v,
                Err(e) => panic!("filter on {field:?} failed to build: {e:?}"),
            };
            assert!(
                !matches!(build_where(&req, &syn), Err(LibError::Unsupported(_))),
                "filter on {field:?} is Unsupported"
            );
            // Prove the SQL is executable against the real schema (both the JOINed page query and
            // the bare COUNT(*) query must accept it).
            let count_sql = format!("SELECT COUNT(*) FROM asset{where_sql}");
            conn.query_row(&count_sql, rusqlite::params_from_iter(binds.iter()), |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or_else(|e| panic!("count with {field:?} filter failed: {e}"));
        }
    }
}
