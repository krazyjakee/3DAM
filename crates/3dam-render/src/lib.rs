//! `dam-render` — the wgpu renderer (tech-spec 06, [ADR 0002](../../docs/adr/0002-3d-render-crate-boundary.md)).
//!
//! Owns GPU work (headless render-to-PNG thumbnails, the multi-view render feeding shape
//! embeddings, and the surface shared with the GUI viewer) but **never** windowing — that stays in
//! `3dam-gui`. Not yet implemented; the headless-render spike (`spikes/headless-render/`) proves the
//! approach and this crate will port it.

/// Placeholder until the render phase. See `spikes/headless-render/` for the validated path.
pub const UNIMPLEMENTED: &str = "3dam-render is not yet implemented (see spikes/headless-render)";
