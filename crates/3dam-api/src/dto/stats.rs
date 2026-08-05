//! Library statistics: the aggregate counts the status bar and dashboards render.

use super::common::CountMap;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LibraryStats {
    pub total: u64,
    pub by_media: CountMap,
    pub by_source: CountMap,
    /// The most-used confirmed tags (asset count per tag) — the vocabulary that powers the Tags
    /// filter facet. Capped to the top handful so the sidebar stays a browsable summary, not the
    /// full long tail. Empty until assets carry confirmed tags.
    pub tags: CountMap,
    pub unanalyzed: u64,
    pub sources: u64,
}
