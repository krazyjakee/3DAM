//! `dam-gui` — the native desktop shell (tech-spec 12, egui/eframe per ADR 0005).
//!
//! The GUI follows the web client (web-first phasing, PRODUCT_SPEC §9), reusing the same `LibraryService` seam. Until
//! then `run` is a friendly stub so the `3dam` binary's no-verb GUI role dispatches somewhere real.
//!
//! ## UI-parity backlog (build these when the egui shell is real — golden rule 1)
//! Features already live in the web client that the native GUI still owes, so it reaches parity
//! rather than silently trailing:
//! - **Search mode selector** (semantic-search M5): a Keywords / Keywords+similar / Most-similar
//!   toggle beside the search box, setting `QueryRequest.mode` (`SearchMode`). The engine already
//!   honours it via `LibraryService::query`, so the GUI only owns the control + wiring.

/// Entry point for the GUI role (bare `3dam`). Returns a process exit code.
pub fn run() -> u8 {
    eprintln!(
        "3dam GUI is not implemented yet.\n\
         The web client (via `3dam serve`) and CLI come first.\n\
         Try:  3dam --help   |   3dam serve   |   3dam scan <dir>"
    );
    0
}
