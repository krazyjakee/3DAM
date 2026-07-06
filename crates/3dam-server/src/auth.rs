//! The auth layer (tech-spec 10 §1) — turns a request into an [`AuthContext`] and guards a scope.
//!
//! One resolution path serves the whole surface (API, MCP, admin): read the presented bearer
//! credential, consult the `Authentication` flag, and produce the granted scopes (§1.2). Rather than
//! a separate middleware + extension, the policy is expressed as three axum **extractors** — a
//! handler that needs a capability names it in its signature (`Reader`, `Writer`, `AdminAuth`) and
//! the guard runs before the body does. A handler with no such extractor (the SPA shell, the
//! version probe) is deliberately public.

use crate::store::ServerStore;
use crate::{ApiError, AppState};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use dam_api::admin::AuthMode;
use dam_api::service::{AuthContext, Scope, Scopes};
use dam_api::LibError;

/// Pull the `Authorization: Bearer <secret>` credential from headers, if present.
pub fn bearer_header(headers: &HeaderMap) -> Option<String> {
    let h = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = h.to_str().ok()?;
    s.strip_prefix("Bearer ")
        .or_else(|| s.strip_prefix("bearer "))
        .map(|t| t.trim().to_string())
}

fn bearer(parts: &Parts) -> Option<String> {
    bearer_header(&parts.headers)
}

/// Resolve a request to its [`AuthContext`] under the current auth mode (tech-spec 10 §1.2).
pub fn resolve(store: &ServerStore, token: Option<String>) -> Result<AuthContext, LibError> {
    let verify = |secret: &str| -> Result<AuthContext, LibError> {
        match store.verify_token(secret)? {
            Some((label, scopes)) => Ok(AuthContext::connected(Some(label), scopes)),
            None => Err(LibError::Unauthorized),
        }
    };
    match store.auth_mode() {
        // Off: no credential inspected; the unauthenticated caller is the local owner (full trust),
        // so a localhost operator is never locked out of their own admin surface.
        AuthMode::Off => Ok(AuthContext::connected(None, Scopes::owner())),
        // Anonymous: a valid credential elevates; otherwise the fixed anonymous scope set.
        AuthMode::Anonymous => match token {
            Some(secret) => verify(&secret),
            None => Ok(AuthContext::connected(None, Scopes::anonymous())),
        },
        // Token: a credential is required — no credential is a 401.
        AuthMode::Token => match token {
            Some(secret) => verify(&secret),
            None => Err(LibError::Unauthorized),
        },
    }
}

/// A context that holds `Read` — every browse/search/preview handler.
pub struct Reader(pub AuthContext);
impl FromRequestParts<AppState> for Reader {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let ctx = resolve(&st.store, bearer(parts))?;
        ctx.require(Scope::Read)?;
        Ok(Reader(ctx))
    }
}

/// A context authorised to **write**: it holds `Write` *and* writes are permitted on this bind — the
/// network-level ceiling (tech-spec 10 §4.2). Writes default off beyond localhost; the
/// `NetworkWrites` flag opens them. Localhost is always allowed to write (the owner's own machine).
pub struct Writer(pub AuthContext);
impl FromRequestParts<AppState> for Writer {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let ctx = resolve(&st.store, bearer(parts))?;
        ctx.require(Scope::Write)?;
        if !st.localhost_only && !st.store.network_writes() {
            return Err(ApiError(LibError::Disabled(
                "network writes are disabled (enable the network_writes flag)".into(),
            )));
        }
        Ok(Writer(ctx))
    }
}

/// A context that holds `Admin` — the entire `/admin/api` surface. Once auth is on this is never
/// reachable anonymously (ADR 0004); under `Off` the local owner holds it (see [`Scopes::owner`]).
pub struct AdminAuth(pub AuthContext);
impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let ctx = resolve(&st.store, bearer(parts))?;
        ctx.require(Scope::Admin)?;
        Ok(AdminAuth(ctx))
    }
}
