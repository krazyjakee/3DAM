//! The `/api/v1/auth` surface (phase 6, issue #42): first-run claim, login/logout, and the
//! caller's own session management. Behind the `UserAccounts` flag — off ⇒ every route here 404s
//! ("off removes the surface", ADR 0004), checked before auth so a disabled surface cannot be
//! probed.
//!
//! ## The claim gate (issue #42 §2, ADR 0014)
//!
//! An unclaimed instance (accounts on, zero accounts) is a land-grab risk — the first-run race
//! Jellyfin/Grafana shipped as CVEs. The open claim window therefore demands **positive evidence of
//! a local peer**, and treats any evidence to the contrary as disqualifying:
//! - the peer socket must actually be loopback. The *bind* posture is not evidence: `localhost_only`
//!   is just "we bound 127.0.0.1", which is precisely the deployment `docs/DEPLOYMENT.md`
//!   recommends (loopback behind nginx/Caddy) — there every remote visitor's peer is 127.0.0.1 too;
//! - a request carrying `X-Forwarded-For`, `X-Real-IP`, or `Forwarded` was proxied and can never
//!   take the open path, whatever its peer address says;
//! - `[accounts] require_claim_token = true` closes the open path entirely, for an operator who
//!   knows they sit behind a proxy that strips those headers;
//! - off-box (and under all of the above), the claim degrades to **token redemption**: a caller
//!   presenting an Admin-scoped bearer (the bootstrap owner token minted when the gate went up) may
//!   claim from anywhere. That path is unconditional — it is the documented headless route;
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

/// The whole `/api/v1/auth` surface, behind **one** router-level `UserAccounts` gate. ADR 0004 says
/// off *unmounts* the route, and every handler here is gated identically, so the guard belongs on
/// the router — not pasted into six handler bodies where the seventh would forget it.
pub fn routes(st: AppState) -> Router<AppState> {
    let r = Router::new()
        .route("/api/v1/auth/status", get(status))
        .route("/api/v1/auth/claim", post(claim))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/sessions", get(list_sessions))
        .route(
            "/api/v1/auth/sessions/{id}",
            axum::routing::delete(revoke_session),
        );
    crate::gate_accounts(r, st)
}

/// Header names a reverse proxy adds to a forwarded request. Their **presence** is what matters
/// here, never their value: we are not trying to recover the real client IP (that needs a trusted
/// proxy list we deliberately don't have), only to notice that this request did not come straight
/// off a local socket. A client that forges one of these merely locks *itself* out of the open
/// claim path — the safe direction to fail.
const FORWARD_HEADERS: [&str; 3] = ["x-forwarded-for", "x-real-ip", "forwarded"];

fn looks_proxied(headers: &HeaderMap) -> bool {
    FORWARD_HEADERS.iter().any(|h| headers.contains_key(*h))
}

/// The `Set-Cookie` pair for a fresh session: the `HttpOnly` session cookie and the JS-readable
/// CSRF cookie (double-submit). `Max-Age` matches the 90-day absolute ceiling — the 14-day
/// inactivity expiry is enforced server-side.
///
/// `secure` is `tls || [server] secure_cookies`: this process's own TLS posture, *or* the
/// operator's declaration that a TLS-terminating proxy sits in front. Without the second term a
/// proxied deployment would ship a 90-day session cookie with no `Secure` attribute.
///
/// **`SameSite=Lax`, not `Strict`** *(amended for issue #41)*. There is one session cookie, so
/// there can only be one answer, and OIDC forces it: the callback arrives as a cross-site
/// navigation from the identity provider, and `Strict` withholds the cookie on exactly that class
/// of request — a user would finish a correct login and land looking signed out. `Lax` is not a
/// meaningful loss here, because `SameSite` was never what protected writes on this surface: `Lax`
/// still withholds the cookie from cross-site POST/PUT/DELETE, and every cookie-authenticated
/// mutation is additionally gated on the double-submit `x-dam-csrf` token (`auth::enforce_csrf`),
/// which an attacker cannot read cross-origin. Shared with `crate::oidc` so the two login paths
/// cannot drift into issuing the same cookie with different attributes.
pub(crate) fn session_cookies(sess: &NewSession, secure: bool) -> [String; 2] {
    let secure = if secure { "; Secure" } else { "" };
    let max_age = 90 * 24 * 60 * 60;
    [
        format!(
            "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
            sess.cookie_value
        ),
        format!(
            "{CSRF_COOKIE}={}; Path=/; SameSite=Lax; Max-Age={max_age}{secure}",
            sess.csrf
        ),
    ]
}

/// Expire both cookies (logout). `SameSite` matches [`session_cookies`] — a browser matches the
/// cookie to overwrite on name/path/domain, and keeping the attributes aligned avoids leaving a
/// stale twin behind under a different `SameSite`.
fn clear_cookies() -> [String; 2] {
    [
        format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
        format!("{CSRF_COOKIE}=; Path=/; SameSite=Lax; Max-Age=0"),
    ]
}

/// Attach cookies to a response by **appending** each `Set-Cookie` line. A response carries two of
/// them (session + CSRF), and the tuple/array `IntoResponse` impls *insert* by header name — which
/// would silently drop the first cookie — so the two are appended explicitly here.
pub(crate) fn with_cookies(body: impl IntoResponse, cookies: [String; 2]) -> Response {
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
    if !st.store.unclaimed() {
        return Err(ApiError(LibError::Conflict(
            "this instance is already claimed".into(),
        )));
    }
    // Gate 1 — the open path. Positive evidence of a local peer only: an actual loopback peer
    // socket, no forwarding header, and no operator opt-out. The bind posture is *not* evidence
    // (see the module docs: behind a same-host proxy it is true for every internet visitor).
    let peer_loopback = peer.map(|p| p.ip().is_loopback()).unwrap_or(false);
    let open_claim = peer_loopback && !looks_proxied(&headers) && !st.require_claim_token;
    // Gate 2 — an Admin-scoped bearer: the bootstrap-token redemption path. Unconditional, so a
    // headless/proxied/remote deployment always keeps a first-class way in (ADR 0014).
    let admin_bearer = auth::bearer_header(&headers)
        .and_then(|t| auth::resolve(&st.store, Some(t), None).ok())
        .map(|r| r.ctx.scopes.has(Scope::Admin))
        .unwrap_or(false);
    if !open_claim && !admin_bearer {
        tracing::warn!(
            peer = ?peer, proxied = looks_proxied(&headers),
            "refused a first-run claim: not a direct loopback peer and no bootstrap owner token"
        );
        return Err(ApiError(LibError::Forbidden(
            "an unclaimed instance can only be claimed from a process on this machine, or with \
             the bootstrap owner token as a bearer credential (see the server log for its path)"
                .into(),
        )));
    }
    // argon2id is deliberately expensive; it belongs on the blocking pool, never on a tokio worker
    // (CLAUDE.md golden rule 5 — the engine wraps every store call the same way).
    let store = st.store.clone();
    let account = tokio::task::spawn_blocking(move || store.claim(&req, "claim"))
        .await
        .map_err(|e| ApiError(LibError::Internal(e.to_string())))??;
    let ident = AccountIdentity {
        account_id: account.account_id.clone(),
        username: account.username.clone(),
        role: account.role,
    };
    let sess = st
        .store
        .mint_session(&account.account_id, user_agent(&headers).as_deref())?;
    tracing::info!(username = %account.username, "instance claimed; first admin account created");
    let cookies = session_cookies(&sess, st.tls || st.secure_cookies);
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
    // The password verify is a KDF: run it on the blocking pool so a burst of failed logins cannot
    // pin tokio's workers (see `ServerStore::login`, which also keeps it off the DB mutex).
    let store = st.store.clone();
    let ua = user_agent(&headers);
    let (sess, ident) = tokio::task::spawn_blocking(move || {
        store.login(&req.username, &req.password, ua.as_deref())
    })
    .await
    .map_err(|e| ApiError(LibError::Internal(e.to_string())))??;
    let cookies = session_cookies(&sess, st.tls || st.secure_cookies);
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
