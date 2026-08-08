//! HTTP resource-family route modules.

pub(super) mod assets;
pub(super) mod collections;
pub(super) mod jobs;
pub(super) mod sources;

// Keep transport helper tests at the crate-level router module while the implementations remain
// owned by their resource modules. These re-exports exist only in test builds.
#[cfg(test)]
pub(super) use assets::{parse_range, RangeSpec};
#[cfg(test)]
pub(super) use jobs::{managed_path_belongs_to, safe_archive_name};
