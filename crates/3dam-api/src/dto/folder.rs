//! Folder navigation (issue #66): the lazy, per-source subfolder listing the folder tree expands.

use crate::id::SourceId;
use serde::{Deserialize, Serialize};

/// Enumerate the immediate subfolders directly under `prefix` within one source — the lazy unit the
/// folder tree expands. `prefix` is source-relative with a trailing slash (or empty for the source
/// root); paths use `/` separators.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderListing {
    pub source: SourceId,
    #[serde(default)]
    pub prefix: String,
}

/// One immediate subfolder (issue #66): its segment `name` and how many assets live anywhere beneath
/// it (the whole subtree, so a collapsed folder still shows its weight).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderEntry {
    pub name: String,
    pub asset_count: u64,
}
