//! Test-only support: a **real OpenID Connect provider**, served over loopback HTTP.
//!
//! The OIDC login flow (`crate::oidc`) is almost entirely made of things that only exist on the
//! wire — a discovery document, a JWKS, an RS256 signature over claims, a PKCE proof. A mocked
//! `LibraryService`-style seam cannot exercise any of that: the security of this flow lives in
//! `openidconnect`'s validation of bytes fetched over HTTP, and a test that never produces those
//! bytes proves nothing. So this follows the same house pattern as the SFTP harness
//! (`crates/3dam-sources/tests/support/mod.rs`): stand up the *other side of the protocol* on
//! `127.0.0.1:0` and let the production code talk to it exactly as it would talk to Keycloak.
//!
//! `ServerStore::set_oidc_config` permits an `http://` issuer for loopback hosts specifically so a
//! local dev provider needs no certificate — which is what makes this harness possible without
//! minting a CA and teaching reqwest to trust it.
//!
//! ## What it serves
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /.well-known/openid-configuration` | discovery; `issuer` is byte-identical to [`TestIssuer::issuer_url`] |
//! | `GET /jwks` | the public half of the signing key |
//! | `GET /authorize` | records the authorization request (no consent UI — the test plays the human) |
//! | `POST /token` | validates `code` + `client_id` + PKCE `code_verifier`, returns a signed ID token |
//!
//! ## The key is a constant, not generated
//!
//! RSA key generation is slow in a debug build (seconds, occasionally much worse — the primality
//! search has a long tail), and this harness is started by every test in `tests/oidc.rs`. Two fixed
//! 2048-bit PKCS#1 keys are embedded instead: [`SIGNING_KEY_PEM`], which the issuer normally both
//! signs with and publishes, and [`FOREIGN_KEY_PEM`], which exists so a test can sign a token with
//! a key that is *not* in the published JWKS and prove the signature is genuinely verified. They
//! are test fixtures with no other life — never used to protect anything.
//!
//! ## Why the browser is simulated rather than driven
//!
//! There is no consent page, because a headless browser is not a dependency this repo is going to
//! take on to test a redirect. `/authorize` records what the relying party asked for; the test then
//! calls [`TestIssuer::grant`] to say "the human authenticated as *this* subject" and drives the
//! callback itself. Everything the relying party actually validates — signature, `iss`, `aud`,
//! `exp`, `nonce`, PKCE — still crosses a real socket.

#![allow(dead_code)] // a shared support module is used piecemeal by each test binary

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use openidconnect::core::{
    CoreClientAuthMethod, CoreIdToken, CoreIdTokenClaims, CoreIdTokenFields, CoreJsonWebKeySet,
    CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType, CoreRsaPrivateSigningKey,
    CoreSubjectIdentifierType, CoreTokenResponse, CoreTokenType,
};
use openidconnect::{
    AccessToken, Audience, AuthUrl, EmptyAdditionalClaims, EmptyAdditionalProviderMetadata,
    EmptyExtraTokenFields, EndUserEmail, EndUserName, EndUserUsername, IssuerUrl, JsonWebKeyId,
    JsonWebKeySetUrl, LocalizedClaim, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    PrivateSigningKey, ResponseTypes, Scope, StandardClaims, SubjectIdentifier, TokenUrl,
};

/// The issuer's ordinary signing key: signs ID tokens *and* is published at `/jwks`.
pub const SIGNING_KEY_PEM: &str = include_str!("oidc_signing_key.pem");

/// A second key that is **never** published. [`TestIssuer::start_signing_with`] makes the issuer
/// sign with this while still publishing [`SIGNING_KEY_PEM`], which is how a test forges a token
/// that is well-formed in every respect except the one that matters.
pub const FOREIGN_KEY_PEM: &str = include_str!("oidc_foreign_key.pem");

/// The `kid` on both keys. Deliberately the same string: with matching key ids the relying party
/// *selects* the forged token's key and then rejects it on the signature, rather than bailing out
/// early with "no key matches this kid". The stricter of the two failures to test.
const KEY_ID: &str = "3dam-test-key";

/// How long a minted ID token is valid. Long enough that a slow debug-build test cannot expire it.
const TOKEN_TTL_SECS: i64 = 300;

// ── the authorization request, as the issuer received it ─────────────────────

/// What the relying party sent to `/authorize`, read back off a real HTTP request.
///
/// The test needs `state` and `nonce` from here to build a callback the server will accept, and
/// asserting on `code_challenge`/`scopes` is how "the redirect was actually well-formed" gets
/// checked rather than assumed.
#[derive(Clone, Debug)]
pub struct AuthRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub response_type: String,
    pub state: String,
    pub nonce: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub scopes: Vec<String>,
}

/// Who the (simulated) human authenticated as, and what the issuer will assert about them.
#[derive(Clone, Debug)]
pub struct SubjectClaims {
    pub sub: String,
    pub preferred_username: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    /// The `nonce` the ID token will carry. `None` means "the one from the authorization request",
    /// which is the correct behaviour; a test sets it to prove the binding is enforced.
    pub nonce: Option<String>,
}

impl SubjectClaims {
    pub fn new(sub: &str) -> Self {
        Self {
            sub: sub.to_string(),
            preferred_username: None,
            email: None,
            name: None,
            nonce: None,
        }
    }
    pub fn preferred_username(mut self, v: &str) -> Self {
        self.preferred_username = Some(v.to_string());
        self
    }
    pub fn email(mut self, v: &str) -> Self {
        self.email = Some(v.to_string());
        self
    }
    pub fn name(mut self, v: &str) -> Self {
        self.name = Some(v.to_string());
        self
    }
    /// Mint the token with a nonce of the test's choosing instead of the request's.
    pub fn nonce(mut self, v: &str) -> Self {
        self.nonce = Some(v.to_string());
        self
    }
}

/// An issued authorization code and everything the token endpoint needs to redeem it.
#[derive(Clone, Debug)]
struct Grant {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    nonce: String,
    claims: SubjectClaims,
}

// ── the server ───────────────────────────────────────────────────────────────

struct Inner {
    /// e.g. `http://127.0.0.1:41234` — no trailing slash, and the exact string the discovery
    /// document reports as `issuer`. `openidconnect` refuses metadata whose `issuer` differs from
    /// the URL it was fetched from, so these must not drift.
    base: String,
    signing_key: CoreRsaPrivateSigningKey,
    jwks: CoreJsonWebKeySet,
    /// Authorization requests seen at `/authorize`, keyed by `state` (unique per login).
    seen: Mutex<HashMap<String, AuthRequest>>,
    /// Codes minted by [`TestIssuer::grant`], removed as they are redeemed (single-use, like a real
    /// provider — the token endpoint is not the place to be laxer than production).
    codes: Mutex<HashMap<String, Grant>>,
}

/// A running OpenID Connect provider on a loopback port. Dropping it stops the server.
pub struct TestIssuer {
    inner: Arc<Inner>,
    http: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestIssuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestIssuer {
    /// Start an issuer that signs with the key it publishes — i.e. an honest one.
    pub async fn start() -> TestIssuer {
        Self::start_signing_with(SIGNING_KEY_PEM).await
    }

    /// Start an issuer that **publishes** [`SIGNING_KEY_PEM`] but signs ID tokens with `pem`.
    ///
    /// Pass [`FOREIGN_KEY_PEM`] to get an issuer whose tokens are structurally perfect and
    /// cryptographically worthless. Everything else about the flow still works, so a test using
    /// this fails at exactly one place: signature verification.
    pub async fn start_signing_with(pem: &str) -> TestIssuer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test issuer");
        let port = listener.local_addr().expect("local_addr").port();
        let base = format!("http://127.0.0.1:{port}");

        let kid = || Some(JsonWebKeyId::new(KEY_ID.to_string()));
        let signing_key =
            CoreRsaPrivateSigningKey::from_pem(pem, kid()).expect("parse signing key PEM");
        // The published key is always the ordinary one, whatever we sign with.
        let published = CoreRsaPrivateSigningKey::from_pem(SIGNING_KEY_PEM, kid())
            .expect("parse published key PEM");
        let jwks = CoreJsonWebKeySet::new(vec![published.as_verification_key()]);

        let inner = Arc::new(Inner {
            base: base.clone(),
            signing_key,
            jwks,
            seen: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
        });

        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks_handler))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .with_state(inner.clone());

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // The relying party refuses redirects on its own OIDC calls (an SSRF guard); matching that
        // here keeps the harness's own requests honest about what the issuer actually returned.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build issuer test client");

        TestIssuer { inner, http, task }
    }

    /// The issuer URL, exactly as it must be configured on the server side.
    pub fn issuer_url(&self) -> String {
        self.inner.base.clone()
    }

    /// Play the browser: follow the relying party's 302 to `/authorize` over real HTTP, and read
    /// back the authorization request the issuer received.
    ///
    /// The `state`/`nonce`/`code_challenge` in the result therefore come from the wire, not from
    /// the test's imagination — which is the point, since `nonce` is what the server will insist
    /// the ID token echoes.
    pub async fn visit_authorize(&self, location: &str) -> AuthRequest {
        let resp = self
            .http
            .get(location)
            .send()
            .await
            .expect("GET the authorization endpoint");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "authorization endpoint refused the request: {}",
            resp.text().await.unwrap_or_default()
        );
        let state = resp.text().await.expect("read authorize body");
        self.inner
            .seen
            .lock()
            .unwrap()
            .get(&state)
            .cloned()
            .expect("the issuer recorded the authorization request")
    }

    /// "The human authenticated as `sub`." Mints an authorization code bound to `auth`.
    pub fn grant(&self, auth: &AuthRequest, sub: &str) -> String {
        self.grant_claims(auth, SubjectClaims::new(sub))
    }

    /// As [`TestIssuer::grant`], with control over the claims (and the nonce) the ID token carries.
    pub fn grant_claims(&self, auth: &AuthRequest, claims: SubjectClaims) -> String {
        let code = format!("code-{}", uuid::Uuid::now_v7().simple());
        let grant = Grant {
            client_id: auth.client_id.clone(),
            redirect_uri: auth.redirect_uri.clone(),
            code_challenge: auth.code_challenge.clone(),
            nonce: claims.nonce.clone().unwrap_or_else(|| auth.nonce.clone()),
            claims,
        };
        self.inner.codes.lock().unwrap().insert(code.clone(), grant);
        code
    }
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn discovery(State(st): State<Arc<Inner>>) -> Json<CoreProviderMetadata> {
    let base = &st.base;
    let md = CoreProviderMetadata::new(
        IssuerUrl::new(base.clone()).expect("issuer url"),
        AuthUrl::new(format!("{base}/authorize")).expect("auth url"),
        JsonWebKeySetUrl::new(format!("{base}/jwks")).expect("jwks url"),
        vec![ResponseTypes::new(vec![CoreResponseType::Code])],
        vec![CoreSubjectIdentifierType::Public],
        vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
        EmptyAdditionalProviderMetadata {},
    )
    .set_token_endpoint(Some(
        TokenUrl::new(format!("{base}/token")).expect("token url"),
    ))
    .set_scopes_supported(Some(vec![
        Scope::new("openid".into()),
        Scope::new("email".into()),
        Scope::new("profile".into()),
    ]))
    .set_token_endpoint_auth_methods_supported(Some(vec![
        CoreClientAuthMethod::ClientSecretBasic,
        CoreClientAuthMethod::ClientSecretPost,
    ]));
    Json(md)
}

async fn jwks_handler(State(st): State<Arc<Inner>>) -> Json<CoreJsonWebKeySet> {
    Json(st.jwks.clone())
}

/// `GET /authorize` — record the request and hand back its `state`.
///
/// A real provider would render a login page here. This one answers with the `state` in the body so
/// [`TestIssuer::visit_authorize`] can look the recorded request up without guessing.
async fn authorize(
    State(st): State<Arc<Inner>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    let state = get("state");
    if state.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing state").into_response();
    }
    let req = AuthRequest {
        client_id: get("client_id"),
        redirect_uri: get("redirect_uri"),
        response_type: get("response_type"),
        state: state.clone(),
        nonce: get("nonce"),
        code_challenge: get("code_challenge"),
        code_challenge_method: get("code_challenge_method"),
        scopes: get("scope").split_whitespace().map(String::from).collect(),
    };
    st.seen.lock().unwrap().insert(state.clone(), req);
    (StatusCode::OK, state).into_response()
}

/// An RFC 6749 §5.2 error body. The relying party parses this, so a refusal here surfaces as a
/// meaningful "token exchange failed: …" rather than a deserialization mess.
fn oauth_error(code: &str, description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": code, "error_description": description })),
    )
        .into_response()
}

/// The client id the request authenticated as: HTTP Basic (a confidential client) or the
/// `client_id` form field (a public one). Both are legal; `openidconnect` picks Basic whenever a
/// client secret is configured.
///
/// RFC 6749 §2.3.1 wants the id and secret form-url-encoded before base64, and `oauth2` obliges.
/// Test client ids here are `[A-Za-z0-9._-]`, for which that encoding is the identity, so no
/// percent-decoding step is needed — an assumption worth stating rather than a subtlety worth
/// hiding.
fn authenticated_client(headers: &HeaderMap, form: &HashMap<String, String>) -> Option<String> {
    if let Some(basic) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(basic.trim())
            .ok()?;
        let decoded = String::from_utf8(raw).ok()?;
        let (id, _secret) = decoded.split_once(':')?;
        return Some(id.to_string());
    }
    form.get("client_id").cloned()
}

/// `POST /token` — redeem an authorization code for a signed ID token.
///
/// The three checks here are the ones a relying party is entitled to rely on, so the harness does
/// them for real: the code must be one this issuer minted (and is spent by redeeming it), the
/// client must be the one the code was issued to, and the PKCE verifier must hash to the challenge
/// from the authorization request. Skipping the last would let this harness pass a server that
/// forgot PKCE entirely.
async fn token(
    State(st): State<Arc<Inner>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return oauth_error("unsupported_grant_type", "expected authorization_code");
    }
    let Some(code) = form.get("code") else {
        return oauth_error("invalid_request", "missing code");
    };
    let Some(grant) = st.codes.lock().unwrap().remove(code) else {
        return oauth_error("invalid_grant", "unknown or already-redeemed code");
    };

    match authenticated_client(&headers, &form) {
        Some(id) if id == grant.client_id => {}
        Some(id) => {
            return oauth_error(
                "invalid_client",
                &format!("code was issued to {}, not {id}", grant.client_id),
            )
        }
        None => return oauth_error("invalid_client", "no client authentication"),
    }

    if let Some(redirect) = form.get("redirect_uri") {
        if *redirect != grant.redirect_uri {
            return oauth_error("invalid_grant", "redirect_uri does not match the request");
        }
    }

    let Some(verifier) = form.get("code_verifier") else {
        return oauth_error("invalid_request", "missing code_verifier (PKCE)");
    };
    // RFC 7636 §4.1 bounds the verifier at 43–128 characters, and `from_code_verifier_sha256`
    // *asserts* it. Checking here keeps a malformed verifier an ordinary `invalid_grant` rather
    // than a panic inside the handler, which would surface to the relying party as an opaque
    // "Request failed" and disguise a real PKCE failure as a transport one.
    if !(43..=128).contains(&verifier.len()) {
        return oauth_error("invalid_grant", "code_verifier is not 43-128 characters");
    }
    let computed =
        PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(verifier.clone()));
    if computed.as_str() != grant.code_challenge {
        return oauth_error(
            "invalid_grant",
            "PKCE verifier does not match the challenge",
        );
    }

    let access_token = AccessToken::new(format!("at-{}", uuid::Uuid::now_v7().simple()));
    let now = chrono::Utc::now();
    let mut standard = StandardClaims::new(SubjectIdentifier::new(grant.claims.sub.clone()))
        .set_preferred_username(grant.claims.preferred_username.map(EndUserUsername::new))
        .set_email(grant.claims.email.map(EndUserEmail::new))
        .set_email_verified(Some(true));
    if let Some(name) = grant.claims.name {
        standard = standard.set_name(Some(LocalizedClaim::from(EndUserName::new(name))));
    }
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(st.base.clone()).expect("issuer url"),
        vec![Audience::new(grant.client_id.clone())],
        now + chrono::Duration::seconds(TOKEN_TTL_SECS),
        now,
        standard,
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(grant.nonce)));

    let id_token = CoreIdToken::new(
        claims,
        &st.signing_key,
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        Some(&access_token),
        None,
    )
    .expect("sign id token");

    let mut resp = CoreTokenResponse::new(
        access_token,
        CoreTokenType::Bearer,
        CoreIdTokenFields::new(Some(id_token), EmptyExtraTokenFields {}),
    );
    resp.set_expires_in(Some(&std::time::Duration::from_secs(TOKEN_TTL_SECS as u64)));
    Json(resp).into_response()
}
