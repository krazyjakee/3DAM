//! Mints and resolves the short-lived tickets that carry a credential onto the URL-only surfaces
//! — the WebSocket handshake and the browser's range requests for audio/video.

use crate::{auth, ApiError, AppState};
use axum::extract::State;
use axum::http::{header, HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use dam_api::id::AssetId;
use dam_api::LibError;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct WsTicket {
    pub(crate) expires: Instant,
    pub(crate) ctx: dam_api::service::AuthContext,
    pub(crate) token: Option<String>,
    pub(crate) cookie: Option<String>,
}

#[derive(Clone)]
pub(crate) struct MediaTicket {
    expires: Instant,
    target: String,
    ctx: dam_api::service::AuthContext,
    token: Option<String>,
    cookie: Option<String>,
}

pub(crate) fn empty_ws_tickets() -> Arc<std::sync::Mutex<HashMap<[u8; 32], WsTicket>>> {
    Arc::new(std::sync::Mutex::new(HashMap::new()))
}

pub(crate) fn empty_media_tickets() -> Arc<std::sync::Mutex<HashMap<[u8; 32], MediaTicket>>> {
    Arc::new(std::sync::Mutex::new(HashMap::new()))
}

const WS_TICKET_TTL: Duration = Duration::from_secs(30);
const MEDIA_TICKET_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_ACTIVE_TICKETS: usize = 4096;

fn ticket_secret(prefix: &str) -> String {
    // UUIDv7 has 74 random bits; two independent values give a comfortably unguessable ephemeral
    // secret without adding another RNG dependency. Only its BLAKE3 digest is retained server-side.
    format!(
        "{prefix}{}_{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    )
}

pub(crate) fn ticket_key(secret: &str) -> [u8; 32] {
    *blake3::hash(secret.as_bytes()).as_bytes()
}

fn csrf_for_ticket(headers: &HeaderMap, resolved: &auth::Resolved) -> Result<(), LibError> {
    let Some(session) = &resolved.session else {
        return Ok(());
    };
    let csrf = headers
        .get(auth::CSRF_HEADER)
        .and_then(|value| value.to_str().ok());
    if csrf == Some(session.csrf.as_str()) {
        Ok(())
    } else {
        Err(LibError::Forbidden("missing or invalid CSRF token".into()))
    }
}

fn ticket_credentials(
    headers: &HeaderMap,
    resolved: &auth::Resolved,
) -> (Option<String>, Option<String>) {
    // Auth-off ignores a supplied header; do not retain attacker-controlled text in that case.
    let token = (resolved.session.is_none() && resolved.ctx.identity.is_some())
        .then(|| auth::bearer_header(headers))
        .flatten();
    let cookie = resolved
        .session
        .as_ref()
        .and_then(|_| auth::cookie_value(headers, auth::SESSION_COOKIE));
    (token, cookie)
}

#[derive(serde::Serialize)]
struct TicketReply {
    ticket: String,
    expires_in: u64,
}

pub(crate) async fn mint_ws_ticket(headers: HeaderMap, State(st): State<AppState>) -> Response {
    let resolved = match auth::resolve_ws(&st, &headers).await {
        Ok(value) => value,
        Err(error) => return ApiError(error).into_response(),
    };
    if let Err(error) = csrf_for_ticket(&headers, &resolved) {
        return ApiError(error).into_response();
    }
    let (token, cookie) = ticket_credentials(&headers, &resolved);
    let secret = ticket_secret("dam_ws_");
    let now = Instant::now();
    let mut tickets = match st.ws_tickets.lock() {
        Ok(tickets) => tickets,
        Err(_) => {
            return ApiError(LibError::Internal("ticket service unavailable".into()))
                .into_response()
        }
    };
    tickets.retain(|_, value| value.expires > now);
    if tickets.len() >= MAX_ACTIVE_TICKETS {
        return ApiError(LibError::RateLimited { retry_after: 1 }).into_response();
    }
    tickets.insert(
        ticket_key(&secret),
        WsTicket {
            expires: now + WS_TICKET_TTL,
            ctx: resolved.ctx,
            token,
            cookie,
        },
    );
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(TicketReply {
            ticket: secret,
            expires_in: WS_TICKET_TTL.as_secs(),
        }),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct MediaTicketRequest {
    target: String,
}

fn allowed_media_target(target: &str) -> bool {
    let Ok(uri) = target.parse::<Uri>() else {
        return false;
    };
    if uri.scheme().is_some() || uri.authority().is_some() {
        return false;
    }
    let segments: Vec<_> = uri
        .path()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() != 5
        || segments[..3] != ["api", "v1", "assets"]
        || segments[3].parse::<AssetId>().is_err()
        || !matches!(
            segments[4],
            "content" | "related" | "preview-mesh" | "thumbnail"
        )
    {
        return false;
    }
    !uri.query()
        .unwrap_or_default()
        .split('&')
        .any(|pair| pair.starts_with("ticket=") || pair.starts_with("token="))
}

pub(crate) async fn mint_media_ticket(
    headers: HeaderMap,
    State(st): State<AppState>,
    Json(request): Json<MediaTicketRequest>,
) -> Response {
    if !allowed_media_target(&request.target) {
        return ApiError(LibError::BadRequest("invalid media target".into())).into_response();
    }
    let resolved = match auth::resolve_ws(&st, &headers).await {
        Ok(value) => value,
        Err(error) => return ApiError(error).into_response(),
    };
    if let Err(error) = csrf_for_ticket(&headers, &resolved) {
        return ApiError(error).into_response();
    }
    let (token, cookie) = ticket_credentials(&headers, &resolved);
    let secret = ticket_secret("dam_media_");
    let now = Instant::now();
    let mut tickets = match st.media_tickets.lock() {
        Ok(tickets) => tickets,
        Err(_) => {
            return ApiError(LibError::Internal("ticket service unavailable".into()))
                .into_response()
        }
    };
    tickets.retain(|_, value| value.expires > now);
    if tickets.len() >= MAX_ACTIVE_TICKETS {
        return ApiError(LibError::RateLimited { retry_after: 1 }).into_response();
    }
    tickets.insert(
        ticket_key(&secret),
        MediaTicket {
            expires: now + MEDIA_TICKET_TTL,
            target: request.target,
            ctx: resolved.ctx,
            token,
            cookie,
        },
    );
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(TicketReply {
            ticket: secret,
            expires_in: MEDIA_TICKET_TTL.as_secs(),
        }),
    )
        .into_response()
}

/// Resolve a media ticket only for the exact path+query it was minted for. Unlike a WebSocket
/// ticket it is replayable until expiry because browsers issue multiple Range GETs while seeking.
/// The parent credential is re-verified on every use, so revocation takes effect immediately.
pub(crate) async fn resolve_media_ticket(
    st: &AppState,
    parts: &axum::http::request::Parts,
) -> Option<Result<dam_api::service::AuthContext, LibError>> {
    let query = parts.uri.query()?;
    let secret = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("ticket="))
        .filter(|value| !value.is_empty())?;
    let clean_query = query
        .split('&')
        .filter(|pair| !pair.starts_with("ticket="))
        .collect::<Vec<_>>()
        .join("&");
    let target = if clean_query.is_empty() {
        parts.uri.path().to_string()
    } else {
        format!("{}?{clean_query}", parts.uri.path())
    };
    let ticket = match st.media_tickets.lock() {
        Ok(mut tickets) => {
            let now = Instant::now();
            tickets.retain(|_, value| value.expires > now);
            tickets.get(&ticket_key(secret)).cloned()
        }
        Err(_) => return Some(Err(LibError::Internal("ticket service unavailable".into()))),
    };
    let Some(ticket) = ticket else {
        return Some(Err(LibError::Unauthorized));
    };
    if ticket.target != target {
        return Some(Err(LibError::Unauthorized));
    }
    let resolved = if ticket.token.is_some() || ticket.cookie.is_some() {
        auth::resolve_request(st, ticket.token, ticket.cookie)
            .await
            .map(|value| value.ctx)
    } else {
        Ok(ticket.ctx)
    };
    Some(resolved.and_then(|ctx| {
        ctx.require(dam_api::service::Scope::Read)?;
        Ok(ctx)
    }))
}

#[cfg(test)]
mod tests {
    use super::allowed_media_target;

    #[test]
    fn media_ticket_targets_are_exact_media_resources() {
        let id = uuid::Uuid::now_v7();
        assert!(allowed_media_target(&format!(
            "/api/v1/assets/{id}/content"
        )));
        assert!(allowed_media_target(&format!(
            "/api/v1/assets/{id}/related?path=materials%2Fa.png"
        )));
        assert!(!allowed_media_target(&format!("/api/v1/assets/{id}")));
        assert!(!allowed_media_target(&format!(
            "/api/v1/assets/{id}/content?token=dam_secret"
        )));
        assert!(!allowed_media_target(
            "https://other.test/api/v1/assets/x/content"
        ));
    }
}
