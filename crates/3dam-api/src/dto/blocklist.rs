//! Removal and the content-hash blocklist (issue #21): dropping a catalog row without letting a
//! later scan re-import the same bytes.

use crate::id::ContentHash;
use serde::{Deserialize, Serialize};

/// Remove one asset from the catalog. Optionally record its content hash on the blocklist so a
/// later scan/watch/auto-rescan never re-imports the same bytes (§2.2). Non-destructive to the
/// source: only the catalog row is deleted, the file on disk is untouched.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemoveAsset {
    /// Also block the asset's content hash from being re-imported by any future scan.
    #[serde(default)]
    pub block: bool,
}

/// One blocked content hash — the review surface for the "removed + blocked" set. Carries the
/// last-known filename (captured at block time) so the entry is human-recognisable after the row
/// it came from is gone.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockEntry {
    pub hash: ContentHash,
    /// The filename the asset had when it was blocked (best-effort, for display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub blocked_at: i64,
}
