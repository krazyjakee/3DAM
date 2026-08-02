//! Shared free helpers for the `Store` submodules: SQL filter building, blob↔id
//! conversions, cursor decoding, and small row-attribute shaping.
use super::*;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Deserialize a persisted `source.connection` blob into the typed connection model.
pub(crate) fn parse_connection(blob: &str) -> Result<SourceConnection, LibError> {
    serde_json::from_str(blob)
        .map_err(|e| LibError::Internal(format!("corrupt source connection: {e}")))
}

/// The SELECT column list every grid/summary query shares — sixteen columns in the exact order
/// `row_to_summary` reads them. Callers append their own `FROM …`, `{ATTR_JOINS}`, WHERE and ORDER.
///
/// Video reuses the dimension and duration slots via `COALESCE` rather than claiming its own
/// columns: a video's `1920×1080` and `1:30` mean exactly what an image's and an audio clip's do,
/// and only one of the joined rows can be non-NULL for a given asset (media type is exclusive).
pub(crate) const GRID_SELECT: &str = "SELECT asset.id, filename, media_type, format,
        browse_size_bytes, license_id, license_status,
        COALESCE(image_attr.width, video_attr.width),
        COALESCE(image_attr.height, video_attr.height),
        COALESCE(audio_attr.duration_ms, video_attr.duration_ms),
        model_attr.triangle_count, audio_attr.class, asset.flags,
        document_attr.page_count, document_attr.word_count, asset.source_id";

/// The attribute table a media type owns. The tables are exclusive — an asset has a row in exactly
/// one of them — which is the invariant [`GRID_SELECT`] `COALESCE`s on, so anything that changes an
/// asset's media type has to clear the row it left behind.
pub(crate) fn attr_table(media: MediaType) -> &'static str {
    match media {
        MediaType::Audio => "audio_attr",
        MediaType::Image => "image_attr",
        MediaType::Model => "model_attr",
        MediaType::Video => "video_attr",
        MediaType::Document => "document_attr",
    }
}

/// The per-media attribute LEFT JOINs the grid select depends on (dimensions / duration / tris /
/// page count).
pub(crate) const ATTR_JOINS: &str = "LEFT JOIN image_attr ON image_attr.asset_id = asset.id
         LEFT JOIN audio_attr ON audio_attr.asset_id = asset.id
         LEFT JOIN model_attr ON model_attr.asset_id = asset.id
         LEFT JOIN video_attr ON video_attr.asset_id = asset.id
         LEFT JOIN document_attr ON document_attr.asset_id = asset.id";

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
    let page_count: Option<i64> = r.get(13)?;
    let word_count: Option<i64> = r.get(14)?;
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
        key_attrs: grid_key_attrs(GridKeyAttrs {
            media,
            width,
            height,
            duration_ms,
            tri_count,
            audio_class: audio_class.as_deref(),
            page_count,
            word_count,
        }),
        favorite: flags & FAVORITE_FLAG != 0,
        source_id: Some(blob_to_source_id(&r.get::<_, Vec<u8>>(15)?)),
    })
}

/// Asset `flags` bit reserved for the user favourite mark (issue #63). Bit 0 (`1`) is the
/// scan-derived "missing" mark; this is bit 1 so the two never collide, and the favourite survives
/// the re-scan upserts that clear/set bit 0.
pub(crate) const FAVORITE_FLAG: i64 = 2;

/// The row shape [`grid_key_attrs`] reads. A struct rather than positional arguments because the
/// list crossed the point where `(media, width, height, duration_ms, tri_count, …)` at a call site
/// says nothing about which `Option<i64>` is which.
pub(crate) struct GridKeyAttrs<'a> {
    pub media: MediaType,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub tri_count: Option<i64>,
    pub audio_class: Option<&'a str>,
    pub page_count: Option<i64>,
    pub word_count: Option<i64>,
}

/// Format a millisecond duration as `m:ss` for a grid tile / table row.
fn fmt_duration(ms: i64) -> String {
    let secs = ms as f64 / 1000.0;
    format!("{:.0}:{:02}", (secs / 60.0).floor(), (secs % 60.0) as i64)
}

/// The couple of cheap per-media attributes shown on a grid tile / table row: dimensions for
/// images, duration (+ a `loop` marker when the analysis classed it so) for audio, triangle count
/// for models, both dimensions and duration for video, page/word count for documents. Cheap and
/// best-effort.
pub(crate) fn grid_key_attrs(a: GridKeyAttrs<'_>) -> SmallMap {
    let mut m = SmallMap::new();
    match a.media {
        MediaType::Image => {
            if let (Some(w), Some(h)) = (a.width, a.height) {
                m.insert("dimensions".into(), format!("{w}×{h}"));
            }
        }
        MediaType::Audio => {
            if let Some(ms) = a.duration_ms {
                m.insert("duration".into(), fmt_duration(ms));
            }
            // The DSP classifier (analysis §4.2) labels audio one_shot | loop | music | sfx — surface
            // it so the table/grid can show the type alongside the duration.
            if let Some(class) = a.audio_class.filter(|c| !c.is_empty()) {
                m.insert("type".into(), class.replace('_', "-"));
            }
        }
        MediaType::Model => {
            if let Some(t) = a.tri_count {
                m.insert("tris".into(), t.to_string());
            }
        }
        // Video is the one type that earns both slots: resolution and running time are each the
        // first thing you want to know, and neither implies the other.
        MediaType::Video => {
            if let (Some(w), Some(h)) = (a.width, a.height) {
                m.insert("dimensions".into(), format!("{w}×{h}"));
            }
            if let Some(ms) = a.duration_ms {
                m.insert("duration".into(), fmt_duration(ms));
            }
        }
        // Page count is meaningless for plaintext (there are no pages), so it only shows when the
        // container actually paginates; word count is the honest fallback for everything else.
        MediaType::Document => {
            if let Some(p) = a.page_count.filter(|p| *p > 0) {
                m.insert("pages".into(), p.to_string());
            }
            if let Some(w) = a.word_count.filter(|w| *w > 0) {
                m.insert("words".into(), w.to_string());
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

const BROWSE_CURSOR_PREFIX: &str = "b1.";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum BrowseKey {
    Text(String),
    Integer(Option<i64>),
    Relevance {
        tier: i64,
        rank: f64,
        length: i64,
        name: String,
    },
    Hybrid {
        score: f64,
        name: String,
    },
}

#[derive(Serialize, Deserialize)]
struct BrowseCursor {
    version: u8,
    order: BrowseOrder,
    key: BrowseKey,
    id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum BrowseOrder {
    Field {
        field: SortField,
        direction: SortDir,
    },
    /// Hybrid and semantic search have one effective order regardless of the display-sort fields
    /// in the request: fused score descending, then filename and asset id ascending.
    Hybrid,
}

impl BrowseOrder {
    pub(crate) fn field(field: SortField, direction: SortDir) -> Self {
        Self::Field { field, direction }
    }
}

pub(crate) fn encode_browse_cursor(
    order: BrowseOrder,
    key: BrowseKey,
    id: AssetId,
) -> Result<Cursor, LibError> {
    let payload = serde_json::to_vec(&BrowseCursor {
        version: 1,
        order,
        key,
        id: id.to_string(),
    })
    .map_err(internal)?;
    Ok(Cursor(format!(
        "{BROWSE_CURSOR_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    )))
}

pub(crate) fn decode_browse_cursor(
    cursor: Option<&Cursor>,
    order: BrowseOrder,
) -> Result<Option<(BrowseKey, AssetId)>, LibError> {
    let Some(Cursor(raw)) = cursor else {
        return Ok(None);
    };
    if raw.len() > 4_096 {
        return Err(LibError::BadRequest("browse cursor is too large".into()));
    }
    let encoded = raw
        .strip_prefix(BROWSE_CURSOR_PREFIX)
        .ok_or_else(|| LibError::BadRequest("invalid browse cursor version".into()))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| LibError::BadRequest("invalid browse cursor encoding".into()))?;
    if bytes.len() > 3_072 {
        return Err(LibError::BadRequest(
            "browse cursor payload is too large".into(),
        ));
    }
    let decoded: BrowseCursor = serde_json::from_slice(&bytes)
        .map_err(|_| LibError::BadRequest("invalid browse cursor payload".into()))?;
    if decoded.version != 1 {
        return Err(LibError::BadRequest(
            "unsupported browse cursor version".into(),
        ));
    }
    if decoded.order != order {
        return Err(LibError::BadRequest(
            "browse cursor does not match the active sort".into(),
        ));
    }
    let id: AssetId = decoded
        .id
        .parse()
        .map_err(|_| LibError::BadRequest("invalid browse cursor asset id".into()))?;
    if decoded.id != id.to_string() {
        return Err(LibError::BadRequest(
            "browse cursor asset id is not canonical".into(),
        ));
    }
    let key_matches = matches!(
        (&order, &decoded.key),
        (
            BrowseOrder::Field {
                field: SortField::Name,
                ..
            },
            BrowseKey::Text(_),
        ) | (
            BrowseOrder::Field {
                field: SortField::Size | SortField::Scanned,
                ..
            },
            BrowseKey::Integer(_),
        ) | (
            BrowseOrder::Field {
                field: SortField::Relevance,
                ..
            },
            BrowseKey::Text(_) | BrowseKey::Relevance { .. },
        ) | (BrowseOrder::Hybrid, BrowseKey::Hybrid { .. })
    );
    if !key_matches {
        return Err(LibError::BadRequest(
            "browse cursor key does not match the declared sort field".into(),
        ));
    }
    let finite = match &decoded.key {
        BrowseKey::Relevance { rank, .. } => rank.is_finite(),
        BrowseKey::Hybrid { score, .. } => score.is_finite(),
        _ => true,
    };
    if !finite {
        return Err(LibError::BadRequest(
            "browse cursor contains a non-finite rank".into(),
        ));
    }
    Ok(Some((decoded.key, id)))
}

/// Legacy numeric cursor for non-browse bounded histories (currently job history). Browse queries
/// never call this seam and reject numeric cursors in [`decode_browse_cursor`].
pub(crate) fn decode_offset(c: Option<&Cursor>) -> Result<usize, LibError> {
    match c {
        None => Ok(0),
        Some(Cursor(value)) => value
            .parse()
            .map_err(|_| LibError::BadRequest("invalid offset cursor".into())),
    }
}

pub(crate) fn push_keyset_predicate(
    where_sql: &mut String,
    binds: &mut Vec<Value>,
    expression: &str,
    key: Option<Value>,
    id: AssetId,
    direction: SortDir,
    nullable: bool,
) {
    let comparison = match direction {
        SortDir::Asc => ">",
        SortDir::Desc => "<",
    };
    match key {
        Some(key) => {
            where_sql.push_str(&format!(
                " AND (({expression}, asset.id) {comparison} (?, ?)"
            ));
            binds.push(key);
            binds.push(Value::Blob(id.as_bytes().to_vec()));
            if direction == SortDir::Desc && nullable {
                // SQLite sorts NULL first ascending and last descending.
                where_sql.push_str(&format!(" OR {expression} IS NULL"));
            }
            where_sql.push(')');
        }
        None if direction == SortDir::Asc => {
            where_sql.push_str(&format!(
                " AND (({expression} IS NULL AND asset.id > ?) OR {expression} IS NOT NULL)"
            ));
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
        None => {
            where_sql.push_str(&format!(" AND {expression} IS NULL AND asset.id < ?"));
            binds.push(Value::Blob(id.as_bytes().to_vec()));
        }
    }
}

pub(crate) struct RelevancePosition<'a> {
    pub(crate) tier: i64,
    pub(crate) rank: f64,
    pub(crate) length: i64,
    pub(crate) name: &'a str,
    pub(crate) id: AssetId,
}

pub(crate) fn push_relevance_keyset(
    where_sql: &mut String,
    binds: &mut Vec<Value>,
    position: RelevancePosition<'_>,
    direction: SortDir,
) {
    let comparison = if direction == SortDir::Asc { ">" } else { "<" };
    where_sql.push_str(&format!(
        " AND (text_matches.tier, text_matches.rank, LENGTH(filename), filename, asset.id) \
         {comparison} (?, ?, ?, ?, ?)"
    ));
    binds.push(Value::Integer(position.tier));
    binds.push(Value::Real(position.rank));
    binds.push(Value::Integer(position.length));
    binds.push(Value::Text(position.name.to_string()));
    binds.push(Value::Blob(position.id.as_bytes().to_vec()));
}

pub(crate) fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Append the visibility ceiling (tech-spec 10 §4.3, issue #42) as a WHERE predicate: the asset
/// must live in a readable source **or** be a member of a readable (shared, manual) collection.
/// `alias` names the asset table in the enclosing statement (`"asset"` or `"a"`). `Full` appends
/// nothing; an empty reachable set appends `0=1` so the result is honestly empty, not unfiltered.
/// This is *the* enforcement point — every read query composes it, so no handler can forget it.
pub(crate) fn push_visibility(
    vis: &Visibility,
    alias: &str,
    where_sql: &mut String,
    binds: &mut Vec<Value>,
) {
    let Some(scope) = vis.restricted() else {
        return;
    };
    if scope.sources.is_empty() && scope.collections.is_empty() {
        where_sql.push_str(" AND 0=1");
        return;
    }
    let mut arms: Vec<String> = Vec::new();
    if !scope.sources.is_empty() {
        let ph = scope
            .sources
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        arms.push(format!("{alias}.source_id IN ({ph})"));
        for s in &scope.sources {
            binds.push(Value::Blob(s.as_bytes().to_vec()));
        }
    }
    if !scope.collections.is_empty() {
        let ph = scope
            .collections
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        arms.push(format!(
            "{alias}.id IN (SELECT asset_id FROM collection_member WHERE collection_id IN ({ph}))"
        ));
        for c in &scope.collections {
            binds.push(Value::Blob(c.as_bytes().to_vec()));
        }
    }
    where_sql.push_str(&format!(" AND ({})", arms.join(" OR ")));
}

/// Build the shared `WHERE` clause (FTS text match + facet filters) and its bind values from a query
/// request — the common prefix of both `query_assets` (paged) and `query_asset_ids` (unbounded).
///
/// Text search hits the `asset_fts` inverted index (M1), widened by the synonym map (M3), and unions
/// that posting list with the filename-only trigram index for legacy in-word substring recall. Both
/// arms are indexed; a sub-trigram punctuation query is restricted to the newest fixed rowid window.
pub(crate) const SHORT_TEXT_CANDIDATES: i64 = 4_096;

fn push_text_where(
    text: &str,
    syn: &crate::search::SynonymMap,
    where_sql: &mut String,
    binds: &mut Vec<Value>,
) {
    let tokenized = crate::search::fts_match_expr(text, syn);
    let substring = crate::search::substring_match_expr(text);
    match (tokenized, substring) {
        (Some(m), Some(sub)) => {
            where_sql.push_str(
                " AND asset.rowid IN (\
                    SELECT rowid FROM asset_fts WHERE asset_fts MATCH ? \
                    UNION SELECT rowid FROM asset_filename_trigram \
                          WHERE asset_filename_trigram MATCH ?)",
            );
            binds.push(Value::Text(m));
            binds.push(Value::Text(sub));
        }
        (Some(m), None) => {
            where_sql.push_str(
                " AND asset.rowid IN (SELECT rowid FROM asset_fts WHERE asset_fts MATCH ?)",
            );
            binds.push(Value::Text(m));
        }
        (None, Some(sub)) => {
            where_sql.push_str(
                " AND asset.rowid IN (SELECT rowid FROM asset_filename_trigram \
                 WHERE asset_filename_trigram MATCH ?)",
            );
            binds.push(Value::Text(sub));
        }
        (None, None) => {
            // FTS5 trigram has no postings for one/two-character punctuation. Preserve the legacy
            // behavior for ordinary/small catalogs without ever making it an unbounded scan: rowid
            // is SQLite's integer primary key, so this is a range SEARCH over at most 4096 ids.
            where_sql.push_str(&format!(
                " AND asset.rowid IN (SELECT rowid FROM asset AS short_asset \
                 WHERE short_asset.rowid > \
                       COALESCE((SELECT MAX(rowid) FROM asset), 0) - {SHORT_TEXT_CANDIDATES} \
                   AND short_asset.filename LIKE ? ESCAPE '\\')"
            ));
            binds.push(Value::Text(format!("%{}%", escape_like(text))));
        }
    }
}

/// The page/hybrid query's one-time ranked match set. Joining this CTE avoids correlated MATCH and
/// bm25 probes in `ORDER BY`: the primary FTS posting list computes bm25 once, the authored tier is
/// another materialized posting list, and trigram-only hits receive the final fallback tier.
pub(crate) struct RankedTextMatches {
    pub cte: String,
    pub binds: Vec<Value>,
}

pub(crate) fn ranked_text_matches(
    text: &str,
    syn: &crate::search::SynonymMap,
) -> RankedTextMatches {
    let tokenized = crate::search::fts_match_expr(text, syn);
    let substring = crate::search::substring_match_expr(text);
    match (tokenized, substring) {
        (Some(m), Some(sub)) => RankedTextMatches {
            cte: format!(
                "WITH fts_matches(rowid, rank) AS MATERIALIZED (\
                    SELECT rowid, {rank} FROM asset_fts WHERE asset_fts MATCH ?\
                 ), authored_matches(rowid) AS MATERIALIZED (\
                    SELECT rowid FROM asset_fts WHERE asset_fts MATCH ?\
                 ), substring_matches(rowid) AS MATERIALIZED (\
                    SELECT rowid FROM asset_filename_trigram \
                    WHERE asset_filename_trigram MATCH ?\
                 ), text_matches(rowid, tier, rank) AS MATERIALIZED (\
                    SELECT f.rowid, CASE WHEN a.rowid IS NULL THEN 1 ELSE 0 END, f.rank \
                    FROM fts_matches f LEFT JOIN authored_matches a ON a.rowid = f.rowid \
                    UNION ALL \
                    SELECT s.rowid, 2, 1e9 FROM substring_matches s \
                    LEFT JOIN fts_matches f ON f.rowid = s.rowid WHERE f.rowid IS NULL\
                 )",
                rank = crate::search::FTS_RANK
            ),
            binds: vec![
                Value::Text(m.clone()),
                Value::Text(crate::search::authored_scoped(&m)),
                Value::Text(sub),
            ],
        },
        (Some(m), None) => RankedTextMatches {
            cte: format!(
                "WITH fts_matches(rowid, rank) AS MATERIALIZED (\
                    SELECT rowid, {rank} FROM asset_fts WHERE asset_fts MATCH ?\
                 ), authored_matches(rowid) AS MATERIALIZED (\
                    SELECT rowid FROM asset_fts WHERE asset_fts MATCH ?\
                 ), text_matches(rowid, tier, rank) AS MATERIALIZED (\
                    SELECT f.rowid, CASE WHEN a.rowid IS NULL THEN 1 ELSE 0 END, f.rank \
                    FROM fts_matches f LEFT JOIN authored_matches a ON a.rowid = f.rowid\
                 )",
                rank = crate::search::FTS_RANK
            ),
            binds: vec![
                Value::Text(m.clone()),
                Value::Text(crate::search::authored_scoped(&m)),
            ],
        },
        (None, Some(sub)) => RankedTextMatches {
            cte: "WITH text_matches(rowid, tier, rank) AS MATERIALIZED (\
                    SELECT rowid, 2, 1e9 FROM asset_filename_trigram \
                    WHERE asset_filename_trigram MATCH ?\
                  )"
            .into(),
            binds: vec![Value::Text(sub)],
        },
        (None, None) => RankedTextMatches {
            cte: format!(
                "WITH text_matches(rowid, tier, rank) AS MATERIALIZED (\
                    SELECT rowid, 2, 1e9 FROM asset AS short_asset \
                    WHERE short_asset.rowid > \
                          COALESCE((SELECT MAX(rowid) FROM asset), 0) - {SHORT_TEXT_CANDIDATES} \
                      AND short_asset.filename LIKE ? ESCAPE '\\'\
                 )"
            ),
            binds: vec![Value::Text(format!("%{}%", escape_like(text)))],
        },
    }
}

pub(crate) fn build_where(
    req: &QueryRequest,
    syn: &crate::search::SynonymMap,
    vis: &Visibility,
) -> Result<(String, Vec<Value>), LibError> {
    let mut where_sql = String::from(" WHERE 1=1");
    let mut binds: Vec<Value> = Vec::new();
    push_visibility(vis, "asset", &mut where_sql, &mut binds);
    if let Some(text) = req.text.as_ref().filter(|t| !t.is_empty()) {
        push_text_where(text, syn, &mut where_sql, &mut binds);
    }
    for f in &req.filters {
        apply_filter(f, &mut where_sql, &mut binds)?;
    }
    Ok((where_sql, binds))
}

/// Visibility + facets without text. Ranked page queries get their text restriction by joining the
/// `text_matches` CTE, while counts/id exports continue to use [`build_where`].
pub(crate) fn build_where_without_text(
    req: &QueryRequest,
    vis: &Visibility,
) -> Result<(String, Vec<Value>), LibError> {
    let mut where_sql = String::from(" WHERE 1=1");
    let mut binds = Vec::new();
    push_visibility(vis, "asset", &mut where_sql, &mut binds);
    for filter in &req.filters {
        apply_filter(filter, &mut where_sql, &mut binds)?;
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
        // Image attributes (image_attr). Dimensions span video too — same column, same meaning.
        Width => attr_num_filter_over(f, &["image_attr", "video_attr"], "width", where_sql, binds)?,
        Height => {
            attr_num_filter_over(f, &["image_attr", "video_attr"], "height", where_sql, binds)?
        }
        ColorDepth => attr_num_filter(f, "image_attr", "color_depth", where_sql, binds)?,
        HasAlpha => attr_bool_filter(f, "image_attr", "has_alpha", where_sql, binds)?,
        ColorSpace => attr_str_filter(f, "image_attr", "color_space", where_sql, binds)?,
        ImageClass => attr_str_filter(f, "image_attr", "class", where_sql, binds)?,
        Tileability => attr_num_filter(f, "image_attr", "tileability", where_sql, binds)?,
        TileClass => attr_str_filter(f, "image_attr", "tile_class", where_sql, binds)?,
        // Audio attributes (audio_attr).
        Bpm => attr_num_filter(f, "audio_attr", "bpm", where_sql, binds)?,
        Duration => attr_num_filter_over(
            f,
            &["audio_attr", "video_attr"],
            "duration_ms",
            where_sql,
            binds,
        )?,
        SampleRate => attr_num_filter(f, "audio_attr", "sample_rate", where_sql, binds)?,
        BitDepth => attr_num_filter(f, "audio_attr", "bit_depth", where_sql, binds)?,
        Channels => attr_num_filter(f, "audio_attr", "channels", where_sql, binds)?,
        MusicalKey => attr_str_filter(f, "audio_attr", "musical_key", where_sql, binds)?,
        Loudness => attr_num_filter(f, "audio_attr", "loudness_lufs", where_sql, binds)?,
        Brightness => attr_num_filter(f, "audio_attr", "brightness", where_sql, binds)?,
        Harmonicity => attr_num_filter(f, "audio_attr", "harmonicity", where_sql, binds)?,
        AudioClass => attr_str_filter(f, "audio_attr", "class", where_sql, binds)?,
        Codec => attr_str_filter(f, "audio_attr", "codec", where_sql, binds)?,
        Container => attr_str_filter(f, "audio_attr", "container", where_sql, binds)?,
        // Model attributes (model_attr).
        TriCount => attr_num_filter(f, "model_attr", "triangle_count", where_sql, binds)?,
        VertexCount => attr_num_filter(f, "model_attr", "vertex_count", where_sql, binds)?,
        MeshCount => attr_num_filter(f, "model_attr", "mesh_count", where_sql, binds)?,
        MaterialCount => attr_num_filter(f, "model_attr", "material_count", where_sql, binds)?,
        TextureCount => attr_num_filter(f, "model_attr", "texture_count", where_sql, binds)?,
        DependencyBytes => attr_num_filter(f, "model_attr", "dependency_bytes", where_sql, binds)?,
        HasRig => attr_bool_filter(f, "model_attr", "has_rig", where_sql, binds)?,
        HasAnimation => attr_bool_filter(f, "model_attr", "has_animation", where_sql, binds)?,
        HasUv => attr_bool_filter(f, "model_attr", "has_uv", where_sql, binds)?,
        ModelClass => attr_str_filter(f, "model_attr", "class", where_sql, binds)?,
        // Video attributes (video_attr). Width/height/duration are handled above, shared with
        // image/audio; these are the axes only video has.
        Fps => attr_num_filter(f, "video_attr", "fps", where_sql, binds)?,
        Bitrate => attr_num_filter(f, "video_attr", "bitrate", where_sql, binds)?,
        HasAudio => attr_bool_filter(f, "video_attr", "has_audio", where_sql, binds)?,
        VideoClass => attr_str_filter(f, "video_attr", "class", where_sql, binds)?,
        // Document attributes (document_attr).
        PageCount => attr_num_filter(f, "document_attr", "page_count", where_sql, binds)?,
        WordCount => attr_num_filter(f, "document_attr", "word_count", where_sql, binds)?,
        Author => attr_str_filter(f, "document_attr", "author", where_sql, binds)?,
        DocumentClass => attr_str_filter(f, "document_attr", "class", where_sql, binds)?,
        // A boolean flag on the asset row itself — presence of the filter means "favourites only".
        // `Eq false` inverts it (everything not favourited), which keeps the op meaningful.
        Favorite => {
            let want = !matches!(f.value, FilterValue::Bool(false));
            let test = if want { "!= 0" } else { "= 0" };
            where_sql.push_str(&format!(" AND (flags & {FAVORITE_FLAG}) {test}"));
        }
        // Source-relative path prefix (issue #66): scope the browse to a folder subtree. `path` lives
        // on the asset row, so it filters inline. An empty prefix is a no-op (matches everything).
        Path => match &f.value {
            FilterValue::Str(prefix) if !prefix.is_empty() => {
                where_sql.push_str(" AND path LIKE ? ESCAPE '\\'");
                binds.push(Value::Text(format!("{}%", escape_like(prefix))));
            }
            FilterValue::Str(_) => {}
            _ => {
                return Err(LibError::BadRequest(
                    "path filter wants a string prefix".into(),
                ))
            }
        },
        // The same subtree, minus its subtrees (issue #66): everything under the prefix with no
        // further `/` after it — the folder's own files. That second pattern is the whole
        // difference, and it is also why an empty value is meaningful here where it is a no-op for
        // `Path`: it selects the loose files sitting in the source root.
        Folder => match &f.value {
            FilterValue::Str(prefix) => {
                let esc = escape_like(prefix);
                where_sql.push_str(" AND path LIKE ? ESCAPE '\\' AND path NOT LIKE ? ESCAPE '\\'");
                binds.push(Value::Text(format!("{esc}%")));
                binds.push(Value::Text(format!("{esc}%/%")));
            }
            _ => {
                return Err(LibError::BadRequest(
                    "folder filter wants a string path".into(),
                ))
            }
        },
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
    attr_num_filter_over(f, &[table], col, where_sql, binds)
}

/// As [`attr_num_filter`], but matching the column across **several** attr tables.
///
/// Video shares its dimension and duration column names with `image_attr` and `audio_attr`
/// (`width`, `height`, `duration_ms`), and they mean the same thing, so "width > 1920" should find
/// a 4K texture *and* a 4K cutscene rather than forcing a second, near-identical facet field per
/// media type. Only one attr table can hold a row for a given asset, so the `UNION ALL` cannot
/// produce duplicates.
fn attr_num_filter_over(
    f: &Filter,
    tables: &[&str],
    col: &str,
    where_sql: &mut String,
    binds: &mut Vec<Value>,
) -> Result<(), LibError> {
    // Build the condition and its binds once, then replay the binds per table — each table
    // contributes its own set of `?` placeholders to the UNION.
    let mut cond_binds: Vec<Value> = Vec::new();
    let cond = match (&f.op, &f.value) {
        (FilterOp::Eq, FilterValue::Num(n)) => {
            cond_binds.push(Value::Real(*n));
            format!("{col} = ?")
        }
        (FilterOp::Gt, FilterValue::Num(n)) => {
            cond_binds.push(Value::Real(*n));
            format!("{col} > ?")
        }
        (FilterOp::Gte, FilterValue::Num(n)) => {
            cond_binds.push(Value::Real(*n));
            format!("{col} >= ?")
        }
        (FilterOp::Lt, FilterValue::Num(n)) => {
            cond_binds.push(Value::Real(*n));
            format!("{col} < ?")
        }
        (FilterOp::Lte, FilterValue::Num(n)) => {
            cond_binds.push(Value::Real(*n));
            format!("{col} <= ?")
        }
        (FilterOp::Range, FilterValue::Range(lo, hi)) => {
            cond_binds.push(Value::Real(*lo));
            cond_binds.push(Value::Real(*hi));
            format!("{col} BETWEEN ? AND ?")
        }
        _ => return Err(LibError::BadRequest(format!("unsupported filter on {col}"))),
    };
    let selects: Vec<String> = tables
        .iter()
        .map(|t| format!("SELECT asset_id FROM {t} WHERE {cond}"))
        .collect();
    where_sql.push_str(&format!(
        " AND asset.id IN ({})",
        selects.join(" UNION ALL ")
    ));
    for _ in tables {
        binds.extend(cond_binds.iter().cloned());
    }
    Ok(())
}

/// A string match against a per-media attribute column (`audio_attr.class`, `image_attr.tile_class`,
/// `audio_attr.musical_key`, …) — the enum-valued dropdowns of Advanced Search. Supports `Eq` and
/// `In` (multi-select). Emitted as a correlated subquery like [`attr_num_filter`] so it composes with
/// both the JOINed page query and the bare `COUNT(*)`. Compared `COLLATE NOCASE` so a dropdown value
/// need not match the stored casing exactly.
fn attr_str_filter(
    f: &Filter,
    table: &str,
    col: &str,
    where_sql: &mut String,
    binds: &mut Vec<Value>,
) -> Result<(), LibError> {
    let cond = match (&f.op, &f.value) {
        (FilterOp::Eq, FilterValue::Str(s)) => {
            binds.push(Value::Text(s.clone()));
            format!("{col} = ? COLLATE NOCASE")
        }
        (FilterOp::In, FilterValue::List(items)) => {
            let placeholders = items.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            for it in items {
                match it {
                    FilterValue::Str(s) => binds.push(Value::Text(s.clone())),
                    _ => return Err(LibError::BadRequest(format!("{col} IN list wants strings"))),
                }
            }
            format!("{col} COLLATE NOCASE IN ({placeholders})")
        }
        _ => return Err(LibError::BadRequest(format!("unsupported filter on {col}"))),
    };
    where_sql.push_str(&format!(
        " AND asset.id IN (SELECT asset_id FROM {table} WHERE {cond})"
    ));
    Ok(())
}

/// A boolean match against a per-media attribute column stored as INTEGER 0/1 (`model_attr.has_rig`,
/// `image_attr.has_alpha`, …) — the on/off toggles of Advanced Search. `Eq true` requires `= 1`,
/// `Eq false` requires `= 0` (an explicit negative, not the NULL "unknown"). Correlated subquery so
/// it composes with the bare `COUNT(*)` query.
fn attr_bool_filter(
    f: &Filter,
    table: &str,
    col: &str,
    where_sql: &mut String,
    binds: &mut Vec<Value>,
) -> Result<(), LibError> {
    let want = match (&f.op, &f.value) {
        (FilterOp::Eq, FilterValue::Bool(b)) => *b,
        _ => return Err(LibError::BadRequest(format!("unsupported filter on {col}"))),
    };
    binds.push(Value::Integer(if want { 1 } else { 0 }));
    where_sql.push_str(&format!(
        " AND asset.id IN (SELECT asset_id FROM {table} WHERE {col} = ?)"
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
            // Image attrs.
            Width => (FilterOp::Gte, FilterValue::Num(512.0)),
            Height => (FilterOp::Lte, FilterValue::Num(512.0)),
            ColorDepth => (FilterOp::Eq, FilterValue::Num(8.0)),
            HasAlpha => (FilterOp::Eq, FilterValue::Bool(true)),
            ColorSpace => (FilterOp::Eq, FilterValue::Str("srgb".into())),
            ImageClass => (FilterOp::Eq, FilterValue::Str("texture".into())),
            Tileability => (FilterOp::Gte, FilterValue::Num(0.8)),
            TileClass => (FilterOp::Eq, FilterValue::Str("seamless".into())),
            // Audio attrs.
            Bpm => (FilterOp::Range, FilterValue::Range(90.0, 130.0)),
            Duration => (FilterOp::Range, FilterValue::Range(0.0, 5000.0)),
            SampleRate => (FilterOp::Eq, FilterValue::Num(44_100.0)),
            BitDepth => (FilterOp::Eq, FilterValue::Num(16.0)),
            Channels => (FilterOp::Eq, FilterValue::Num(2.0)),
            MusicalKey => (FilterOp::Eq, FilterValue::Str("c".into())),
            Loudness => (FilterOp::Gte, FilterValue::Num(-23.0)),
            Brightness => (FilterOp::Range, FilterValue::Range(0.0, 1.0)),
            Harmonicity => (FilterOp::Range, FilterValue::Range(0.0, 1.0)),
            AudioClass => (FilterOp::Eq, FilterValue::Str("loop".into())),
            Codec => (FilterOp::Eq, FilterValue::Str("pcm".into())),
            Container => (FilterOp::Eq, FilterValue::Str("wav".into())),
            // Model attrs.
            TriCount => (FilterOp::Lt, FilterValue::Num(50_000.0)),
            VertexCount => (FilterOp::Lt, FilterValue::Num(50_000.0)),
            MeshCount => (FilterOp::Lte, FilterValue::Num(8.0)),
            MaterialCount => (FilterOp::Lte, FilterValue::Num(4.0)),
            TextureCount => (FilterOp::Lte, FilterValue::Num(4.0)),
            DependencyBytes => (FilterOp::Lt, FilterValue::Num(1_000_000.0)),
            HasRig => (FilterOp::Eq, FilterValue::Bool(true)),
            HasAnimation => (FilterOp::Eq, FilterValue::Bool(false)),
            HasUv => (FilterOp::Eq, FilterValue::Bool(true)),
            ModelClass => (FilterOp::Eq, FilterValue::Str("prop_lowpoly".into())),
            // Video attrs (width/height/duration are shared with image/audio, above).
            Fps => (FilterOp::Gte, FilterValue::Num(24.0)),
            Bitrate => (FilterOp::Lt, FilterValue::Num(8_000_000.0)),
            HasAudio => (FilterOp::Eq, FilterValue::Bool(true)),
            VideoClass => (FilterOp::Eq, FilterValue::Str("cutscene".into())),
            // Document attrs.
            PageCount => (FilterOp::Lte, FilterValue::Num(20.0)),
            WordCount => (FilterOp::Gte, FilterValue::Num(100.0)),
            Author => (FilterOp::Eq, FilterValue::Str("ada".into())),
            DocumentClass => (FilterOp::Eq, FilterValue::Str("license".into())),
            Favorite => (FilterOp::Eq, FilterValue::Bool(true)),
            Path => (FilterOp::Eq, FilterValue::Str("Environment/".into())),
            Folder => (FilterOp::Eq, FilterValue::Str("Environment/Rock/".into())),
        };
        Filter { field, op, value }
    }

    /// Acceptance for #37: no `FacetField` variant reachable from the API is `Unsupported`, and each
    /// builds a WHERE clause that a live SQLite catalog accepts.
    #[test]
    fn every_facet_field_builds_and_runs() {
        use FacetField::*;
        let fields = [
            MediaType,
            Format,
            Source,
            Tag,
            SizeBytes,
            License,
            UsageRight,
            Favorite,
            Path,
            Folder,
            // Image attrs.
            Width,
            Height,
            ColorDepth,
            HasAlpha,
            ColorSpace,
            ImageClass,
            Tileability,
            TileClass,
            // Audio attrs.
            Bpm,
            Duration,
            SampleRate,
            BitDepth,
            Channels,
            MusicalKey,
            Loudness,
            Brightness,
            Harmonicity,
            AudioClass,
            Codec,
            Container,
            // Model attrs.
            TriCount,
            VertexCount,
            MeshCount,
            MaterialCount,
            TextureCount,
            DependencyBytes,
            HasRig,
            HasAnimation,
            HasUv,
            ModelClass,
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
            let (where_sql, binds) = match build_where(&req, &syn, &Visibility::Full) {
                Ok(v) => v,
                Err(e) => panic!("filter on {field:?} failed to build: {e:?}"),
            };
            assert!(
                !matches!(
                    build_where(&req, &syn, &Visibility::Full),
                    Err(LibError::Unsupported(_))
                ),
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
