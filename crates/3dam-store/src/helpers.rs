//! Shared free helpers for the `Store` submodules: SQL filter building, blob↔id
//! conversions, cursor decoding, and small row-attribute shaping.
use super::*;

/// Deserialize a persisted `source.connection` blob into the typed connection model.
pub(crate) fn parse_connection(blob: &str) -> Result<SourceConnection, LibError> {
    serde_json::from_str(blob)
        .map_err(|e| LibError::Internal(format!("corrupt source connection: {e}")))
}

/// images, duration for audio, triangle count for models. Cheap and best-effort.
pub(crate) fn grid_key_attrs(
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

/// Build the shared `WHERE` clause (text `LIKE` + facet filters) and its bind values from a query
/// request — the common prefix of both `query_assets` (paged) and `query_asset_ids` (unbounded).
pub(crate) fn build_where(req: &QueryRequest) -> Result<(String, Vec<Value>), LibError> {
    let mut where_sql = String::from(" WHERE 1=1");
    let mut binds: Vec<Value> = Vec::new();
    if let Some(text) = req.text.as_ref().filter(|t| !t.is_empty()) {
        where_sql.push_str(" AND filename LIKE ?");
        binds.push(Value::Text(format!("%{}%", escape_like(text))));
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
        other => {
            return Err(LibError::Unsupported(format!(
                "filter on {other:?} is not implemented in this build"
            )));
        }
    }
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
