//! Serves the embedded React web client (tech-spec 09 §A.4): SPA-fallback routing plus
//! `Accept-Encoding` negotiation over the pre-compressed sidecars baked in beside each asset.

use crate::ApiError;
use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use dam_api::LibError;
use rust_embed::RustEmbed;

/// The built React web client (tech-spec 09 §A.4). `pnpm build` emits content-hashed assets into
/// `web/dist/`; this bakes them into the `3dam` binary so one file serves the whole UI with no
/// separate deploy. In debug builds rust-embed reads from disk (fast iteration); release embeds.
/// If `web/dist` is empty (web client not yet built), the handler serves a build hint instead.
#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
struct WebAssets;

/// Serve the embedded web client with SPA-fallback semantics (tech-spec 09 §A.4).
pub(crate) async fn static_handler(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');

    if path.starts_with("api/") || path == "api" {
        return ApiError(LibError::NotFound(format!("no route: /{path}"))).into_response();
    }

    if let Some(resp) = serve_embedded(path, &headers) {
        return resp;
    }
    match serve_embedded("index.html", &headers) {
        Some(resp) => resp,
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<!doctype html><meta charset=utf-8><title>3dam</title>\
             <h1>3dam serve</h1><p>API is live at <code>/api/v1</code>. The web client bundle is \
             not present — run <code>pnpm --dir web build</code> (or <code>cargo xtask web</code>) \
             and rebuild.</p>",
        )
            .into_response(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaticEncoding {
    Brotli,
    Gzip,
}

impl StaticEncoding {
    fn header_value(self) -> header::HeaderValue {
        match self {
            Self::Brotli => header::HeaderValue::from_static("br"),
            Self::Gzip => header::HeaderValue::from_static("gzip"),
        }
    }
}

fn available_static_encoding(
    candidates: &[StaticEncoding],
    has_brotli: bool,
    has_gzip: bool,
) -> Option<StaticEncoding> {
    candidates
        .iter()
        .copied()
        .find(|candidate| match candidate {
            StaticEncoding::Brotli => has_brotli,
            StaticEncoding::Gzip => has_gzip,
        })
}

#[derive(Default)]
struct AcceptedEncodings {
    present: bool,
    brotli: Option<u16>,
    gzip: Option<u16>,
    wildcard: Option<u16>,
    identity: Option<u16>,
}

/// Parse an HTTP qvalue to thousandths. Invalid values reject that coding rather than silently
/// promoting it to full quality.
fn encoding_qvalue(parameter: Option<&str>) -> u16 {
    let Some(parameter) = parameter else {
        return 1000;
    };
    let Some((name, raw)) = parameter.trim().split_once('=') else {
        return 0;
    };
    if !name.trim().eq_ignore_ascii_case("q") {
        return 0;
    }
    let raw = raw.trim();
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return 0;
    }
    match whole {
        "0" => {
            let padded = format!("{fraction:0<3}");
            padded.parse().unwrap_or(0)
        }
        "1" if fraction.bytes().all(|byte| byte == b'0') => 1000,
        _ => 0,
    }
}

fn accepted_encodings(headers: &HeaderMap) -> AcceptedEncodings {
    let mut accepted = AcceptedEncodings::default();
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        accepted.present = true;
        let Ok(value) = value.to_str() else {
            continue;
        };
        for item in value.split(',') {
            let mut parts = item.trim().split(';');
            let coding = parts.next().unwrap_or_default().trim();
            let quality = encoding_qvalue(parts.next());
            let slot = if coding.eq_ignore_ascii_case("br") {
                &mut accepted.brotli
            } else if coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip") {
                &mut accepted.gzip
            } else if coding == "*" {
                &mut accepted.wildcard
            } else if coding.eq_ignore_ascii_case("identity") {
                &mut accepted.identity
            } else {
                continue;
            };
            *slot = Some((*slot).unwrap_or(0).max(quality));
        }
    }
    accepted
}

/// Order supported codings by the client's quality (Brotli wins an equal-quality tie). Returning
/// both acceptable choices lets the static handler fall back to a gzip sidecar when Brotli was
/// preferred but not emitted, rather than throwing away the remaining acceptable representation.
/// `Err` means the client explicitly ruled out Brotli, gzip, *and* identity.
fn static_encoding_candidates(headers: &HeaderMap) -> Result<Vec<StaticEncoding>, ()> {
    let accepted = accepted_encodings(headers);
    if !accepted.present {
        return Ok(Vec::new());
    }
    let brotli = accepted.brotli.or(accepted.wildcard).unwrap_or(0);
    let gzip = accepted.gzip.or(accepted.wildcard).unwrap_or(0);
    let mut candidates = Vec::with_capacity(2);
    if brotli >= gzip {
        if brotli > 0 {
            candidates.push(StaticEncoding::Brotli);
        }
        if gzip > 0 {
            candidates.push(StaticEncoding::Gzip);
        }
    } else {
        if gzip > 0 {
            candidates.push(StaticEncoding::Gzip);
        }
        if brotli > 0 {
            candidates.push(StaticEncoding::Brotli);
        }
    }
    if !candidates.is_empty() {
        return Ok(candidates);
    }
    let identity = accepted
        .identity
        .unwrap_or(if accepted.wildcard == Some(0) {
            0
        } else {
            1000
        });
    if identity == 0 {
        Err(())
    } else {
        Ok(candidates)
    }
}

fn serve_embedded(path: &str, request_headers: &HeaderMap) -> Option<Response> {
    let file = WebAssets::get(path)?;
    let mime = file.metadata.mimetype().to_string();
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let candidates = match static_encoding_candidates(request_headers) {
        Ok(candidates) => candidates,
        Err(()) => return Some(StatusCode::NOT_ACCEPTABLE.into_response()),
    };
    let brotli = WebAssets::get(&format!("{path}.br"));
    let gzip = WebAssets::get(&format!("{path}.gz"));
    let has_encoded_variants = brotli.is_some() || gzip.is_some();
    let encoded =
        available_static_encoding(&candidates, brotli.is_some(), gzip.is_some()).map(|encoding| {
            let file = match encoding {
                StaticEncoding::Brotli => brotli.unwrap(),
                StaticEncoding::Gzip => gzip.unwrap(),
            };
            (encoding, file)
        });
    let mut response = (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        Body::from(match &encoded {
            Some((_, file)) => file.data.clone().into_owned(),
            None => file.data.into_owned(),
        }),
    )
        .into_response();
    if let Some((encoding, _)) = encoded {
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, encoding.header_value());
    }
    // Every cache entry in a family with sidecars varies, including identity. Otherwise a shared
    // cache populated by a client without Accept-Encoding can mask Brotli/gzip for later clients.
    if has_encoded_variants {
        response.headers_mut().append(
            header::VARY,
            header::HeaderValue::from_static("Accept-Encoding"),
        );
    }
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::{
        available_static_encoding, serve_embedded, static_encoding_candidates, StaticEncoding,
    };
    use axum::http::{header, HeaderMap};

    fn accepted(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, value.parse().unwrap());
        headers
    }

    #[test]
    fn static_negotiation_orders_qvalues_and_uses_an_available_fallback() {
        let preferred_brotli = static_encoding_candidates(&accepted("gzip;q=0.5, br;q=1"))
            .expect("acceptable encodings");
        assert_eq!(
            preferred_brotli,
            [StaticEncoding::Brotli, StaticEncoding::Gzip]
        );
        assert_eq!(
            available_static_encoding(&preferred_brotli, false, true),
            Some(StaticEncoding::Gzip),
            "a missing preferred Brotli sidecar falls back to acceptable gzip"
        );

        let preferred_gzip = static_encoding_candidates(&accepted("gzip;q=1, br;q=0.2"))
            .expect("acceptable encodings");
        assert_eq!(
            preferred_gzip,
            [StaticEncoding::Gzip, StaticEncoding::Brotli]
        );
        assert!(static_encoding_candidates(&accepted("*;q=0, identity;q=0")).is_err());
    }

    #[test]
    fn static_identity_varies_when_sidecars_exist() {
        // A plain `cargo test` is supported without a web build; the canonical CI gate builds web
        // first. Exercise the cache semantics whenever that production input is present.
        if super::WebAssets::get("index.html.br").is_none() {
            return;
        }
        let response = serve_embedded("index.html", &accepted("identity")).unwrap();
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(response
            .headers()
            .get_all(header::VARY)
            .iter()
            .any(|value| {
                value
                    .to_str()
                    .unwrap_or_default()
                    .eq_ignore_ascii_case("accept-encoding")
            }));
    }
}
