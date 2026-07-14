//! Federation wire types (tech-spec 07 §4–§6, ADR 0009 §5, phase 6 — issue #39).
//!
//! A federated source is a *client* of a peer's read API: these are the few shapes that exist only
//! on that edge. The federation protocol is a versioned subset of the read surface (`query`,
//! `get_asset`, preview fetch, `similar` by vector) plus `advertise()`. Compatibility is
//! major-version exact-match; newer peers degrade to the caller's version and unknown fields are
//! ignored (forward-compatible).

use crate::dto::{Filter, MediaType};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The federation protocol version this build speaks (semver).
pub const FEDERATION_PROTOCOL_VERSION: &str = "1.0.0";

/// `true` when two protocol versions can talk: exact match on the semver **major** component.
/// Anything unparseable is incompatible — better to refuse a peer than mis-merge its rows.
pub fn protocol_compatible(a: &str, b: &str) -> bool {
    match (major(a), major(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn major(v: &str) -> Option<u64> {
    v.split('.').next()?.parse().ok()
}

/// Peer self-description (`GET /api/v1/advertise`) — cached with a TTL by the caller. Carries the
/// protocol version, catalog weight, and — load-bearing for cross-peer similarity (issue #40) —
/// the peer's `EmbeddingSpace` id per media type. Served only while the `federation` runtime flag
/// is on: off means the surface disappears (ADR 0004).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerAdvertise {
    /// Semver federation protocol version; see [`protocol_compatible`].
    pub protocol_version: String,
    /// Human display name for "similar on <peer>" style attribution.
    pub instance: String,
    /// Total catalog rows the peer would answer over (a health/weight hint, not a contract).
    pub assets: u64,
    /// `media type → EmbeddingSpace id` (e.g. `"image" → "image-stats-v1"`). Cross-peer similarity
    /// is gated on an **exact** space match (issue #40) — never compare distances across spaces.
    #[serde(default)]
    pub spaces: BTreeMap<String, String>,
}

/// "Find similar" **by vector** (`POST /api/v1/similar-by-vector`) — the federated form of
/// `SimilarRequest`. The caller embeds locally and ships the vector; the peer ranks against its
/// own index and returns its top-k. The peer never re-embeds and the caller never runs the peer's
/// ANN (tech-spec 07 §4). `space` must name a space the peer actually has — a mismatch is an
/// error, not a silent empty page, so the caller's gate stays honest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorSimilarRequest {
    pub media: MediaType,
    /// The `EmbeddingSpace` id the vector lives in; must match the peer's space for `media`.
    pub space: String,
    pub vector: Vec<f32>,
    #[serde(default = "default_k")]
    pub k: u32,
    /// Post-filters, same grammar as text search.
    #[serde(default)]
    pub filters: Vec<Filter>,
}

fn default_k() -> u32 {
    24
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compat_is_major_exact_match() {
        assert!(protocol_compatible("1.0.0", "1.2.3"));
        assert!(!protocol_compatible("1.0.0", "2.0.0"));
        assert!(!protocol_compatible("junk", "1.0.0"));
    }
}
