//! `3dam-api` — the seam. The `LibraryService` trait, its DTOs, and the error model.
//!
//! This crate is deliberately the thinnest thing in the workspace (tech-spec 01 §1): it depends
//! on `serde` (+ `async-trait`/`futures` for the async trait) and nothing that does I/O, GPU, or
//! HTTP, so it costs nothing to link everywhere and imposes no transitive weight. Everyone
//! depends on it; it depends on no other workspace crate.

pub mod dto;
pub mod error;
pub mod event;
pub mod id;
pub mod page;
pub mod service;

// Flat re-exports so consumers write `dam_api::AssetId`, `dam_api::LibError`, etc.
pub use dto::*;
pub use error::{ErrorBody, LibError};
pub use event::{ChangeKind, EventTopic, JobEvent, LibraryEvent, SubscribeRequest};
pub use id::{AssetId, CollectionId, ContentHash, JobId, SourceId, TagId};
pub use page::{Cursor, ItemWarning, Page, PageParams, PartialStatus};
pub use service::{AuthContext, EventStream, LibraryService};
