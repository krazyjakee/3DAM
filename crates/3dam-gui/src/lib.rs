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
mod ui;
mod viewer3d;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use dam_api::service::{AuthContext, LibraryService};
use dam_frontend::{default_data_dir, open_backend, Backend};

pub use app::Conn;

/// Entry point for the GUI role. Bare `3dam` opens the platform-default embedded library; the
/// hosted-mode flags `--connect <url> [--token <t>]` launch straight against a remote `3dam serve`
/// (issue #70), and `--data <dir>` overrides the embedded location. Then the main thread is handed
/// to eframe's event loop. Returns a process exit code.
pub fn run(args: Vec<OsString>) -> u8 {
    let launch = match LaunchArgs::parse(&args) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("3dam GUI: {e}");
            return 2;
        }
    };

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

    // Resolve the initial backend. In connected mode `open_backend` only builds the HTTP client (no
    // handshake), so a down server does not block launch — the app opens and shows an offline state
    // until the server answers. In embedded mode a failure to open the local DB is fatal.
    let data_dir = launch.data.clone().unwrap_or_else(default_data_dir);
    let (backend, conn) = match &launch.connect {
        Some(url) => match url::Url::parse(url) {
            Ok(endpoint) => (
                Backend::Connected {
                    endpoint: endpoint.clone(),
                    token: launch.token.clone(),
                },
                Conn::remote(endpoint, launch.token.clone()),
            ),
            Err(e) => {
                eprintln!("3dam GUI: invalid --connect URL '{url}': {e}");
                return 2;
            }
        },
        None => (
            Backend::Embedded {
                data_dir: data_dir.clone(),
            },
            Conn::embedded(),
        ),
    };

    let lib: Arc<dyn LibraryService> = match rt.block_on(open_backend(backend)) {
        Ok(lib) => Arc::from(lib),
        Err(e) => {
            eprintln!("3dam GUI: failed to open library: {e}");
            return 1;
        }
    };

    let rt = Arc::new(rt);
    // The embedded context is used in both modes: the `ApiClient` carries its bearer token in the
    // request headers, so the context it is handed is advisory only (matches the CLI, dispatch.rs).
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
        Box::new(move |cc| {
            Ok(Box::new(app::DamGui::new(
                cc, rt, lib, auth, conn, data_dir,
            )))
        }),
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("3dam GUI: {e}");
            1
        }
    }
}

/// The launch flags the GUI understands — a hand parser (kept off clap so the GUI crate needn't link
/// the CLI grammar). Mirrors the CLI global flags relevant to a front-end.
#[derive(Default)]
struct LaunchArgs {
    connect: Option<String>,
    token: Option<String>,
    data: Option<PathBuf>,
}

impl LaunchArgs {
    fn parse(args: &[OsString]) -> Result<Self, String> {
        let mut out = LaunchArgs::default();
        let mut i = 0;
        while i < args.len() {
            let tok = args[i]
                .to_str()
                .ok_or_else(|| "non-UTF-8 argument".to_string())?;
            // Support both `--flag value` and `--flag=value`.
            let (flag, inline) = match tok.split_once('=') {
                Some((f, v)) => (f, Some(v.to_string())),
                None => (tok, None),
            };
            let mut take_value = |name: &str| -> Result<String, String> {
                if let Some(v) = &inline {
                    Ok(v.clone())
                } else {
                    i += 1;
                    args.get(i)
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .ok_or_else(|| format!("{name} requires a value"))
                }
            };
            match flag {
                "--connect" => out.connect = Some(take_value("--connect")?),
                "--token" => out.token = Some(take_value("--token")?),
                "--data" => out.data = Some(PathBuf::from(take_value("--data")?)),
                other => return Err(format!("unknown option '{other}'")),
            }
            i += 1;
        }
        Ok(out)
    }
}
