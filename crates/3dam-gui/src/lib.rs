//! `dam-gui` — the native desktop shell (tech-spec 12, egui/eframe per ADR 0005).
//!
//! The GUI is a thin front-end over the `LibraryService` seam, exactly like the CLI and web client:
//! it holds a `Box<dyn LibraryService>` (embedded engine) and never reaches around the trait into
//! the store or engine internals. The service is async and egui's frame loop is synchronous, so a
//! background Tokio runtime owns the service and the two talk over channels — I/O never blocks a
//! frame (golden rule 5). See [`app`] for the browse/search/inspect workspace.
//!
//! ## Web ↔ egui parity
//! The web client leads (web-first phasing, PRODUCT_SPEC §9); this shell follows it. The tracked
//! gap list lives in `docs/GUI_PARITY.md` (referenced from `CLAUDE.md`) so new user-facing features
//! land in both clients rather than silently diverging.

mod app;
mod theme;
mod viewer3d;

use std::sync::Arc;

use dam_api::service::{AuthContext, LibraryService};
use dam_frontend::{default_data_dir, open_backend, Backend};

/// Entry point for the GUI role (bare `3dam`). Opens the embedded library, then hands the main
/// thread to eframe's event loop. Returns a process exit code.
pub fn run() -> u8 {
    // A multi-thread runtime backs every service call (and stays alive for the window's lifetime).
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("3dam GUI: failed to start async runtime: {e}");
            return 1;
        }
    };

    // Embedded, at the platform-default data dir — the same library the CLI opens with no `--data`.
    let data_dir = default_data_dir();
    let lib: Arc<dyn LibraryService> =
        match rt.block_on(open_backend(Backend::Embedded { data_dir })) {
            Ok(lib) => Arc::from(lib),
            Err(e) => {
                eprintln!("3dam GUI: failed to open library: {e}");
                return 1;
            }
        };

    let rt = Arc::new(rt);
    let auth = AuthContext::embedded();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("3DAM")
            .with_inner_size([1200.0, 760.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };

    match eframe::run_native(
        "3DAM",
        options,
        Box::new(move |cc| Ok(Box::new(app::DamGui::new(cc, rt, lib, auth)))),
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("3dam GUI: {e}");
            1
        }
    }
}
