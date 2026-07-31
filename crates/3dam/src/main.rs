//! The `3dam` binary — role dispatch only (tech-spec 01 §5). It peeks at `argv` to choose a role,
//! then hands off to the owning crate. The GUI role runs synchronously (the webview shell owns its
//! own event loop); the CLI/serve/mcp roles run on a Tokio runtime.

use dam_frontend::{classify, Role};
use std::future::Future;
use std::process::ExitCode;
use std::time::Duration;

/// Grace the Tokio runtime gets to finish in-flight **blocking** work at shutdown before we stop
/// waiting and let the process exit. `spawn_blocking` tasks cannot be cancelled, so a long one —
/// e.g. registering a recursive filesystem watch over a large or network-backed root (`serve`'s
/// auto-rescan) — would otherwise make `Runtime`-drop block forever. The async side has already
/// drained connections by this point; this only bounds the teardown of detached blocking tasks.
const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    match classify(std::env::args_os()) {
        Role::Gui(argv) => ExitCode::from(dam_desktop::run(argv)),
        Role::Cli(argv) => on_runtime(dam_cli::run(argv)),
        Role::Serve(argv) => on_runtime(dam_cli::serve(argv)),
        Role::Mcp(argv) => on_runtime(dam_cli::mcp(argv)),
    }
}

/// Run a future to completion on a fresh multi-thread Tokio runtime.
fn on_runtime<F: Future<Output = ExitCode>>(fut: F) -> ExitCode {
    match tokio::runtime::Runtime::new() {
        Ok(rt) => {
            let code = rt.block_on(fut);
            // Bounded teardown: `Runtime`-drop waits on running `spawn_blocking` tasks forever, and
            // some (recursive watch registration, an in-flight scan) may not finish promptly. Give
            // them a short grace, then abandon them so the process always exits cleanly.
            rt.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
            code
        }
        Err(e) => {
            eprintln!("failed to start async runtime: {e}");
            ExitCode::FAILURE
        }
    }
}
