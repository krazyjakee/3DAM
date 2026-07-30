//! The `/api/v1/auth` surface (phase 6, issue #42): first-run claim, login/logout, and the
//! caller's own session management. Behind the `UserAccounts` flag — off ⇒ every route here 404s
//! ("off removes the surface", ADR 0004), checked before auth so a disabled surface cannot be
//! probed.
//!
//! ## The claim gate (issue #42 §2)
//!
//! An unclaimed instance (accounts on, zero accounts) is a land-grab risk — the first-run race
//! Jellyfin/Grafana shipped as CVEs. The mitigations here, per the spec amendment to tech-spec 10
//! §4.4:
//! - claim acceptance is bound to a **loopback peer address** (or a loopback-only bind); a remote
//!   request to an unclaimed instance is refused, not served a signup form;
//! - off-box, the claim degrades to **token redemption**: a caller presenting an Admin-scoped
//!   bearer (the bootstrap owner token minted when the gate went up) may claim from anywhere;
//! - the unclaimed state is **loud**: `serve` logs a recurring warning and `/admin/api/status`
//!   reports `unclaimed: true`.

use crate::auth::{self, CSRF_COOKIE, SESSION_COOKIE};
use crate::store::accounts::NewSession;
use crate::{ApiError, AppState};
use axum::extract::{ConnectInfo, FromRequestParts, Path as AxPath, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_api::accounts::*;
use dam_api::service::Scope;
use dam_api::LibError;
use std::net::SocketAddr;

/// The peer address, when the serve stack registered connect-info (`None` in the in-process test
/// seam, which never crosses a socket). Infallible — absence is data here, not an error, because
/// the claim gate treats "unknown peer" as "not loopback".
struct PeerAddr(Option<SocketAddr>);
impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _s: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0),
        ))
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/status", get(status))
        .route("/api/v1/auth/claim", post(claim))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/sessions", get(list_sessions))
        .route(
            "/api/v1/auth/sessions/{id}",
            axum::routing::delete(revoke_session),
        )
}

/// Route-level flag guard: `UserAccounts` off ⇒ the surface is absent (404), not forbidden.
fn require_enabled(st: &AppState) -> Result<(), ApiError> {
    if st.store.user_accounts() {
        Ok(())
    } else {
        Err(ApiError(LibError::NotFound(
            "user accounts are disabled".into(),
        )))
    }
}

/// The `Set-Cookie` pair for a fresh session: the `HttpOnly` session cookie and the JS-readable
/// CSRF cookie (double-submit). `Max-Age` matches the 90-day absolute ceiling — the 14-day
/// inactivity expiry is enforced server-side. `Secure` rides the TLS posture.
fn session_cookies(sess: &NewSession, tls: bool) -> [String; 2] {
    let secure = if tls { "; Secure" } else { "" };
    let max_age = 90 * 24 * 60 * 60;
    [
        format!(
            "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}",
            sess.cookie_value
        ),
        format!(
            "{CSRF_COOKIE}={}; Path=/; SameSite=Strict; Max-Age={max_age}{secure}",
            sess.csrf
        ),
    ]
}

/// Expire both cookies (logout).
fn clear_cookies() -> [String; 2] {
    [
        format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
        format!("{CSRF_COOKIE}=; Path=/; SameSite=Strict; Max-Age=0"),
    ]
}

/// Attach cookies to a response by **appending** each `Set-Cookie` line. A response carries two of
/// them (session + CSRF), and the tuple/array `IntoResponse` impls *insert* by header name — which
/// would silently drop the first cookie — so the two are appended explicitly here.
fn with_cookies(body: impl IntoResponse, cookies: [String; 2]) -> Response {
    let mut resp = body.into_response();
    for c in cookies {
        // Cookie values are hex/uuid-safe by construction, so this never fails in practice.
        if let Ok(v) = header::HeaderValue::from_str(&c) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

/// `GET /api/v1/auth/status` — the public posture a client needs to render the right gate
/// (login vs first-run claim). Deliberately unauthenticated: it reveals only what the login page
/// itself would.
async fn status(State(st): State<AppState>) -> Result<Json<AccountsStatus>, ApiError> {
    require_enabled(&st)?;
    Ok(Json(AccountsStatus {
        enabled: true,
        unclaimed: st.store.unclaimed(),
    }))
}

/// `POST /api/v1/auth/claim` — redeem the first-run claim: create the admin account and sign it
/// in. Honoured only while unclaimed, and only from loopback or with an Admin bearer (see module
/// docs). The race between two simultaneous claims is settled inside the store under its lock.
async fn claim(
    State(st): State<AppState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Json(req): Json<ClaimRequest>,
) -> Result<Response, ApiError> {
    require_enabled(&st)?;
    if !st.store.unclaimed() {
        return Err(ApiError(LibError::Conflict(
            "this instance is already claimed".into(),
        )));
    }
    // Gate 1: a loopback peer (or a loopback-only bind, where every peer is local by construction).
    let loopback = st.localhost_only || peer.map(|p| p.ip().is_loopback()).unwrap_or(false);
    // Gate 2 (off-box): an Admin-scoped bearer — the bootstrap-token redemption path.
    let admin_bearer = auth::bearer_header(&headers)
        .and_then(|t| auth::resolve(&st.store, Some(t), None).ok())
        .map(|r| r.ctx.scopes.has(Scope::Admin))
        .unwrap_or(false);
    if !loopback && !admin_bearer {
        return Err(ApiError(LibError::Forbidden(
            "an unclaimed instance can only be claimed from localhost (or with the bootstrap \
             owner token as a bearer credential)"
                .into(),
        )));
    }
    let account = st.store.claim(&req, "claim")?;
    let ident = AccountIdentity {
        account_id: account.account_id.clone(),
        username: account.username.clone(),
        role: account.role,
    };
    let sess = st
        .store
        .mint_session(&account.account_id, user_agent(&headers).as_deref())?;
    tracing::info!(username = %account.username, "instance claimed; first admin account created");
    let cookies = session_cookies(&sess, st.tls);
    Ok(with_cookies(
        Json(LoginReply {
            account: ident,
            csrf: sess.csrf,
        }),
        cookies,
    ))
}

/// `POST /api/v1/auth/login` — username/password → session cookie + CSRF token. Lockout after 10
/// failures / 15 min (ADR 0009 §4) surfaces as 429.
async fn login(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    require_enabled(&st)?;
    let (sess, ident) = st.store.login(
        &req.username,
        &req.password,
        user_agent(&headers).as_deref(),
    )?;
    let cookies = session_cookies(&sess, st.tls);
    Ok(with_cookies(
        Json(LoginReply {
            account: ident,
            csrf: sess.csrf,
        }),
        cookies,
    ))
}

/// `POST /api/v1/auth/logout` — revoke the presented session and expire the cookies.
async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    require_enabled(&st)?;
    let session = auth::session_user(&st.store, &headers, true)?;
    st.store
        .revoke_session(&session.account_id, &session.session_id)?;
    Ok(with_cookies(StatusCode::NO_CONTENT, clear_cookies()))
}

/// `GET /api/v1/auth/sessions` — the caller's own sessions, current one marked.
async fn list_sessions(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SessionInfo>>, ApiError> {
    require_enabled(&st)?;
    let session = auth::session_user(&st.store, &headers, false)?;
    Ok(Json(
        st.store
            .list_sessions(&session.account_id, &session.session_id)?,
    ))
}

/// `DELETE /api/v1/auth/sessions/{id}` — revoke one of the caller's own sessions (a stray login).
async fn revoke_session(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    require_enabled(&st)?;
    let session = auth::session_user(&st.store, &headers, true)?;
    st.store.revoke_session(&session.account_id, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        // Bound what we store — a UA string is display metadata, not a log.
        .map(|s| s.chars().take(200).collect())
}
