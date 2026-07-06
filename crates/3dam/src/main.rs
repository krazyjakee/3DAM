//! The `3dam` binary — role dispatch only (tech-spec 01 §5). It peeks at `argv` to choose a role,
//! then hands off to the owning crate. The GUI role runs synchronously (it will own its own event
//! loop); the CLI/serve/mcp roles run on a Tokio runtime.

use dam_frontend::{classify, Role};
use std::future::Future;
use std::process::ExitCode;

fn main() -> ExitCode {
    match classify(std::env::args_os()) {
        Role::Gui => ExitCode::from(dam_gui::run()),
        Role::Cli(argv) => on_runtime(dam_cli::run(argv)),
        Role::Serve(argv) => on_runtime(dam_cli::serve(argv)),
        Role::Mcp(argv) => on_runtime(dam_cli::mcp(argv)),
    }
}

/// Run a future to completion on a fresh multi-thread Tokio runtime.
fn on_runtime<F: Future<Output = ExitCode>>(fut: F) -> ExitCode {
    match tokio::runtime::Runtime::new() {
        Ok(rt) => rt.block_on(fut),
        Err(e) => {
            eprintln!("failed to start async runtime: {e}");
            ExitCode::FAILURE
        }
    }
}
