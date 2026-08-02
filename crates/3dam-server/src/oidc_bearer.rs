//! Stateless OIDC bearer-JWT authentication for API, MCP, and ticket callers (issue #100).
//!
//! The browser code flow and this verifier deliberately share provider discovery and the
//! `openidconnect` ID-token verifier. The cache is process-local and contains exactly one provider
//! (the store itself permits exactly one): configuration changes replace it, a five-minute TTL
//! bounds staleness, and an unknown `kid` gets at most one coalesced refresh per cooldown. No token,
//! subject, or attacker-chosen key id is retained.

use crate::oidc;
use crate::store::oidc::StoredOidc;
use crate::AppState;
use base64::Engine as _;
use dam_api::accounts::AccountIdentity;
use dam_api::service::{AuthContext, AuthSource};
use dam_api::LibError;
use openidconnect::core::{
    CoreIdToken, CoreIdTokenVerifier, CoreJwsSigningAlgorithm, CoreProviderMetadata,
};
use openidconnect::{ClientId, IssuerUrl, Nonce};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const UNKNOWN_KID_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_JWT_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct Cache {
    issuer: String,
    client_id: String,
    metadata: Option<CoreProviderMetadata>,
    fetched_at: Option<Instant>,
    generation: u64,
    last_unknown_refresh: Option<Instant>,
    next_regular_refresh: Option<Instant>,
}

/// A bounded, single-provider discovery/JWKS cache.
pub(crate) struct Verifier {
    cache: Mutex<Cache>,
}

impl Verifier {
    pub(crate) fn new() -> Self {
        Self {
            cache: Mutex::new(Cache::default()),
        }
    }

    async fn metadata(
        &self,
        st: &AppState,
        cfg: &StoredOidc,
    ) -> Result<(CoreProviderMetadata, u64), LibError> {
        let mut cache = self.cache.lock().await;
        reset_for_config(&mut cache, cfg);
        let fresh = cache.fetched_at.is_some_and(|at| at.elapsed() < CACHE_TTL);
        let retry_due = cache
            .next_regular_refresh
            .is_none_or(|at| Instant::now() >= at);
        if !fresh && retry_due {
            // Bound outage retries as well as hostile unknown-kid retries. Stamping before the
            // call means a timeout cannot cause every waiter to launch its own discovery.
            cache.next_regular_refresh = Some(Instant::now() + UNKNOWN_KID_COOLDOWN);
            let permit = st.auth_protection.discovery_permit()?;
            match oidc::discover(cfg).await {
                Ok((metadata, _http)) => {
                    cache.metadata = Some(metadata);
                    cache.fetched_at = Some(Instant::now());
                    cache.generation = cache.generation.wrapping_add(1);
                    cache.next_regular_refresh = None;
                }
                Err(error) if cache.metadata.is_none() => return Err(error),
                // A transient issuer outage must not turn a cached, still-cryptographically-valid
                // key into an availability outage. The next request after the cooldown retries
                // because `fetched_at` remains stale; unknown keys still cannot use this fallback.
                Err(error) => {
                    tracing::warn!(error = %error, "OIDC JWKS refresh failed; using cached keys")
                }
            }
            drop(permit);
        }
        cache
            .metadata
            .clone()
            .map(|metadata| (metadata, cache.generation))
            .ok_or_else(|| LibError::Internal("OIDC metadata cache is empty".into()))
    }

    async fn refresh_unknown_key(
        &self,
        st: &AppState,
        cfg: &StoredOidc,
        observed_generation: u64,
    ) -> Result<CoreProviderMetadata, LibError> {
        let mut cache = self.cache.lock().await;
        reset_for_config(&mut cache, cfg);
        // Another waiter already refreshed while this request was verifying. Reuse its result:
        // this is the singleflight edge, with no per-kid state for an attacker to grow.
        if cache.generation != observed_generation {
            return cache
                .metadata
                .clone()
                .ok_or_else(|| LibError::Internal("OIDC metadata cache is empty".into()));
        }
        if cache
            .last_unknown_refresh
            .is_some_and(|at| at.elapsed() < UNKNOWN_KID_COOLDOWN)
        {
            return cache
                .metadata
                .clone()
                .ok_or_else(|| LibError::Internal("OIDC metadata cache is empty".into()));
        }
        // Stamp before the network call. A failed or hostile issuer is rate-bounded too.
        cache.last_unknown_refresh = Some(Instant::now());
        let permit = st.auth_protection.discovery_permit()?;
        let refreshed = oidc::discover(cfg).await;
        drop(permit);
        if let Ok((metadata, _http)) = refreshed {
            cache.metadata = Some(metadata);
            cache.fetched_at = Some(Instant::now());
            cache.generation = cache.generation.wrapping_add(1);
        }
        cache.metadata.clone().ok_or(LibError::Unauthorized)
    }
}

fn reset_for_config(cache: &mut Cache, cfg: &StoredOidc) {
    if cache.issuer != cfg.config.issuer || cache.client_id != cfg.config.client_id {
        *cache = Cache {
            issuer: cfg.config.issuer.clone(),
            client_id: cfg.config.client_id.clone(),
            ..Cache::default()
        };
    }
}

#[derive(serde::Deserialize)]
struct UnverifiedClaims {
    iss: String,
    #[serde(default)]
    nbf: Option<i64>,
}

fn unverified_claims(token: &str) -> Result<UnverifiedClaims, LibError> {
    if token.len() > MAX_JWT_BYTES {
        return Err(LibError::Unauthorized);
    }
    let payload = token.split('.').nth(1).ok_or(LibError::Unauthorized)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| LibError::Unauthorized)?;
    serde_json::from_slice(&bytes).map_err(|_| LibError::Unauthorized)
}

fn verifier(
    cfg: &StoredOidc,
    metadata: &CoreProviderMetadata,
) -> Result<CoreIdTokenVerifier<'static>, LibError> {
    let issuer = IssuerUrl::new(cfg.config.issuer.clone()).map_err(|_| LibError::Unauthorized)?;
    Ok(CoreIdTokenVerifier::new_public_client(
        ClientId::new(cfg.config.client_id.clone()),
        issuer,
        metadata.jwks().clone(),
    )
    // Only asymmetric RS256 is accepted. In particular, a provider client secret never becomes
    // an HMAC JWT verification key, avoiding algorithm-confusion attacks.
    .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256]))
}

fn verify_with(
    token: &CoreIdToken,
    cfg: &StoredOidc,
    metadata: &CoreProviderMetadata,
) -> Result<Option<String>, LibError> {
    let verifier = verifier(cfg, metadata)?;
    if token.signing_key(&verifier).is_err() {
        return Ok(None);
    }
    let claims = token
        .claims(&verifier, ignore_nonce)
        .map_err(|_| LibError::Unauthorized)?;
    Ok(Some(claims.subject().as_str().to_string()))
}

fn ignore_nonce(_nonce: Option<&Nonce>) -> Result<(), String> {
    Ok(())
}

/// Verify a JWT bearer and resolve its `(issuer, subject)` through the ordinary account seam.
pub(crate) async fn verify(st: &AppState, raw: &str) -> Result<AuthContext, LibError> {
    st.store.require_oidc()?;
    let cfg = oidc::provider(st)?;
    let unverified = unverified_claims(raw)?;
    // This comparison happens before discovery, so a token naming an arbitrary issuer can never
    // turn this server into an SSRF client. The cryptographic verifier repeats the issuer check.
    if unverified.iss != cfg.config.issuer {
        return Err(LibError::Unauthorized);
    }
    let token: CoreIdToken = raw.parse().map_err(|_| LibError::Unauthorized)?;
    if token.signing_alg().ok() != Some(&CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256) {
        return Err(LibError::Unauthorized);
    }

    let (metadata, generation) = st.oidc_bearer.metadata(st, &cfg).await?;
    let subject = match verify_with(&token, &cfg, &metadata)? {
        Some(subject) => subject,
        None => {
            let refreshed = st
                .oidc_bearer
                .refresh_unknown_key(st, &cfg, generation)
                .await?;
            verify_with(&token, &cfg, &refreshed)?.ok_or(LibError::Unauthorized)?
        }
    };

    // `openidconnect` validates signature, issuer, audience, and expiration. `nbf` is a generic JWT
    // claim rather than an OIDC ID-token claim, so enforce it after signature validation here.
    if unverified
        .nbf
        .is_some_and(|not_before| unix_seconds() < not_before)
    {
        return Err(LibError::Unauthorized);
    }
    let account_id = st
        .store
        .oidc_account(&cfg.config.issuer, &subject)?
        .ok_or(LibError::Unauthorized)?;
    let account = st.store.get_account(&account_id)?;
    if account.disabled {
        return Err(LibError::Unauthorized);
    }
    let identity = AccountIdentity {
        account_id: account.account_id,
        username: account.username,
        role: account.role,
    };
    let visibility = st.store.resolve_visibility(&identity)?;
    Ok(AuthContext::connected(
        Some(identity.username.clone()),
        identity.role.scopes(),
        visibility,
    )
    .with_account(identity)
    .with_auth_source(AuthSource::OidcBearer))
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
