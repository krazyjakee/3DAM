//! OIDC login coverage (phase 6, issue #41), end to end against a **real issuer**.
//!
//! `tests/support/oidc_issuer.rs` runs an actual OpenID Connect provider on a loopback port:
//! discovery document, JWKS, and a token endpoint that validates PKCE and returns an RS256-signed
//! ID token. The server under test discovers it, redirects to it, and exchanges a code with it over
//! HTTP exactly as it would with Keycloak — the same choice `crates/3dam-sources/tests/sftp.rs`
//! makes about SSH. Nothing here is stubbed, because everything worth testing in this flow is a
//! property of bytes on the wire: a signature over a JWKS key, an `iss`/`aud`/`exp`, a `nonce`
//! binding, a PKCE proof. A mock would assert that we called the functions we wrote.
//!
//! The server half is still driven in-process (`ServiceExt::oneshot`, no socket bind), as the other
//! server test binaries do; the test plays the browser, carrying the 302 to the issuer and the
//! callback back again by hand.
//!
//! ## What each test is for
//!
//! - the happy path: a verified subject becomes a session that actually authenticates a later
//!   request (asserted by *using* the cookie, not by reading it).
//! - the default `linked` policy: a valid login for an unknown subject is refused, and creates
//!   nothing. This is the security-relevant default — "the provider says the token is good" is not
//!   authority to hold an account here.
//! - a pre-linked subject resolves to *that* account rather than provisioning a second one.
//! - replay: a captured callback URL is single-use.
//! - a token signed by a key that is not in the issuer's JWKS is refused. This is the test that
//!   proves signature verification happens at all; every other test would pass without it.
//! - a token carrying the wrong `nonce` is refused — the binding that stops a token minted for one
//!   login being injected into another.
//!
//! The last three deliberately run under `auto_viewer`, so that a server which skipped the check
//! would *provision an account* — the assertion "no account exists" then has teeth.

mod support;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use dam_api::admin::{OidcConfig, OidcProvisioning, SetOidcConfig};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use support::oidc_issuer::{SubjectClaims, TestIssuer, FOREIGN_KEY_PEM};
use tower::ServiceExt;

/// The redirect URI registered with the provider. Never actually fetched by a browser here — the
/// test carries the callback to the in-process router itself — but it is echoed in the token
/// exchange, so both sides must agree on it.
const REDIRECT_URL: &str = "http://127.0.0.1:7878/api/v1/auth/oidc/callback";
const CLIENT_ID: &str = "3dam-test-client";
const CLIENT_SECRET: &str = "test-client-secret";

fn unique_tmp() -> std::path::PathBuf {
    // Per-process atomic counter as well as a timestamp, for the reason `dam-core/tests/scan.rs`
    // documents: these tests run in parallel within one process and `as_nanos()` can coincide for
    // two that start in the same clock tick, silently sharing a data dir.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-oidc-{}-{}-{}", std::process::id(), nanos, n))
}

/// A signed-in browser: the session cookie plus the CSRF token from the paired cookie.
#[derive(Clone, Debug)]
struct Session {
    cookie: String,
    csrf: String,
}

impl Session {
    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("cookie".into(), format!("dam_session={}", self.cookie)),
            ("x-dam-csrf".into(), self.csrf.clone()),
        ]
    }
}

/// Fire one request; returns `(status, response headers, json-or-null)`.
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(String, String)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(k, v);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let hdrs = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let val = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, hdrs, val)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    session: Option<&Session>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let headers = session.map(|s| s.headers()).unwrap_or_default();
    let (st, _h, v) = send(app, method, uri, &headers, body).await;
    (st, v)
}

/// Read one cookie out of a reply's `Set-Cookie` headers.
fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .find_map(|h| {
            let s = h.to_str().ok()?;
            s.strip_prefix(&prefix)
                .and_then(|rest| rest.split(';').next())
                .map(str::to_string)
        })
}

/// The `Location` of a redirect reply.
fn location(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::LOCATION)
        .expect("a redirect must carry Location")
        .to_str()
        .unwrap()
        .to_string()
}

// ── harness ──────────────────────────────────────────────────────────────────

/// A server with accounts + OIDC on, pointed at `issuer`, and an admin session for the local owner.
///
/// Flag order matters: both flips are exposure-increasing and so need `confirm`, and `oidc` is
/// flipped *first*, while auth is still `Off` and the admin plane takes no credential. Turning
/// accounts on is what raises the effective gate to `Token`.
async fn harness(
    issuer: &TestIssuer,
    provisioning: OidcProvisioning,
) -> (axum::Router, Arc<ServerStore>, Session) {
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&unique_tmp(), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let peer: SocketAddr = "127.0.0.1:54321".parse().unwrap();
    let app = router(lib.clone(), store.clone(), "127.0.0.1:7878", true)
        .layer(axum::Extension(ConnectInfo(peer)));

    for key in ["oidc", "user_accounts"] {
        let (st, body) = call(
            &app,
            "PUT",
            &format!("/admin/api/flags/{key}"),
            None,
            Some(json!({"value": true, "expected_version": null, "confirm": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "flipping {key} failed: {body}");
    }

    store
        .set_oidc_config(
            &SetOidcConfig {
                config: OidcConfig {
                    issuer: issuer.issuer_url(),
                    client_id: CLIENT_ID.into(),
                    redirect_url: REDIRECT_URL.into(),
                    scopes: vec!["email".into(), "profile".into()],
                    provisioning,
                },
                client_secret: Some(CLIENT_SECRET.into()),
            },
            "test",
        )
        .expect("configure the provider");

    // The first-run claim: gives the instance an admin, so the accounts surface can be inspected.
    let (st, headers, body) = send(
        &app,
        "POST",
        "/api/v1/auth/claim",
        &[],
        Some(json!({"username": "owner", "password": "password123"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "claim failed: {body}");
    let admin = Session {
        cookie: cookie(&headers, "dam_session").expect("claim sets the session cookie"),
        csrf: body["csrf"].as_str().expect("csrf in reply").to_string(),
    };
    (app, store, admin)
}

/// Usernames of every account on the instance.
async fn accounts(app: &axum::Router, admin: &Session) -> Vec<String> {
    let (st, body) = call(app, "GET", "/admin/api/accounts", Some(admin), None).await;
    assert_eq!(st, StatusCode::OK, "listing accounts failed: {body}");
    body.as_array()
        .expect("accounts list")
        .iter()
        .map(|a| a["username"].as_str().unwrap().to_string())
        .collect()
}

/// Start a login and play the browser as far as the issuer: returns the callback path the provider
/// would send the browser to, given a code minted for `claims`.
///
/// Deliberately *not* one call that also drives the callback — several tests need to tamper with
/// exactly one element of the callback, and a single opaque `login()` helper would hide the thing
/// under test.
async fn start_and_grant(
    app: &axum::Router,
    issuer: &TestIssuer,
    return_to: Option<&str>,
    claims: SubjectClaims,
) -> Callback {
    let uri = match return_to {
        Some(p) => format!("/api/v1/auth/oidc/start?return_to={p}"),
        None => "/api/v1/auth/oidc/start".to_string(),
    };
    let (st, headers, body) = send(app, "GET", &uri, &[], None).await;
    assert_eq!(st, StatusCode::FOUND, "start did not redirect: {body}");

    // The browser also comes away holding the in-flight-login cookie, which binds this `state` to
    // it. Carrying it is what makes these tests model a browser rather than a bare HTTP client —
    // and the callback is refused without it (see `a_callback_from_another_browser_is_refused`).
    let browser = cookie(&headers, "dam_oidc").expect("start must set the login cookie");

    let auth = issuer.visit_authorize(&location(&headers)).await;
    let code = issuer.grant_claims(&auth, claims);
    Callback {
        uri: format!(
            "/api/v1/auth/oidc/callback?code={code}&state={}",
            urlencode(&auth.state)
        ),
        browser,
    }
}

/// A callback URL together with the browser cookie that is allowed to redeem it.
struct Callback {
    uri: String,
    browser: String,
}

impl Callback {
    /// The `Cookie` header a browser would send back with the callback.
    fn headers(&self) -> Vec<(String, String)> {
        vec![("cookie".into(), format!("dam_oidc={}", self.browser))]
    }
}

/// Minimal percent-encoding for the query values this test builds. `state`/`code` are
/// base64url/uuid text, so only `+`, `=` and `/` are ever at stake.
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '+' => "%2B".to_string(),
            '/' => "%2F".to_string(),
            '=' => "%3D".to_string(),
            '&' => "%26".to_string(),
            other => other.to_string(),
        })
        .collect()
}

// ── the happy path ───────────────────────────────────────────────────────────

/// A verified subject under `auto_viewer` gets an account **and** a session that works.
///
/// The cookie is proved by using it: an unauthenticated read of `/api/v1/whoami` 401s on this
/// instance, so the same read returning 200 with the provisioned username is the whole claim.
/// Asserting on the `Set-Cookie` string would only prove the server can format a header.
#[tokio::test]
async fn a_verified_subject_gets_an_account_and_a_working_session() {
    let issuer = TestIssuer::start().await;
    let (app, _store, admin) = harness(&issuer, OidcProvisioning::AutoViewer).await;
    assert_eq!(accounts(&app, &admin).await, vec!["owner".to_string()]);

    // The redirect the client follows is a real authorization request: PKCE S256, our client id,
    // and the scopes the operator configured on top of `openid`.
    let (st, headers, _) = send(
        &app,
        "GET",
        "/api/v1/auth/oidc/start?return_to=/browse",
        &[],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FOUND);
    // The browser also receives the cookie binding this login to itself.
    let browser = cookie(&headers, "dam_oidc").expect("start must set the login cookie");
    let auth = issuer.visit_authorize(&location(&headers)).await;
    assert_eq!(auth.client_id, CLIENT_ID);
    assert_eq!(auth.redirect_uri, REDIRECT_URL);
    assert_eq!(auth.response_type, "code");
    assert_eq!(auth.code_challenge_method, "S256");
    assert!(
        !auth.code_challenge.is_empty(),
        "the authorization request must carry a PKCE challenge"
    );
    for scope in ["openid", "email", "profile"] {
        assert!(
            auth.scopes.iter().any(|s| s == scope),
            "scope {scope} missing from {:?}",
            auth.scopes
        );
    }

    let code = issuer.grant_claims(
        &auth,
        SubjectClaims::new("subject-alice")
            .preferred_username("alice")
            .email("alice@example.com")
            .name("Alice Example"),
    );
    let (st, headers, body) = send(
        &app,
        "GET",
        &format!(
            "/api/v1/auth/oidc/callback?code={code}&state={}",
            urlencode(&auth.state)
        ),
        &[("cookie".to_string(), format!("dam_oidc={browser}"))],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FOUND, "callback failed: {body}");
    // Lands back on the app, at the path the login started from.
    assert_eq!(location(&headers), "/browse");

    let session = Session {
        cookie: cookie(&headers, "dam_session").expect("callback sets the session cookie"),
        csrf: cookie(&headers, "dam_csrf").expect("callback sets the CSRF cookie"),
    };

    // The gate is real: the same read without the cookie is refused.
    let (st, _) = call(&app, "GET", "/api/v1/whoami", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // …and with it, the session is an ordinary signed-in identity.
    let (st, body) = call(&app, "GET", "/api/v1/whoami", Some(&session), None).await;
    assert_eq!(st, StatusCode::OK, "the OIDC session did not authenticate");
    assert_eq!(body["account"]["username"], "alice");
    assert_eq!(body["account"]["role"], "viewer");

    // A route that does actual work, not just identity reflection.
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/query",
        Some(&session),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "query refused the OIDC session: {body}");

    let mut names = accounts(&app, &admin).await;
    names.sort();
    assert_eq!(names, vec!["alice".to_string(), "owner".to_string()]);
}

// ── the default policy ───────────────────────────────────────────────────────

/// `linked` (the v1 default) refuses a subject nobody has vouched for, and leaves no trace.
///
/// Everything about this login is valid — the signature verifies, the nonce matches, PKCE checks
/// out. It is refused on policy alone, which is the point: for a public issuer, "holds a valid
/// account with the provider" is most of the internet.
#[tokio::test]
async fn the_default_policy_refuses_an_unlinked_subject() {
    let issuer = TestIssuer::start().await;
    let (app, _store, admin) = harness(&issuer, OidcProvisioning::Linked).await;

    let callback = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-stranger").preferred_username("stranger"),
    )
    .await;
    let (st, headers, body) = send(&app, "GET", &callback.uri, &callback.headers(), None).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "unlinked subject was let in: {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not linked"),
        "the refusal should say why: {body}"
    );
    assert!(
        cookie(&headers, "dam_session").is_none(),
        "a refused login must not mint a session"
    );
    assert_eq!(
        accounts(&app, &admin).await,
        vec!["owner".to_string()],
        "a refused login must not create an account"
    );
}

// ── a linked identity ────────────────────────────────────────────────────────

/// A pre-linked `(issuer, subject)` resolves to that existing account — no second account, and the
/// provider's `preferred_username` does not get to rename or shadow it.
#[tokio::test]
async fn a_linked_subject_signs_in_as_that_account() {
    let issuer = TestIssuer::start().await;
    let (app, store, admin) = harness(&issuer, OidcProvisioning::Linked).await;

    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/accounts",
        Some(&admin),
        Some(json!({"username": "vera", "password": "password123", "role": "editor"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "creating the account failed: {body}");
    let vera = body["account_id"].as_str().unwrap().to_string();
    store
        .link_oidc_identity(&issuer.issuer_url(), "subject-vera", &vera, "test")
        .expect("link the identity");

    // The token claims a different `preferred_username` on purpose: the link, not the claim, is
    // what decides which account this is.
    let callback = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-vera").preferred_username("someone-else"),
    )
    .await;
    let (st, headers, body) = send(&app, "GET", &callback.uri, &callback.headers(), None).await;
    assert_eq!(st, StatusCode::FOUND, "linked login failed: {body}");

    let session = Session {
        cookie: cookie(&headers, "dam_session").expect("session cookie"),
        csrf: cookie(&headers, "dam_csrf").expect("csrf cookie"),
    };
    let (st, body) = call(&app, "GET", "/api/v1/whoami", Some(&session), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["account"]["username"], "vera");
    assert_eq!(body["account"]["account_id"], vera);

    let mut names = accounts(&app, &admin).await;
    names.sort();
    assert_eq!(
        names,
        vec!["owner".to_string(), "vera".to_string()],
        "a linked login must not provision a second account"
    );
}

// ── replay ───────────────────────────────────────────────────────────────────

/// The same callback URL cannot be walked through the flow twice.
///
/// `state` is redeemed with `DELETE … RETURNING`, so the second attempt fails at the *first* step —
/// before the code is even presented to the issuer. The message assertion is what distinguishes
/// that from merely being saved by the issuer refusing a spent code.
#[tokio::test]
async fn a_captured_callback_cannot_be_replayed() {
    let issuer = TestIssuer::start().await;
    let (app, _store, admin) = harness(&issuer, OidcProvisioning::AutoViewer).await;

    let callback = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-replay").preferred_username("replay"),
    )
    .await;
    let (st, _, body) = send(&app, "GET", &callback.uri, &callback.headers(), None).await;
    assert_eq!(st, StatusCode::FOUND, "first use should succeed: {body}");

    let (st, headers, body) = send(&app, "GET", &callback.uri, &callback.headers(), None).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a replayed callback was accepted"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already-used"),
        "replay must be refused at the state check, not incidentally: {body}"
    );
    assert!(
        cookie(&headers, "dam_session").is_none(),
        "a replayed callback must not mint a second session"
    );
    // One account from the first (legitimate) login, and no more.
    let mut names = accounts(&app, &admin).await;
    names.sort();
    assert_eq!(names, vec!["owner".to_string(), "replay".to_string()]);
}

// ── the signature ────────────────────────────────────────────────────────────

/// An ID token signed by a key that is not in the issuer's JWKS is refused.
///
/// The issuer publishes its ordinary key and signs with a foreign one, so the token is correct in
/// every other respect — right `iss`, right `aud`, unexpired, right `nonce`, redeemed with a valid
/// PKCE verifier. Only the signature is wrong. Under `auto_viewer` a server that did not verify it
/// would provision an account, which is exactly what the final assertion looks for.
#[tokio::test]
async fn an_id_token_signed_by_a_foreign_key_is_rejected() {
    let issuer = TestIssuer::start_signing_with(FOREIGN_KEY_PEM).await;
    let (app, _store, admin) = harness(&issuer, OidcProvisioning::AutoViewer).await;

    let callback = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-forged").preferred_username("forged"),
    )
    .await;
    let (st, headers, body) = send(&app, "GET", &callback.uri, &callback.headers(), None).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a token signed by an unpublished key was accepted: {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("ID token failed validation"),
        "expected a validation failure, got: {body}"
    );
    assert!(cookie(&headers, "dam_session").is_none());
    assert_eq!(
        accounts(&app, &admin).await,
        vec!["owner".to_string()],
        "an unverifiable token must not provision anything"
    );
}

// ── the nonce ────────────────────────────────────────────────────────────────

/// An ID token whose `nonce` is not the one this login started with is refused.
///
/// This is the binding that stops a token minted for one authorization request being replayed into
/// another. The token is properly signed by the published key, so nothing else can be doing the
/// rejecting.
#[tokio::test]
async fn an_id_token_with_the_wrong_nonce_is_rejected() {
    let issuer = TestIssuer::start().await;
    let (app, _store, admin) = harness(&issuer, OidcProvisioning::AutoViewer).await;

    let callback = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-nonce")
            .preferred_username("nonce")
            .nonce("a-nonce-from-some-other-login"),
    )
    .await;
    let (st, headers, body) = send(&app, "GET", &callback.uri, &callback.headers(), None).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a token with a foreign nonce was accepted: {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("ID token failed validation"),
        "expected a validation failure, got: {body}"
    );
    assert!(cookie(&headers, "dam_session").is_none());
    assert_eq!(
        accounts(&app, &admin).await,
        vec!["owner".to_string()],
        "a nonce mismatch must not provision anything"
    );
}

/// Login CSRF: single-use `state` is not enough on its own.
///
/// The attack this guards is the one that survives every other check in the flow. The attacker
/// starts a login *at this server*, authenticates at the provider as themselves, and gets a
/// callback URL that is valid in every respect — right `state`, right `code`, right `nonce`,
/// correctly signed token, never used before. They then get the victim to open it. Without a
/// browser binding the victim is silently signed in **as the attacker**, and everything they do
/// next happens inside the attacker's library.
///
/// So the callback is refused unless the browser presents the cookie `/start` gave it.
#[tokio::test]
async fn a_callback_from_another_browser_is_refused() {
    let issuer = TestIssuer::start().await;
    let (app, _store, admin) = harness(&issuer, OidcProvisioning::AutoViewer).await;

    // The attacker's browser runs a complete, honest login and stops holding the callback URL.
    let attacker = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-attacker").preferred_username("attacker"),
    )
    .await;

    // The victim's browser opens it. It has no `dam_oidc` cookie for this login.
    let (st, headers, body) = send(&app, "GET", &attacker.uri, &[], None).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a callback redeemed by a different browser must be refused: {body}"
    );
    assert!(
        cookie(&headers, "dam_session").is_none(),
        "the victim must not be handed a session for the attacker's identity"
    );
    assert_eq!(
        accounts(&app, &admin).await,
        vec!["owner".to_string()],
        "no account should have been provisioned"
    );

    // A *wrong* cookie is refused too — and this needs its own fresh login, because reusing the
    // one above would be spent and would 403 at the state lookup instead, proving nothing about
    // the cookie check. The message is asserted for the same reason: both refusals are 403.
    let second = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-attacker-2").preferred_username("attacker2"),
    )
    .await;
    let (st, _h, body) = send(
        &app,
        "GET",
        &second.uri,
        &[("cookie".to_string(), "dam_oidc=not-the-right-value".into())],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a forged cookie must not pass");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not started by this browser"),
        "a forged cookie must fail the browser binding, not the state lookup: {body}"
    );

    // And the rightful browser can still finish: a wrong-cookie attempt must not consume the
    // pending login, or anyone holding a leaked `state` could lock the real user out.
    let (st, _h, body) = send(&app, "GET", &second.uri, &second.headers(), None).await;
    assert_eq!(
        st,
        StatusCode::FOUND,
        "a failed CSRF attempt must not spend the victim's login: {body}"
    );
}

/// Acceptance for issue #41: flag off ⇒ the surface is *absent*, not merely forbidden.
///
/// Checked at the router layer, so it holds before any provider config is consulted — and both
/// flags are required, because an OIDC login with accounts off has no account to resolve to.
#[tokio::test]
async fn the_login_surface_is_absent_until_both_flags_are_on() {
    let issuer = TestIssuer::start().await;
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&unique_tmp(), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let app = router(lib.clone(), store.clone(), "127.0.0.1:7878", true);

    // Even fully configured, the routes do not exist while the flags are off — so an operator who
    // sets up a provider and forgets the switch gets a 404, not a half-working login.
    store
        .set_oidc_config(
            &SetOidcConfig {
                config: OidcConfig {
                    issuer: issuer.issuer_url(),
                    client_id: CLIENT_ID.into(),
                    redirect_url: REDIRECT_URL.into(),
                    scopes: vec![],
                    provisioning: OidcProvisioning::AutoViewer,
                },
                client_secret: Some(CLIENT_SECRET.into()),
            },
            "test",
        )
        .unwrap();

    for uri in [
        "/api/v1/auth/oidc/start",
        "/api/v1/auth/oidc/callback?code=x&state=y",
    ] {
        let (st, _h, _b) = send(&app, "GET", uri, &[], None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri} should be absent");
    }

    // `oidc` alone is not enough: with accounts off there is nothing for a subject to become.
    let (st, body) = call(
        &app,
        "PUT",
        "/admin/api/flags/oidc",
        None,
        Some(json!({"value": true, "expected_version": null, "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, _h, _b) = send(&app, "GET", "/api/v1/auth/oidc/start", &[], None).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "OIDC without user accounts must stay absent"
    );

    // Both on: the route exists and gets as far as talking to the provider.
    let (st, body) = call(
        &app,
        "PUT",
        "/admin/api/flags/user_accounts",
        None,
        Some(json!({"value": true, "expected_version": null, "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, _h, _b) = send(&app, "GET", "/api/v1/auth/oidc/start", &[], None).await;
    assert_eq!(
        st,
        StatusCode::FOUND,
        "with both flags on the login must actually start"
    );
}

/// The default policy is `Linked`, so the admin link route is what makes a default-configured
/// provider usable at all. Without it every login is refused with "an administrator must link it
/// first" and there is no way for an administrator to do so.
#[tokio::test]
async fn an_admin_can_link_a_subject_and_that_subject_can_then_sign_in() {
    let issuer = TestIssuer::start().await;
    let (app, store, admin) = harness(&issuer, OidcProvisioning::Linked).await;

    // A local account exists; its owner's provider subject is not yet known to us.
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/accounts",
        Some(&admin),
        Some(
            json!({"username": "dana", "password": "correct-horse-battery",
                    "role": "editor"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "creating the account failed: {body}");
    let dana_id = body["account_id"].as_str().unwrap().to_string();

    // Before linking, a perfectly valid login is refused.
    let before = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-dana").preferred_username("dana-at-idp"),
    )
    .await;
    let (st, _h, _b) = send(&app, "GET", &before.uri, &before.headers(), None).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "unlinked subject must be refused"
    );

    // The admin links it through the API.
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/oidc/identities",
        Some(&admin),
        Some(json!({"subject": "subject-dana", "account_id": dana_id})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "linking failed: {body}");
    let links = body.as_array().expect("the link list comes back");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["username"], "dana");
    assert_eq!(links[0]["subject"], "subject-dana");

    // Now the same subject signs in — as `dana`, not as a new account named after the claim.
    let after = start_and_grant(
        &app,
        &issuer,
        None,
        SubjectClaims::new("subject-dana").preferred_username("dana-at-idp"),
    )
    .await;
    let (st, headers, body) = send(&app, "GET", &after.uri, &after.headers(), None).await;
    assert_eq!(
        st,
        StatusCode::FOUND,
        "the linked subject must sign in: {body}"
    );
    let session = Session {
        cookie: cookie(&headers, "dam_session").expect("a session must be minted"),
        csrf: cookie(&headers, "dam_csrf").expect("callback sets the CSRF cookie"),
    };
    let (st, who) = call(&app, "GET", "/api/v1/whoami", Some(&session), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(who["identity"], "dana");
    assert_eq!(
        accounts(&app, &admin).await,
        vec!["dana".to_string(), "owner".to_string()],
        "linking must not have created a second account"
    );

    // Unlinking revokes the provider's ability to sign in as that account; the account remains.
    let (st, body) = call(
        &app,
        "DELETE",
        "/admin/api/oidc/identities/subject-dana",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "unlink failed: {body}");
    let _ = store;
}
