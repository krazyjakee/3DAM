//! Asset content, related-file, preview-mesh, and thumbnail HTTP delivery.

use crate::auth::Reader;
use crate::{parse_id, ApiError, AppState, OwnerQuery};
use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, MethodRouter};
use dam_api::dto::{AssetContent, AssetContentMetadata, AssetContentStream, ContentRange};
use dam_api::id::AssetId;
use dam_api::service::LibraryService;

/// Method routers for asset byte and derivative delivery.
pub(crate) struct Routes {
    pub(crate) content: MethodRouter<AppState>,
    pub(crate) related: MethodRouter<AppState>,
    pub(crate) preview_mesh: MethodRouter<AppState>,
    pub(crate) thumbnail: MethodRouter<AppState>,
}

pub(crate) fn routes() -> Routes {
    Routes {
        content: get(asset_content),
        related: get(asset_related),
        preview_mesh: get(asset_preview_mesh),
        thumbnail: get(asset_thumbnail),
    }
}

fn content_response(content: AssetContent, cache_control: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content.content_type),
            (header::CACHE_CONTROL, cache_control.to_string()),
        ],
        Body::from(content.bytes),
    )
        .into_response()
}

/// What a `Range` header resolves to against a known length — the three outcomes RFC 9110 §14.2
/// distinguishes, which are *not* two: "I can't parse this" and "this asks for bytes you don't
/// have" get different answers, and conflating them 416s a client that only sent us a typo.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RangeSpec {
    /// A valid single range that overlaps the representation: serve `206` with these bounds.
    Satisfiable(u64, u64),
    /// Syntactically valid but starting past the end of the representation: `416`, the one case
    /// the status is for.
    Unsatisfiable,
    /// Unparsable (`bytes=abc-def`, `bytes=50-10`, an overflowing number), or a form we don't
    /// implement. RFC 9110 requires an unsatisfiable-*looking* but invalid spec to be **ignored**
    /// — "a server MUST ignore a Range header field that contains a range unit it does not
    /// understand" — so these fall through to the full `200`.
    Ignore,
}

/// Resolve a single byte range against a known length.
///
/// Only the single-range form is supported. Multi-range (`bytes=0-99,200-299`) requires a
/// `multipart/byteranges` body, is not used by any media element, and is explicitly optional in
/// RFC 9110 §14.2 — a server may answer it with the whole representation, which is what
/// [`RangeSpec::Ignore`] does.
///
/// Total: every arithmetic path saturates, so a zero-length representation or an absurd offset
/// returns an answer rather than panicking.
pub(crate) fn parse_range(spec: &str, len: u64) -> RangeSpec {
    let Some(spec) = spec.trim().strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    if spec.contains(',') {
        return RangeSpec::Ignore;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return RangeSpec::Ignore;
    };
    let (start, end) = (start.trim(), end.trim());
    let (first, last) = if start.is_empty() {
        // Suffix form `bytes=-N`: the final N bytes. `-0` asks for nothing, which is valid syntax
        // and cannot be satisfied.
        let Ok(n) = end.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        if n == 0 {
            return RangeSpec::Unsatisfiable;
        }
        (len.saturating_sub(n), len.saturating_sub(1))
    } else {
        let Ok(first) = start.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        let last = if end.is_empty() {
            len.saturating_sub(1)
        } else {
            let Ok(last) = end.parse::<u64>() else {
                return RangeSpec::Ignore;
            };
            // A last-byte-pos below first-byte-pos makes the spec invalid, not unsatisfiable.
            // Checked before clamping, so `bytes=150-140` against a short body is still a typo.
            if last < first {
                return RangeSpec::Ignore;
            }
            last.min(len.saturating_sub(1))
        };
        (first, last)
    };
    if len == 0 || first >= len {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Satisfiable(first, last)
}

/// Serve asset bytes with **range support**.
///
/// Range matters far more here than it did for images and meshes: a browser `<video>` element
/// issues a range request to seek, and several will refuse to show a scrub bar (or to play at all,
/// on Safari) against a server that answers `200` to every request. Video is served as original
/// bytes through this one route — there is no transcoding and no WASM island — so this is what
/// makes video preview work at all (PRODUCT_SPEC §9 phase 2b).
///
/// `Accept-Ranges: bytes` is advertised on every response, including the unranged `200`, so the
/// client knows seeking is available before it tries.
///
/// Bytes are pulled directly from the capability-confined local handle or the remote source range
/// API into a two-chunk producer window. Axum polls that stream as the socket accepts data; a
/// disconnect drops it, which stops the source loop instead of finishing a gigabyte transfer.
fn streamed_content_response(
    content: AssetContentStream,
    status: StatusCode,
    cache_control: &'static str,
) -> Response {
    let mut response = Response::new(Body::from_stream(content.bytes));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        content
            .metadata
            .content_type
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&content.range.len().to_string()).unwrap(),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            header::HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                content.range.first(),
                content.range.last(),
                content.metadata.len
            ))
            .unwrap(),
        );
    }
    if let Some(etag) = content.metadata.etag {
        if let Ok(etag) = header::HeaderValue::from_str(&etag) {
            headers.insert(header::ETAG, etag);
        }
    }
    response
}

fn metadata_only_response(
    metadata: &AssetContentMetadata,
    status: StatusCode,
    cache_control: &'static str,
    content_range: Option<String>,
) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        metadata
            .content_type
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(
            &if status == StatusCode::OK {
                metadata.len
            } else {
                0
            }
            .to_string(),
        )
        .unwrap(),
    );
    if let Some(value) = content_range {
        headers.insert(
            header::CONTENT_RANGE,
            header::HeaderValue::from_str(&value).unwrap(),
        );
    }
    if let Some(etag) = metadata.etag.as_deref() {
        if let Ok(etag) = header::HeaderValue::from_str(etag) {
            headers.insert(header::ETAG, etag);
        }
    }
    response
}

/// `If-Range` is deliberately strict. Only the strong content-hash tag emitted by this route can
/// prove the requested range belongs to the current representation; weak tags, dates, malformed
/// values, and assets without a hash all fall back to a complete `200` stream.
fn if_range_matches(headers: &HeaderMap, metadata: &AssetContentMetadata) -> bool {
    let Some(value) = headers.get(header::IF_RANGE) else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    !value.starts_with("W/") && metadata.etag.as_deref() == Some(value.trim())
}

async fn asset_content(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<OwnerQuery>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let metadata = st.lib.content_metadata_from(&ctx, &id, q.source).await?;
    const CACHE_CONTROL: &str = "private, max-age=60";

    // HEAD describes the complete selected representation. Range is defined for GET; more
    // importantly this branch never starts a source producer, so an availability probe is cheap.
    if method == Method::HEAD {
        return Ok(metadata_only_response(
            &metadata,
            StatusCode::OK,
            CACHE_CONTROL,
            None,
        ));
    }

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|_| if_range_matches(&headers, &metadata));
    let (status, range) = match range.map(|value| parse_range(value, metadata.len)) {
        Some(RangeSpec::Satisfiable(first, last)) => (
            StatusCode::PARTIAL_CONTENT,
            ContentRange::new(first, last).expect("a satisfiable range is ordered"),
        ),
        Some(RangeSpec::Unsatisfiable) => {
            return Ok(metadata_only_response(
                &metadata,
                StatusCode::RANGE_NOT_SATISFIABLE,
                CACHE_CONTROL,
                Some(format!("bytes */{}", metadata.len)),
            ));
        }
        Some(RangeSpec::Ignore) | None if metadata.len > 0 => (
            StatusCode::OK,
            ContentRange::new(0, metadata.len - 1).expect("non-empty full range is ordered"),
        ),
        Some(RangeSpec::Ignore) | None => {
            return Ok(metadata_only_response(
                &metadata,
                StatusCode::OK,
                CACHE_CONTROL,
                None,
            ));
        }
    };
    let content = st
        .lib
        .stream_content_from(&ctx, &id, range, q.source)
        .await?;
    Ok(streamed_content_response(content, status, CACHE_CONTROL))
}

async fn asset_related(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<RelatedQuery>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st
        .lib
        .read_related_content_from(&ctx, &id, &q.path, q.source)
        .await?;
    Ok(content_response(content, "private, max-age=60"))
}

/// Serve the interactive 3D preview mesh (`DMSH` blob) the WASM viewer island uploads directly.
/// Generated + cached server-side from the same Assimp decode as the turntable thumbnail, so it
/// covers the full professional format range with textures. `Unsupported` (415) for non-models.
async fn asset_preview_mesh(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<OwnerQuery>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st.lib.read_model_preview_from(&ctx, &id, q.source).await?;
    Ok(content_response(content, "private, max-age=300"))
}

#[derive(serde::Deserialize)]
struct RelatedQuery {
    /// The glTF-relative URI of the sibling file (`.bin` / texture), resolved against the asset dir.
    path: String,
    source: Option<dam_api::id::SourceId>,
}

async fn asset_thumbnail(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<ThumbQuery>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let edge = q.edge.unwrap_or(256);
    let content = st
        .lib
        .read_thumbnail_from(&ctx, &id, edge, q.source)
        .await?;
    Ok(content_response(content, "private, max-age=300"))
}

#[derive(serde::Deserialize)]
struct ThumbQuery {
    edge: Option<u32>,
    source: Option<dam_api::id::SourceId>,
}
