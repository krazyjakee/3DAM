//! In-memory protection for the unauthenticated account and OIDC surfaces.
//!
//! Two independent token buckets are charged where an identity is available: one for the
//! trustworthy network client and one for a one-way digest of the normalized identity. The first
//! makes rotating usernames/states useless; the second stops a distributed spray against one
//! account. Buckets are process-local by design: account lockout remains durable, while this layer
//! bounds the work one server process can accept.

use crate::{ApiError, AppState};
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::{request::Parts, HeaderMap};
use dam_api::LibError;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_BUCKETS: usize = 10_000;
const PASSWORD_HASH_SLOTS: usize = 4;
const OIDC_DISCOVERY_SLOTS: usize = 4;

/// Socket peer supplied by axum's connect-info service. Missing connect info is deliberately an
/// attribution result rather than an extractor failure: all unknown clients share one bucket.
pub(crate) struct PeerAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|connect| connect.0),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Endpoint {
    Status,
    Claim,
    Login,
    OidcStart,
    OidcCallback,
}

impl Endpoint {
    fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Claim => "claim",
            Self::Login => "login",
            Self::OidcStart => "oidc_start",
            Self::OidcCallback => "oidc_callback",
        }
    }

    /// `(peer policy, optional identity policy, optional process-wide policy)`. Each duration is
    /// the time to replenish one token, not a fixed window, so capacity refills smoothly.
    fn policies(self) -> (Policy, Option<Policy>, Option<Policy>) {
        match self {
            Self::Status => (Policy::new(60, 1), None, None),
            Self::Claim => (
                Policy::new(5, 60),
                Some(Policy::new(5, 60)),
                Some(Policy::new(10, 30)),
            ),
            Self::Login => (
                Policy::new(20, 3),
                Some(Policy::new(12, 5)),
                Some(Policy::new(60, 1)),
            ),
            Self::OidcStart => (Policy::new(10, 6), None, Some(Policy::new(20, 3))),
            // A callback is cheap until its state is valid, and a generous independent budget
            // lets real users finish starts already admitted by the stricter start limiter.
            Self::OidcCallback => (
                Policy::new(30, 2),
                Some(Policy::new(5, 12)),
                Some(Policy::new(60, 1)),
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct Policy {
    burst: f64,
    one_token: Duration,
}

impl Policy {
    const fn new(burst: u32, seconds_per_token: u64) -> Self {
        Self {
            burst: burst as f64,
            one_token: Duration::from_secs(seconds_per_token),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ClientKey {
    /// A direct connection, or a client address asserted by a specifically trusted peer. Keeping
    /// the proxy peer in the key means two independent ingress tiers do not share attribution.
    Attributed { peer: IpAddr, client: IpAddr },
    /// Missing/malformed attribution from a trusted proxy fails closed into one bucket per proxy.
    UnknownBehindProxy(IpAddr),
    /// The in-process/otherwise-unattributed seam shares one global bucket.
    Unknown,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum LimitKey {
    Client(Endpoint, ClientKey),
    Global(Endpoint),
    /// Digest only: normalized usernames and OIDC state never become retained/loggable keys.
    Identity(Endpoint, blake3::Hash),
    /// Once the bounded map is full, all new cardinality shares an overflow bucket per endpoint.
    Overflow(Endpoint),
}

struct Bucket {
    tokens: f64,
    updated: Instant,
    denial_signalled: bool,
}

struct Limited {
    retry_after: u32,
    signal: bool,
    endpoint: Endpoint,
    reason: &'static str,
}

#[derive(Default)]
struct TokenBuckets {
    buckets: Mutex<HashMap<LimitKey, Bucket>>,
}

impl TokenBuckets {
    fn check(&self, key: LimitKey, policy: Policy, now: Instant) -> Result<(), (u32, bool)> {
        let mut buckets = self.buckets.lock().unwrap();
        let key = if buckets.contains_key(&key) || buckets.len() < MAX_BUCKETS {
            key
        } else {
            // Do not let source-address/username cardinality turn the limiter itself into a memory
            // denial of service. Existing buckets keep working; new ones contend here.
            match key {
                LimitKey::Client(endpoint, _) | LimitKey::Identity(endpoint, _) => {
                    LimitKey::Overflow(endpoint)
                }
                key @ LimitKey::Global(_) => key,
                key @ LimitKey::Overflow(_) => key,
            }
        };
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: policy.burst,
            updated: now,
            denial_signalled: false,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        let one_token = policy.one_token.as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed / one_token).min(policy.burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            bucket.denial_signalled = false;
            return Ok(());
        }
        let retry_after = (((1.0 - bucket.tokens) * one_token).ceil() as u32).max(1);
        let signal = !bucket.denial_signalled;
        bucket.denial_signalled = true;
        Err((retry_after, signal))
    }
}

pub(crate) struct AuthProtection {
    buckets: TokenBuckets,
    trusted_proxies: HashSet<IpAddr>,
    password_hashes: Arc<Semaphore>,
    oidc_discovery: Arc<Semaphore>,
}

impl AuthProtection {
    pub(crate) fn new(trusted_proxies: impl IntoIterator<Item = IpAddr>) -> Self {
        Self {
            buckets: TokenBuckets::default(),
            trusted_proxies: trusted_proxies.into_iter().map(canonical_ip).collect(),
            password_hashes: Arc::new(Semaphore::new(PASSWORD_HASH_SLOTS)),
            oidc_discovery: Arc::new(Semaphore::new(OIDC_DISCOVERY_SLOTS)),
        }
    }

    fn client_key(&self, peer: Option<SocketAddr>, headers: &HeaderMap) -> ClientKey {
        let Some(peer) = peer.map(|socket| canonical_ip(socket.ip())) else {
            return ClientKey::Unknown;
        };
        if !self.trusted_proxies.contains(&peer) {
            // Forwarding headers from a direct/untrusted peer are untrusted input and ignored.
            return ClientKey::Attributed { peer, client: peer };
        }

        // Trusted-proxy contract: strip the inbound header and set exactly one bare IP. Multiple
        // values, comma lists, ports, obfuscated identifiers, or invalid UTF-8 are unknown rather
        // than guessed. Unknown clients contend in one bucket, which is the safe failure mode.
        let values: Vec<_> = headers.get_all("x-forwarded-for").iter().collect();
        let forwarded = match values.as_slice() {
            [value] => value
                .to_str()
                .ok()
                .filter(|value| !value.contains(','))
                .and_then(|value| value.trim().parse::<IpAddr>().ok())
                .map(canonical_ip),
            _ => None,
        };
        match forwarded {
            Some(client) => ClientKey::Attributed { peer, client },
            None => ClientKey::UnknownBehindProxy(peer),
        }
    }

    fn check_at(
        &self,
        endpoint: Endpoint,
        peer: Option<SocketAddr>,
        headers: &HeaderMap,
        identity: Option<&str>,
        now: Instant,
    ) -> Result<(), Limited> {
        let (client_policy, identity_policy, global_policy) = endpoint.policies();
        let client = self.client_key(peer, headers);
        self.buckets
            .check(LimitKey::Client(endpoint, client), client_policy, now)
            .map_err(|(retry_after, signal)| Limited {
                retry_after,
                signal,
                endpoint,
                reason: "client",
            })?;
        if let Some(policy) = global_policy {
            self.buckets
                .check(LimitKey::Global(endpoint), policy, now)
                .map_err(|(retry_after, signal)| Limited {
                    retry_after,
                    signal,
                    endpoint,
                    reason: "global",
                })?;
        }
        if let (Some(identity), Some(policy)) = (identity, identity_policy) {
            let clipped: String = identity.trim().chars().take(128).collect();
            // Account uniqueness is SQLite `NOCASE` (ASCII folding), so mirror that exact
            // equivalence here. OIDC state is opaque and case-sensitive, so preserve it.
            let normalized = match endpoint {
                Endpoint::Claim | Endpoint::Login => clipped.to_ascii_lowercase(),
                Endpoint::OidcCallback => clipped,
                Endpoint::Status | Endpoint::OidcStart => unreachable!("no identity policy"),
            };
            let digest = blake3::hash(normalized.as_bytes());
            self.buckets
                .check(LimitKey::Identity(endpoint, digest), policy, now)
                .map_err(|(retry_after, signal)| Limited {
                    retry_after,
                    signal,
                    endpoint,
                    reason: "identity",
                })?;
        }
        Ok(())
    }

    pub(crate) fn password_permit(&self) -> Result<OwnedSemaphorePermit, LibError> {
        self.password_hashes
            .clone()
            .try_acquire_owned()
            .map_err(|_| LibError::RateLimited { retry_after: 1 })
    }

    pub(crate) fn discovery_permit(&self) -> Result<OwnedSemaphorePermit, LibError> {
        self.oidc_discovery
            .clone()
            .try_acquire_owned()
            .map_err(|_| LibError::RateLimited { retry_after: 1 })
    }

    /// Reserve outbound capacity before running a state-consuming preparation step. The callback
    /// uses this seam so a capacity refusal provably cannot delete its single-use login state.
    pub(crate) fn with_discovery_capacity<T>(
        &self,
        prepare: impl FnOnce() -> Result<T, LibError>,
    ) -> Result<(OwnedSemaphorePermit, T), LibError> {
        let permit = self.discovery_permit()?;
        let prepared = prepare()?;
        Ok((permit, prepared))
    }
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

/// Apply a rate decision and emit one coalesced audit/log signal per blocked bucket interval. No
/// address, username, OIDC state, authorization code, cookie, or callback query is recorded.
pub(crate) fn enforce(
    st: &AppState,
    endpoint: Endpoint,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    identity: Option<&str>,
) -> Result<(), ApiError> {
    st.auth_protection
        .check_at(endpoint, peer, headers, identity, Instant::now())
        .map_err(|limited| {
            if limited.signal {
                tracing::warn!(
                    endpoint = limited.endpoint.name(),
                    reason = limited.reason,
                    retry_after = limited.retry_after,
                    "unauthenticated auth request rate limited"
                );
                let _ = st.store.audit(
                    "auth-limiter",
                    "account.auth_rate_limited",
                    None,
                    Some(serde_json::json!({
                        "endpoint": limited.endpoint.name(),
                        "reason": limited.reason,
                        "retry_after": limited.retry_after,
                    })),
                );
            }
            ApiError(LibError::RateLimited {
                retry_after: limited.retry_after,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn socket(ip: &str) -> SocketAddr {
        format!("{ip}:1234").parse().unwrap()
    }

    #[test]
    fn token_bucket_bursts_then_refills_smoothly() {
        let buckets = TokenBuckets::default();
        let key = LimitKey::Overflow(Endpoint::Login);
        let policy = Policy::new(2, 10);
        let start = Instant::now();
        assert!(buckets.check(key.clone(), policy, start).is_ok());
        assert!(buckets.check(key.clone(), policy, start).is_ok());
        assert_eq!(buckets.check(key.clone(), policy, start), Err((10, true)));
        assert_eq!(
            buckets.check(key.clone(), policy, start + Duration::from_secs(4)),
            Err((6, false))
        );
        assert!(buckets
            .check(key, policy, start + Duration::from_secs(10))
            .is_ok());
    }

    #[test]
    fn rotating_usernames_cannot_bypass_the_client_budget() {
        let protection = AuthProtection::new([]);
        let headers = HeaderMap::new();
        let peer = Some(socket("203.0.113.8"));
        let now = Instant::now();
        for n in 0..20 {
            assert!(protection
                .check_at(
                    Endpoint::Login,
                    peer,
                    &headers,
                    Some(&format!("user-{n}")),
                    now
                )
                .is_ok());
        }
        assert!(matches!(
            protection.check_at(Endpoint::Login, peer, &headers, Some("new-user"), now),
            Err(Limited {
                reason: "client",
                ..
            })
        ));
    }

    #[test]
    fn rotating_clients_cannot_create_unbounded_oidc_discovery_rate() {
        let protection = AuthProtection::new([]);
        let headers = HeaderMap::new();
        let now = Instant::now();
        for host in 1..=20 {
            assert!(protection
                .check_at(
                    Endpoint::OidcStart,
                    Some(socket(&format!("203.0.113.{host}"))),
                    &headers,
                    None,
                    now,
                )
                .is_ok());
        }
        assert!(matches!(
            protection.check_at(
                Endpoint::OidcStart,
                Some(socket("198.51.100.1")),
                &headers,
                None,
                now,
            ),
            Err(Limited {
                reason: "global",
                ..
            })
        ));
    }

    #[test]
    fn rotating_clients_cannot_bypass_normalized_account_budget() {
        let protection = AuthProtection::new([]);
        let headers = HeaderMap::new();
        let now = Instant::now();
        for host in 1..=12 {
            let account = if host % 2 == 0 { " Alice " } else { "ALICE" };
            assert!(protection
                .check_at(
                    Endpoint::Login,
                    Some(socket(&format!("198.51.100.{host}"))),
                    &headers,
                    Some(account),
                    now,
                )
                .is_ok());
        }
        assert!(matches!(
            protection.check_at(
                Endpoint::Login,
                Some(socket("203.0.113.1")),
                &headers,
                Some("alice"),
                now,
            ),
            Err(Limited {
                reason: "identity",
                ..
            })
        ));
    }

    #[test]
    fn forwarding_headers_only_count_from_an_explicitly_trusted_peer() {
        let trusted: IpAddr = "127.0.0.1".parse().unwrap();
        let protection = AuthProtection::new([trusted]);
        let mut one = HeaderMap::new();
        one.insert("x-forwarded-for", "2001:db8::5".parse().unwrap());
        assert_eq!(
            protection.client_key(Some(socket("127.0.0.1")), &one),
            ClientKey::Attributed {
                peer: trusted,
                client: "2001:db8::5".parse().unwrap(),
            }
        );

        // The exact same attacker-controlled header on a direct connection is ignored.
        assert_eq!(
            protection.client_key(Some(socket("203.0.113.8")), &one),
            ClientKey::Attributed {
                peer: "203.0.113.8".parse().unwrap(),
                client: "203.0.113.8".parse().unwrap(),
            }
        );

        let mut list = HeaderMap::new();
        list.insert(
            "x-forwarded-for",
            "203.0.113.8, 198.51.100.4".parse().unwrap(),
        );
        assert_eq!(
            protection.client_key(Some(socket("127.0.0.1")), &list),
            ClientKey::UnknownBehindProxy(trusted)
        );
        assert_eq!(
            protection.client_key(Some(socket("127.0.0.1")), &HeaderMap::new()),
            ClientKey::UnknownBehindProxy(trusted)
        );
    }

    #[test]
    fn ipv4_mapped_ipv6_and_ipv4_share_an_identity() {
        let protection = AuthProtection::new([]);
        let headers = HeaderMap::new();
        assert_eq!(
            protection.client_key(Some(socket("203.0.113.8")), &headers),
            protection.client_key(Some("[::ffff:203.0.113.8]:1234".parse().unwrap()), &headers)
        );
        assert_ne!(
            protection.client_key(Some(socket("203.0.113.8")), &headers),
            protection.client_key(Some("[2001:db8::8]:1234".parse().unwrap()), &headers)
        );
    }

    #[test]
    fn exhausted_concurrency_slots_fail_without_queueing() {
        let protection = AuthProtection::new([]);
        let password: Vec<_> = (0..PASSWORD_HASH_SLOTS)
            .map(|_| protection.password_permit().unwrap())
            .collect();
        assert!(matches!(
            protection.password_permit(),
            Err(LibError::RateLimited { retry_after: 1 })
        ));
        let discovery: Vec<_> = (0..OIDC_DISCOVERY_SLOTS)
            .map(|_| protection.discovery_permit().unwrap())
            .collect();
        assert!(matches!(
            protection.discovery_permit(),
            Err(LibError::RateLimited { retry_after: 1 })
        ));
        drop((password, discovery));
    }

    #[test]
    fn discovery_capacity_refusal_does_not_run_state_consuming_preparation() {
        let protection = AuthProtection::new([]);
        let permits: Vec<_> = (0..OIDC_DISCOVERY_SLOTS)
            .map(|_| protection.discovery_permit().unwrap())
            .collect();
        let consumed = std::sync::atomic::AtomicBool::new(false);
        let result = protection.with_discovery_capacity(|| {
            consumed.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
        assert!(matches!(
            result,
            Err(LibError::RateLimited { retry_after: 1 })
        ));
        assert!(
            !consumed.load(std::sync::atomic::Ordering::Relaxed),
            "OIDC state preparation must not run until discovery capacity is reserved"
        );
        drop(permits);
    }

    #[test]
    fn typed_rate_limit_response_carries_retry_after() {
        let response = ApiError(LibError::RateLimited { retry_after: 7 }).into_response();
        assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "7");
    }
}
