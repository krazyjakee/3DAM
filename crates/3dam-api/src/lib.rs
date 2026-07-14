//! `3dam-api` — the seam. The `LibraryService` trait, its DTOs, and the error model.
//!
//! This crate is deliberately the thinnest thing in the workspace (tech-spec 01 §1): it depends
//! on `serde` (+ `async-trait`/`futures` for the async trait) and nothing that does I/O, GPU, or
//! HTTP, so it costs nothing to link everywhere and imposes no transitive weight. Everyone
//! depends on it; it depends on no other workspace crate.

pub mod admin;
pub mod dto;
pub mod error;
pub mod event;
pub mod federation;
pub mod id;
pub mod page;
pub mod service;

/// Unix epoch milliseconds — the single time unit used across the schema and job/audit timestamps
/// (tech-spec 02 §3.1). Shared here so every crate reads the clock the same way.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Flat re-exports so consumers write `dam_api::AssetId`, `dam_api::LibError`, etc.
pub use dto::*;
pub use error::{internal, ErrorBody, LibError};
pub use event::{ChangeKind, EventTopic, JobEvent, LibraryEvent, SubscribeRequest};
pub use federation::{
    protocol_compatible, PeerAdvertise, VectorSimilarRequest, FEDERATION_PROTOCOL_VERSION,
};
pub use id::{AssetId, CollectionId, ContentHash, JobId, SourceId, TagId};
pub use page::{Cursor, ItemWarning, Page, PageParams, PartialStatus};
pub use service::{AuthContext, EventStream, LibraryService, Scope, Scopes};
