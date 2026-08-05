//! Collections and smart folders (PRODUCT_SPEC §3, §6.4): named static or query-backed asset sets.

use super::query::QueryRequest;
use crate::id::{AssetId, CollectionId};
use serde::{Deserialize, Serialize};

/// Two kinds of set. A **manual** collection holds an explicit, hand-curated member list. A
/// **smart** folder holds a saved query and resolves *live* — its members are whatever currently
/// matches, so a smart folder like *safe-to-ship* stays correct as the library changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionKind {
    #[default]
    Manual,
    Smart,
}

impl CollectionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CollectionKind::Manual => "manual",
            CollectionKind::Smart => "smart",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(CollectionKind::Manual),
            "smart" => Some(CollectionKind::Smart),
            _ => None,
        }
    }
}

/// A collection or smart folder record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collection {
    pub id: CollectionId,
    pub name: String,
    pub kind: CollectionKind,
    /// The saved query backing a smart folder (live set); `None` for a manual collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
    /// Member count: exact for a manual collection; the current match count for a smart folder when
    /// it was computed, else `None` (list views may skip the per-folder query for cheapness).
    #[serde(default)]
    pub count: Option<u64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Create a collection. A smart folder must carry a `query`; a manual collection ignores it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewCollection {
    pub name: String,
    #[serde(default)]
    pub kind: CollectionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
}

/// Patch a collection: rename and/or (smart folders) replace the saved query. Absent fields are
/// left unchanged.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateCollection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
}

/// Add/remove members of a **manual** collection (a smart folder's membership is query-driven and
/// cannot be edited directly).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CollectionMembers {
    #[serde(default)]
    pub add: Vec<AssetId>,
    #[serde(default)]
    pub remove: Vec<AssetId>,
}
