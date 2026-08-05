//! Request/response DTOs (tech-spec 03 §3–§4). These are the same structs used as Rust args
//! and JSON bodies. Only the phase-1 slice is modelled here; the rest slot in as their areas land.
//!
//! The DTOs live one module per domain, in the order their section banners used to appear in
//! this file (and still appear in `web/src/api/types.ts`). Everything is re-exported flat, so
//! callers keep writing `dam_api::AssetSummary` / `dam_api::dto::AssetSummary`.

pub mod analysis;
pub mod asset;
pub mod blocklist;
pub mod collection;
pub mod common;
pub mod convert;
pub mod discussion;
pub mod export;
pub mod folder;
pub mod job;
pub mod media;
pub mod query;
pub mod source;
pub mod stats;
pub mod upload;

pub use analysis::*;
pub use asset::*;
pub use blocklist::*;
pub use collection::*;
pub use common::*;
pub use convert::*;
pub use discussion::*;
pub use export::*;
pub use folder::*;
pub use job::*;
pub use media::*;
pub use query::*;
pub use source::*;
pub use stats::*;
pub use upload::*;
