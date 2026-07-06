//! `cargo xtask` — dev automation. Phase 1 wires the CI aggregation; the dependency-graph guard
//! (`check-deps`, tech-spec 01 §2) is sketched here and fleshed out with `cargo metadata` later.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_default();
    let ok = match task.as_str() {
        "ci" => run("cargo", &["fmt", "--all", "--check"])
            && run("cargo", &["clippy", "--all-targets", "--", "-D", "warnings"])
            && run("cargo", &["test", "--workspace"]),
        "check-deps" => check_deps(),
        other => {
            eprintln!("unknown xtask '{other}'. try: ci | check-deps");
            false
        }
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Placeholder for the dependency-direction guard (tech-spec 01 §2): assert the allowed-edge
/// whitelist over `cargo metadata` and forbid GPU/UI/HTTP in `3dam-core`'s tree.
fn check_deps() -> bool {
    eprintln!(
        "check-deps: not yet implemented — will assert the crate-boundary rules (tech-spec 01 §2) \
         via `cargo metadata`."
    );
    true
}

fn run(cmd: &str, args: &[&str]) -> bool {
    eprintln!("$ {cmd} {}", args.join(" "));
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
