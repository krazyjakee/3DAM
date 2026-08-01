//! OIDC / OAuth2 login (phase 6, issue #41; tech-spec 10 §1.5).
//!
//! Authorization Code + PKCE. The browser is redirected to the issuer, comes back with a code, and
//! the server — never the browser — exchanges it. On success the caller gets an **ordinary server
//! session** (the same cookie password login mints), so everything downstream of `AuthContext` is
//! unchanged: OIDC alters *how* a context is populated, not who consumes it.
//!
//! ## What actually makes this safe
//!
//! The interesting failures of this flow are all in details that are easy to leave out, so they are
//! named here rather than left implicit:
//!
//! - **PKCE (S256).** The verifier lives in `server.db`, never in a cookie or the URL, so an
//!   intercepted authorization code cannot be redeemed by whoever intercepted it.
//! - **`state` is single-use.** It is deleted as it is read (`DELETE … RETURNING`), so a captured
//!   callback URL cannot be replayed, and it expires after ten minutes regardless.
//! - **`state` is bound to the browser that started the login.** Single-use alone does not stop
//!   *login CSRF*: an attacker can start their own login, authenticate at the provider as
//!   themselves, and then feed the victim the resulting callback URL — the victim's browser
//!   redeems a perfectly valid code and is silently signed in **as the attacker**, so everything
//!   they then do happens inside the attacker's library. `/start` therefore sets a short-lived
//!   `HttpOnly` `dam_oidc` cookie and stores only its hash; the callback is refused unless the
//!   browser presents the matching cookie, which an attacker cannot set on this origin.
//! - **`nonce` is bound into the ID token** and checked on the way back, which is what stops a
//!   token minted for one login being injected into another.
//! - **Signature, `iss`, `aud`, `exp` are verified against the issuer's JWKS** by `openidconnect`.
//!   This is the reason a library is used at all — it is the part where rolling our own would be a
//!   mistake, not a preference.
//! - **`return_to` is a path, not a URL.** Anything else is an open redirect wearing a login flow
//!   as a disguise, so the value is refused unless it is a single-slash-rooted local path.
//! - **An unknown-but-valid subject is refused by default.** For most issuers *anyone* may hold a
//!   valid account, so "the provider says this token is good" is not authority to create an account
//!   here. Auto-provisioning exists, but an operator has to choose it (tech-spec 10 §1.5).
//!
//! ## Cost, knowingly accepted for now
//!
//! Provider metadata (and its JWKS) is re-discovered on each leg rather than cached, so a login
//! costs a couple of extra round trips to the issuer. That is the simple, always-correct behaviour
//! — it can never serve a stale signing key after a rotation — and caching it is a contained
//! optimisation for when it matters.

use crate::auth_rate::{self, Endpoint, PeerAddr};
use crate::authn::{session_cookies, with_cookies};
use crate::store::oidc::{StoredOidc, TakeLogin};
use crate::{ApiError, AppState};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dam_api::accounts::Role;
use dam_api::admin::OidcProvisioning;
use dam_api::LibError;
use openidconnect::core::{CoreClient, CoreProviderMetadata, CoreResponseType};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};

/// Cookie tying an in-flight authorization request to the browser that began it. Short-lived,
/// `HttpOnly`, and cleared as soon as the callback consumes it.
const LOGIN_COOKIE: &str = "dam_oidc";

/// The whole `/api/v1/auth/oidc` surface, behind one router-level gate — the same shape the
/// accounts and upload surfaces use, so a new route here cannot forget the check.
pub fn routes(st: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/oidc/start", get(start))
        .route("/api/v1/auth/oidc/callback", get(callback))
        .route_layer(axum::middleware::from_fn_with_state(st, gate))
}

async fn gate(
    State(st): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match st.store.require_oidc() {
        Ok(()) => next.run(req).await,
        Err(e) => ApiError(e).into_response(),
    }
}

/// Fetch the provider's discovery document, plus the HTTP client to use for the rest of the leg.
///
/// Returns the metadata rather than a ready-made client because `openidconnect` 4 tracks which
/// endpoints are configured in the *type* — `from_provider_metadata` yields a client whose auth and
/// token endpoints are statically "set", which is a different type from `CoreClient` and one that
/// takes seventeen generic parameters to name. Handing back the metadata and building the client at
/// each use site costs two lines and keeps that type inferred; see [`build_client`].
///
/// The HTTP client refuses redirects deliberately: `openidconnect` documents this as an SSRF guard,
/// since without it a hostile or compromised issuer could bounce these server-side requests at
/// anything reachable from the server, including its own loopback services.
async fn discover(cfg: &StoredOidc) -> Result<(CoreProviderMetadata, reqwest::Client), LibError> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // Timeouts, because `/start` is unauthenticated and every call here is an outbound request
        // to a third party. Without them a slow or hostile issuer parks server tasks indefinitely,
        // and anyone able to reach the login route can make that happen at will.
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| LibError::Internal(format!("http client: {e}")))?;
    let issuer = IssuerUrl::new(cfg.config.issuer.clone())
        .map_err(|e| LibError::BadRequest(format!("issuer is not a URL: {e}")))?;
    let metadata = CoreProviderMetadata::discover_async(issuer, &http)
        .await
        .map_err(|e| LibError::Internal(format!("OIDC discovery failed: {e}")))?;
    Ok((metadata, http))
}

/// The redirect URI, parsed. Kept separate so both legs fail the same way on a bad config.
fn redirect_uri(cfg: &StoredOidc) -> Result<RedirectUrl, LibError> {
    RedirectUrl::new(cfg.config.redirect_url.clone())
        .map_err(|e| LibError::BadRequest(format!("redirect_url is not a URL: {e}")))
}

/// Assemble the client from discovered metadata. A macro-free two-liner so the caller keeps the
/// inferred typestate; see [`discover`] for why it is not a named return type.
macro_rules! build_client {
    ($metadata:expr, $cfg:expr) => {
        CoreClient::from_provider_metadata(
            $metadata,
            ClientId::new($cfg.config.client_id.clone()),
            $cfg.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(redirect_uri(&$cfg)?)
    };
}

fn provider(st: &AppState) -> Result<StoredOidc, LibError> {
    st.store.oidc_provider()?.ok_or_else(|| {
        LibError::BadRequest(
            "no OIDC provider is configured — set one with PUT /admin/api/oidc".into(),
        )
    })
}

/// Accept only a rooted, local path as the post-login destination.
///
/// `//evil.example` is the case worth naming: browsers read a leading `//` as protocol-relative, so
/// it is an absolute URL that merely looks like a path. Anything with a scheme, a backslash (which
/// some clients normalise to `/`), or a control character is refused for the same reason. The point
/// is that a login link a stranger sends must not be able to choose where you land afterwards.
fn safe_return_to(raw: Option<&str>) -> Option<String> {
    let p = raw?;
    if !p.starts_with('/') || p.starts_with("//") {
        return None;
    }
    if p.contains('\\') || p.contains(|c: char| c.is_control()) {
        return None;
    }
    Some(p.to_string())
}

#[derive(serde::Deserialize)]
struct StartQuery {
    /// Where to land after a successful login. Validated by [`safe_return_to`].
    #[serde(default)]
    return_to: Option<String>,
}

/// `GET /api/v1/auth/oidc/start` — begin the authorization code flow.
///
/// A 302 rather than JSON: this is a link the browser follows, and the client should not have to
/// understand the provider to start a login.
async fn start(
    State(st): State<AppState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Query(q): Query<StartQuery>,
) -> Result<Response, ApiError> {
    auth_rate::enforce(&st, Endpoint::OidcStart, peer, &headers, None)?;
    let cfg = provider(&st)?;
    // Before the outbound request, not after: `/start` is unauthenticated, so the cheap refusal
    // has to come first or a flood still costs one round trip to the provider each.
    st.store.check_oidc_login_capacity()?;
    let discovery_permit = st.auth_protection.discovery_permit()?;
    let (metadata, _http) = discover(&cfg).await?;
    drop(discovery_permit);
    let client = build_client!(metadata, cfg);

    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut req = client.authorize_url(
        AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
        CsrfToken::new_random,
        Nonce::new_random,
    );
    for s in &cfg.config.scopes {
        req = req.add_scope(Scope::new(s.clone()));
    }
    let (auth_url, csrf, nonce) = req.set_pkce_challenge(challenge).url();

    // The browser half of the state binding: a high-entropy value the browser keeps in a cookie,
    // of which the server stores only a hash. See the module docs — this is what makes the callback
    // answerable *only* by the browser that started this login.
    let browser_token = format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    );
    st.store.begin_oidc_login(
        csrf.secret(),
        nonce.secret(),
        verifier.secret(),
        safe_return_to(q.return_to.as_deref()).as_deref(),
        &blake3::hash(browser_token.as_bytes()).to_hex(),
    )?;

    let secure = if st.tls || st.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    // `SameSite=Lax` for the same reason the session cookie is: the callback arrives as a
    // cross-site navigation from the provider, and `Strict` would withhold this cookie there —
    // turning every login into a refusal. `Max-Age` matches the login TTL.
    let cookie = format!(
        "{LOGIN_COOKIE}={browser_token}; Path=/api/v1/auth/oidc; HttpOnly; SameSite=Lax; \
         Max-Age=600{secure}"
    );
    let mut resp = (
        StatusCode::FOUND,
        [(header::LOCATION, auth_url.to_string())],
    )
        .into_response();
    if let Ok(v) = header::HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    Ok(resp)
}

/// Read the `dam_oidc` cookie out of the request.
fn login_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == LOGIN_COOKIE)
        .map(|(_, v)| v.trim().to_string())
}

/// Expire the in-flight-login cookie. Path must match the one it was set with.
fn clear_login_cookie() -> String {
    format!("{LOGIN_COOKIE}=; Path=/api/v1/auth/oidc; HttpOnly; SameSite=Lax; Max-Age=0")
}

#[derive(serde::Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    /// The issuer reports a refusal here (`access_denied` when the user declines consent).
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// `GET /api/v1/auth/oidc/callback` — redeem the code and sign the caller in.
async fn callback(
    State(st): State<AppState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    auth_rate::enforce(
        &st,
        Endpoint::OidcCallback,
        peer,
        &headers,
        q.state.as_deref(),
    )?;
    if let Some(err) = q.error {
        // These two are query parameters, so *anyone* can call this route and choose their content
        // — they are not evidence that a provider said anything. Echoing them back raw would make
        // this endpoint a convenient way to put chosen text in front of a user (and into logs), so
        // both are clipped to a sane length and stripped of anything but printable ASCII before
        // they appear anywhere.
        let clean = |s: String| -> String {
            s.chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .take(200)
                .collect()
        };
        let err = clean(err);
        let detail = clean(q.error_description.unwrap_or_default());
        st.store
            .audit("oidc", "account.oidc_login_failed", None, None)
            .ok();
        return Err(ApiError(LibError::Forbidden(format!(
            "the identity provider refused the login: {err} {detail}"
        ))));
    }
    let (code, state) = match (q.code, q.state) {
        (Some(c), Some(s)) => (c, s),
        _ => {
            return Err(ApiError(LibError::BadRequest(
                "callback is missing `code` or `state`".into(),
            )))
        }
    };

    // Reserve discovery capacity before consuming the single-use state. If the independent
    // outbound ceiling is busy, the browser receives a retryable 429 and can retry this same
    // callback; consuming state first would strand an otherwise completed provider login.
    // Redeeming the state proves this callback belongs to a login *we* started, and — because the
    // browser hash is checked as part of the same operation — that it belongs to a login *this
    // browser* started. The second half is what stops login CSRF: an attacker who completes an
    // honest login of their own and hands the victim the resulting URL. Without it the victim is
    // silently signed in as the attacker and works inside the attacker's library.
    let presented = login_cookie(&headers).unwrap_or_default();
    let presented_hash = blake3::hash(presented.as_bytes()).to_hex().to_string();
    let (discovery_permit, taken) = st
        .auth_protection
        .with_discovery_capacity(|| st.store.take_oidc_login(&state, &presented_hash))?;
    let pending = match taken {
        TakeLogin::Redeemed(p) => *p,
        TakeLogin::WrongBrowser => {
            st.store
                .audit(
                    "oidc",
                    "account.oidc_login_failed",
                    None,
                    Some(serde_json::json!({ "reason": "state_not_bound_to_this_browser" })),
                )
                .ok();
            return Err(ApiError(LibError::Forbidden(
                "this login was not started by this browser".into(),
            )));
        }
        TakeLogin::Unknown => {
            return Err(ApiError(LibError::Forbidden(
                "unknown, expired, or already-used login attempt".into(),
            )))
        }
    };

    let cfg = provider(&st)?;
    let (metadata, http) = discover(&cfg).await?;
    drop(discovery_permit);
    let client = build_client!(metadata, cfg);

    let tokens = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| LibError::Internal(format!("token exchange unavailable: {e}")))?
        .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
        .request_async(&http)
        .await
        .map_err(|e| LibError::Forbidden(format!("token exchange failed: {e}")))?;

    let id_token = tokens
        .id_token()
        .ok_or_else(|| LibError::Forbidden("the provider returned no ID token".into()))?;
    // This one call is the whole trust decision: signature against the issuer's JWKS, plus `iss`,
    // `aud`, `exp`, and the `nonce` binding this token to the login we started.
    let claims = id_token
        .claims(&client.id_token_verifier(), &Nonce::new(pending.nonce))
        .map_err(|e| LibError::Forbidden(format!("ID token failed validation: {e}")))?;

    // Key the identity on the *configured* issuer so it matches what `set_oidc_config` normalised
    // and stored. `iss` inside the token has already been checked against it by the verifier.
    let issuer = cfg.config.issuer.as_str();
    let subject = claims.subject().as_str().to_string();

    let account = match st.store.oidc_account(issuer, &subject)? {
        Some(id) => st.store.get_account(&id)?,
        None => provision(&st, &cfg, issuer, &subject, claims)?,
    };
    if account.disabled {
        return Err(ApiError(LibError::Forbidden("account is disabled".into())));
    }

    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(200).collect::<String>());
    let sess = st.store.mint_session(&account.account_id, ua.as_deref())?;
    st.store
        .audit(
            "oidc",
            "account.oidc_login",
            Some(&account.account_id),
            Some(serde_json::json!({ "issuer": issuer, "username": account.username })),
        )
        .ok();
    tracing::info!(username = %account.username, "OIDC login");

    // Land on the app, not on JSON: this response is rendered by a browser mid-redirect.
    let dest = pending.return_to.unwrap_or_else(|| "/".into());
    let mut resp = with_cookies(
        (StatusCode::FOUND, [(header::LOCATION, dest)]),
        session_cookies(&sess, st.tls || st.secure_cookies),
    );
    // The in-flight-login cookie has done its job; leaving it would keep a spent token in the
    // browser for its full ten minutes.
    if let Ok(v) = header::HeaderValue::from_str(&clear_login_cookie()) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    Ok(resp)
}

/// Create an account for a verified-but-unlinked subject, if policy allows it.
fn provision(
    st: &AppState,
    cfg: &StoredOidc,
    issuer: &str,
    subject: &str,
    claims: &openidconnect::IdTokenClaims<
        openidconnect::EmptyAdditionalClaims,
        openidconnect::core::CoreGenderClaim,
    >,
) -> Result<dam_api::accounts::AccountInfo, ApiError> {
    let role = match cfg.config.provisioning {
        // The v1 default (tech-spec 10 §1.5). The message says how to fix it, because from the
        // user's side a refusal here is indistinguishable from a broken provider.
        OidcProvisioning::Linked => {
            st.store
                .audit(
                    "oidc",
                    "account.oidc_login_failed",
                    None,
                    Some(serde_json::json!({ "issuer": issuer, "reason": "unlinked_subject" })),
                )
                .ok();
            return Err(ApiError(LibError::Forbidden(
                "this provider identity is not linked to an account on this instance; an \
                 administrator must link it first"
                    .into(),
            )));
        }
        OidcProvisioning::AutoViewer => Role::Viewer,
        OidcProvisioning::AutoEditor => Role::Editor,
    };
    // Clipped like `user_agent` is: `name` is provider-controlled text that lands in account lists
    // and comment attributions, and nothing else bounds it.
    let display = claims
        .name()
        .and_then(|n| n.get(None))
        .map(|n| n.as_str().chars().take(100).collect::<String>());
    let username = derive_username(claims, subject);
    Ok(st.store.provision_oidc_account(
        issuer,
        subject,
        &username,
        display.as_deref(),
        role,
        "oidc",
    )?)
}

/// Pick a local username for a provisioned account.
///
/// Preference order is `preferred_username`, then `email`, then the opaque `sub` — most useful
/// first, always ending somewhere that exists. A collision with an existing account is refused
/// upstream rather than merged, so this cannot be used to take one over.
///
/// Two narrowings on the raw claim, both because this becomes a name other users read:
/// - **`email` is only used when the provider says it is verified.** An unverified `email` claim is
///   a string the subject typed, so without this it picks its own local identity.
/// - **ASCII only.** `char::is_alphanumeric` is Unicode-wide, but `account.username` is
///   `UNIQUE COLLATE NOCASE`, which is ASCII-only — so `аlice` (Cyrillic а) is a *distinct* account
///   that renders identically to `alice`. Uniqueness would not catch it and a human would not see
///   it. Characters outside ASCII are dropped rather than rejected, so a purely non-Latin name
///   still lands on the `sub`-derived fallback instead of failing the login.
fn derive_username(
    claims: &openidconnect::IdTokenClaims<
        openidconnect::EmptyAdditionalClaims,
        openidconnect::core::CoreGenderClaim,
    >,
    subject: &str,
) -> String {
    let verified_email = claims
        .email_verified()
        .unwrap_or(false)
        .then(|| claims.email().map(|e| e.as_str().to_string()))
        .flatten();
    let candidate = claims
        .preferred_username()
        .map(|u| u.as_str().to_string())
        .or(verified_email)
        .unwrap_or_else(|| subject.to_string());
    let cleaned: String = candidate
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
        .take(64)
        .collect();
    if cleaned.is_empty() {
        // `sub` can be any string at all, including one that filters to nothing. A stable,
        // in-charset fallback keeps provisioning deterministic instead of failing validation.
        format!("oidc-{}", blake3::hash(subject.as_bytes()).to_hex())
            .chars()
            .take(64)
            .collect()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_accepts_only_rooted_local_paths() {
        assert_eq!(safe_return_to(Some("/browse")).as_deref(), Some("/browse"));
        assert_eq!(
            safe_return_to(Some("/browse?q=a&b=c#frag")).as_deref(),
            Some("/browse?q=a&b=c#frag")
        );
        for hostile in [
            // Protocol-relative: an absolute URL that reads as a path.
            "//evil.example/",
            "https://evil.example/",
            "http://evil.example",
            // Some clients fold backslashes to forward slashes, making this protocol-relative too.
            "/\\evil.example",
            "\\\\evil.example",
            // Not rooted: would resolve relative to the callback path.
            "browse",
            "javascript:alert(1)",
            "/browse\nSet-Cookie: x=y",
        ] {
            assert_eq!(
                safe_return_to(Some(hostile)),
                None,
                "{hostile:?} must be refused as a post-login destination"
            );
        }
        assert_eq!(safe_return_to(None), None);
    }
}
