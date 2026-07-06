//! `dam-gui` — the native desktop shell (tech-spec 12, egui/eframe per ADR 0005).
//!
//! The GUI follows the web client in the roadmap, reusing the same `LibraryService` seam. Until
//! then `run` is a friendly stub so the `3dam` binary's no-verb GUI role dispatches somewhere real.

/// Entry point for the GUI role (bare `3dam`). Returns a process exit code.
pub fn run() -> u8 {
    eprintln!(
        "3dam GUI is not implemented yet.\n\
         Per the roadmap the web client (via `3dam serve`) and CLI come first.\n\
         Try:  3dam --help   |   3dam serve   |   3dam scan <dir>"
    );
    0
}
