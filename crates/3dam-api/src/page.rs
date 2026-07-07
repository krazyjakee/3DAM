//! Cursor pagination and the fail-soft partial carrier (tech-spec 03 §4.1, §6.1).

use serde::{Deserialize, Serialize};

/// Opaque, server-encoded continuation token. Only valid against the query that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub String);

/// Page request: cursor + limit (offset is O(n) and unstable at scale — tech-spec 03 §6.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageParams {
    #[serde(default)]
    pub after: Option<Cursor>,
    pub limit: u32,
}

impl Default for PageParams {
    fn default() -> Self {
        Self {
            after: None,
            limit: 100,
        }
    }
}

impl PageParams {
    /// Clamp a caller-supplied limit into `[1, max]`.
    pub fn clamped(&self, max: u32) -> u32 {
        self.limit.clamp(1, max)
    }
}

/// The paginated envelope. `cursor: None` ⇒ end of results.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(default = "Option::default")]
    pub cursor: Option<Cursor>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub partial: PartialStatus,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, cursor: Option<Cursor>) -> Self {
        Self {
            items,
            cursor,
            total: None,
            partial: PartialStatus::default(),
        }
    }
    pub fn empty() -> Self {
        Self::new(Vec::new(), None)
    }
}

/// Soft, per-call degradation record — NOT an error (tech-spec 03 §4.1). A `complete: false`
/// result carries the reasons so callers render a usable partial rather than failing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartialStatus {
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ItemWarning>,
}

impl Default for PartialStatus {
    fn default() -> Self {
        Self {
            complete: true,
            warnings: Vec::new(),
        }
    }
}

/// One soft per-item/per-source failure (bad asset skipped, source offline, …).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemWarning {
    pub subject: String,
    pub code: String,
    pub message: String,
}
