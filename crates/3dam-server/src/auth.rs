//! The auth layer (tech-spec 10 §1) — turns a request into an [`AuthContext`] and guards a scope.
//!
//! One resolution path serves the whole surface (API, MCP, admin): read the presented credential
//! (bearer token, or — with `UserAccounts` on — a session cookie), consult the `Authentication`
//! flag, and produce the granted scopes (§1.2) plus the resolved visibility ceiling (§4.3).
//! Rather than a separate middleware + extension, the policy is expressed as three axum
//! **extractors** — a handler that needs a capability names it in its signature (`Reader`,
//! `Writer`, `AdminAuth`) and the guard runs before the body does. A handler with no such
//! extractor (the SPA shell, the version probe) is deliberately public.
//!
//! Sessions (phase 6, issue #42): the cookie value is `<session_id>.<secret>`; the secret half is
//! stored only as a hash. Cookie-authenticated **mutations** additionally require the double-submit
//! CSRF token in `x-dam-csrf` (`SameSite=Strict` is the first fence, this is the second —
//! ADR 0009 §4). Bearer tokens need no CSRF: a cross-site attacker cannot attach a header
//! credential in the first place.

use crate::store::ServerStore;
use crate::{ApiError, AppState};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use dam_api::admin::AuthMode;
use dam_api::service::{AuthContext, AuthSource, Scope, Scopes, Visibility};
use dam_api::LibError;

/// The session cookie name. `HttpOnly`; value `<session_id>.<secret>`.
pub const SESSION_COOKIE: &str = "dam_session";
/// The CSRF cookie name. Readable by the SPA, echoed back in [`CSRF_HEADER`] on writes.
pub const CSRF_COOKIE: &str = "dam_csrf";
/// The double-submit CSRF header a cookie-authenticated mutation must carry.
pub const CSRF_HEADER: &str = "x-dam-csrf";

/// What a request resolved to: the context every handler consumes, plus — when the credential was
/// a session cookie — the session identity the CSRF check and the `/auth/sessions` surface need.
pub struct Resolved {
    pub ctx: AuthContext,
    pub session: Option<SessionAuth>,
}

/// The cookie-session half of a [`Resolved`] request.
#[derive(Clone)]
pub struct SessionAuth {
    pub session_id: String,
    pub account_id: String,
    pub csrf: String,
}

/// Pull the `Authorization: Bearer <secret>` credential from headers, if present.
pub fn bearer_header(headers: &HeaderMap) -> Option<String> {
    let h = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = h.to_str().ok()?;
    s.strip_prefix("Bearer ")
        .or_else(|| s.strip_prefix("bearer "))
        .map(|t| t.trim().to_string())
}

/// Pull one cookie's value out of the `Cookie` header (no percent-decoding — our values are
/// hex/uuid-safe by construction).
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k == name && !v.is_empty()).then(|| v.to_string())
    })
}

fn bearer(parts: &Parts) -> Option<String> {
    bearer_header(&parts.headers)
}

/// Resolve auth for a live-event WebSocket ticket mint, which needs `Read`. The mint is an ordinary
/// fetch and therefore uses the Authorization header or same-origin session cookie. The long-lived
/// credential is never accepted from a URI (issue #128).
pub async fn resolve_ws(st: &AppState, headers: &HeaderMap) -> Result<Resolved, LibError> {
    let token = bearer_header(headers);
    let resolved = resolve_request(st, token, cookie_value(headers, SESSION_COOKIE)).await?;
    resolved.ctx.require(Scope::Read)?;
    Ok(resolved)
}

/// Resolve the credentials carried by an HTTP request.
///
/// Native `dam_…` API keys have explicit precedence and are never interpreted as JWTs, including
/// when invalid. Only a three-segment non-native bearer reaches the OIDC verifier; opaque legacy
/// API keys still use the native store lookup. This keeps the existing token contract deterministic
/// and prevents a bad native key from causing outbound issuer traffic.
pub async fn resolve_request(
    st: &AppState,
    token: Option<String>,
    session_cookie: Option<String>,
) -> Result<Resolved, LibError> {
    if matches!(st.store.effective_auth_mode(), AuthMode::Off) {
        return resolve(&st.store, None, None);
    }
    let Some(secret) = token else {
        return resolve(&st.store, None, session_cookie);
    };
    if secret.starts_with("dam_") || secret.split('.').count() != 3 {
        return resolve(&st.store, Some(secret), None);
    }

    let ctx = crate::oidc_bearer::verify(st, &secret).await?;
    Ok(Resolved { ctx, session: None })
}

/// Resolve a request to its [`AuthContext`] under the current auth mode (tech-spec 10 §1.2).
///
/// Precedence within a mode: an explicit bearer token wins; otherwise a valid session cookie
/// (accounts on) authenticates; otherwise the mode's fallback (owner / anonymous / 401). Under
/// `Off` no credential is inspected at all — but note that `UserAccounts = on` raises the
/// *effective* mode to at least `Token` (`ServerStore::effective_auth_mode`), so accounts and
/// unauthenticated owner trust never coexist.
pub fn resolve(
    store: &ServerStore,
    token: Option<String>,
    session_cookie: Option<String>,
) -> Result<Resolved, LibError> {
    let verify_token = |secret: &str| -> Result<Resolved, LibError> {
        match store.verify_token(secret)? {
            // A token is an operator-issued credential with no share graph behind it: its reach is
            // the whole library, capped only by its scopes (tech-spec 10 §4.3).
            Some((label, scopes)) => Ok(Resolved {
                ctx: AuthContext::connected(Some(label), scopes, Visibility::Full),
                session: None,
            }),
            None => Err(LibError::Unauthorized),
        }
    };
    let verify_session = |cookie: &str| -> Result<Option<Resolved>, LibError> {
        if !store.user_accounts() {
            return Ok(None);
        }
        let Some((ident, csrf, session_id)) = store.verify_session(cookie)? else {
            return Ok(None); // expired/garbage cookie falls through to the mode default
        };
        let vis = store.resolve_visibility(&ident)?;
        let ctx = AuthContext::connected(Some(ident.username.clone()), ident.role.scopes(), vis)
            .with_account(ident.clone())
            .with_auth_source(AuthSource::Session);
        Ok(Some(Resolved {
            ctx,
            session: Some(SessionAuth {
                session_id,
                account_id: ident.account_id,
                csrf,
            }),
        }))
    };
    match store.effective_auth_mode() {
        // Off: no credential inspected; the unauthenticated caller is the local owner (full trust),
        // so a localhost operator is never locked out of their own admin surface.
        AuthMode::Off => Ok(Resolved {
            ctx: AuthContext::connected(None, Scopes::owner(), Visibility::Full)
                .with_auth_source(AuthSource::LocalOwner),
            session: None,
        }),
        // Anonymous: a valid credential elevates; otherwise the fixed anonymous scope set.
        AuthMode::Anonymous => {
            if let Some(secret) = token {
                return verify_token(&secret);
            }
            if let Some(cookie) = session_cookie {
                if let Some(r) = verify_session(&cookie)? {
                    return Ok(r);
                }
            }
            Ok(Resolved {
                ctx: AuthContext::connected(None, Scopes::anonymous(), Visibility::Full)
                    .with_auth_source(AuthSource::Anonymous),
                session: None,
            })
        }
        // Token: a credential is required — no valid credential is a 401.
        AuthMode::Token => {
            if let Some(secret) = token {
                return verify_token(&secret);
            }
            if let Some(cookie) = session_cookie {
                if let Some(r) = verify_session(&cookie)? {
                    return Ok(r);
                }
            }
            Err(LibError::Unauthorized)
        }
    }
}

/// The double-submit check for cookie-authenticated mutations: the `x-dam-csrf` header must match
/// the session's stored token. Bearer/owner/anonymous callers pass untouched.
fn enforce_csrf(parts: &Parts, resolved: &Resolved) -> Result<(), LibError> {
    let Some(sess) = &resolved.session else {
        return Ok(());
    };
    let presented = parts.headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok());
    if presented == Some(sess.csrf.as_str()) {
        Ok(())
    } else {
        Err(LibError::Forbidden(
            "missing or invalid CSRF token (send the login csrf in x-dam-csrf)".into(),
        ))
    }
}

/// A context that holds `Read` — every browse/search/preview handler.
pub struct Reader(pub AuthContext);
impl FromRequestParts<AppState> for Reader {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        if let Some(ticket) = crate::tickets::resolve_media_ticket(st, parts).await {
            return Ok(Reader(ticket?));
        }
        // Browser media is fetched with this same header and converted to a local blob URL; raw
        // bearer query credentials are deliberately not supported (issue #128).
        let resolved = resolve_request(
            st,
            bearer(parts),
            cookie_value(&parts.headers, SESSION_COOKIE),
        )
        .await?;
        resolved.ctx.require(Scope::Read)?;
        Ok(Reader(resolved.ctx))
    }
}

/// A context authorised to **write**: it holds `Write` and, when the caller has **no verified
/// credential**, writes are permitted on this bind — the network ceiling (tech-spec 10 §4.2) caps
/// *implicit trust* (the auth-off owner posture, anonymous callers), not authenticated identities.
/// A verified token's scopes alone decide (front-door auth: one gate, then permissions). Implicit
/// writes default off beyond localhost; the `NetworkWrites` flag opens them; localhost is always
/// allowed (the owner's own machine). Cookie-session writes additionally pass the CSRF gate.
pub struct Writer(pub AuthContext);
impl FromRequestParts<AppState> for Writer {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let resolved = resolve_request(
            st,
            bearer(parts),
            cookie_value(&parts.headers, SESSION_COOKIE),
        )
        .await?;
        resolved.ctx.require(Scope::Write)?;
        enforce_csrf(parts, &resolved)?;
        // identity is Some exactly for store-verified credentials (see `resolve`).
        if resolved.ctx.identity.is_none() && !st.localhost_only && !st.store.network_writes() {
            return Err(ApiError(LibError::Disabled(
                "network writes are disabled (enable the network_writes flag)".into(),
            )));
        }
        Ok(Writer(resolved.ctx))
    }
}

/// A context that may take part in a **discussion** (issue #82): it holds `Read`, and a cookie
/// session passes CSRF on anything that mutates.
///
/// Deliberately `Read` and not `Write`. Requiring `Scope::Write` would conflate "may modify the
/// library" with "may talk about it" and lock out the reviewing art director or client — precisely
/// the person the feature exists for, and precisely the person a `viewer` account models. The
/// identity requirement lives one layer down in the engine, which rejects anyone without a
/// signed-in account, so widening the scope here does not widen who can post.
pub struct Commenter(pub AuthContext);
impl FromRequestParts<AppState> for Commenter {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let resolved = resolve_request(
            st,
            bearer(parts),
            cookie_value(&parts.headers, SESSION_COOKIE),
        )
        .await?;
        resolved.ctx.require(Scope::Read)?;
        if parts.method != axum::http::Method::GET {
            enforce_csrf(parts, &resolved)?;
        }
        Ok(Commenter(resolved.ctx))
    }
}

/// A context that holds `Admin` — the entire `/admin/api` surface. Once auth is on this is never
/// reachable anonymously (ADR 0004); under `Off` the local owner holds it (see [`Scopes::owner`]).
/// The whole admin surface mutates or reads privileged state, so cookie sessions pass CSRF here
/// regardless of method.
pub struct AdminAuth(pub AuthContext);
impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let resolved = resolve_request(
            st,
            bearer(parts),
            cookie_value(&parts.headers, SESSION_COOKIE),
        )
        .await?;
        resolved.ctx.require(Scope::Admin)?;
        if parts.method != axum::http::Method::GET {
            enforce_csrf(parts, &resolved)?;
        }
        Ok(AdminAuth(resolved.ctx))
    }
}

/// Resolve the session behind a cookie-authenticated request — the `/api/v1/auth` self-service
/// surface (logout, session list/revoke). 401 when the request carries no valid session (a bearer
/// token is *not* a session); with `csrf_required`, the double-submit header must match.
pub fn session_user(
    store: &ServerStore,
    headers: &HeaderMap,
    csrf_required: bool,
) -> Result<SessionAuth, LibError> {
    let resolved = resolve(store, None, cookie_value(headers, SESSION_COOKIE))?;
    let Some(session) = resolved.session else {
        return Err(LibError::Unauthorized);
    };
    if csrf_required {
        let presented = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok());
        if presented != Some(session.csrf.as_str()) {
            return Err(LibError::Forbidden(
                "missing or invalid CSRF token (send the login csrf in x-dam-csrf)".into(),
            ));
        }
    }
    Ok(session)
}
