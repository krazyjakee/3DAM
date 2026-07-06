//! `cargo xtask` — dev automation. Phase 1 wires the CI aggregation; the dependency-graph guard
//! (`check-deps`, tech-spec 01 §2) is sketched here and fleshed out with `cargo metadata` later.

use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_default();
    let ok = match task.as_str() {
        // `web` builds the React client first so its dist/ exists before `cargo build` embeds it
        // (tech-spec 09 §A.4, tech-spec 15 §15.5: build the web client before the native build).
        "ci" => {
            build_web()
                && run("cargo", &["fmt", "--all", "--check"])
                && run(
                    "cargo",
                    &["clippy", "--all-targets", "--", "-D", "warnings"],
                )
                && run("cargo", &["test", "--workspace"])
        }
        "web" => build_web(),
        // Build just the WASM viewer islands (tech-spec 09 §B.3) into web/src/wasm/.
        "wasm" => build_wasm(),
        "check-deps" => check_deps(),
        other => {
            eprintln!("unknown xtask '{other}'. try: ci | web | wasm | check-deps");
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

/// Build the web client into `web/dist/` (consumed by the server's `rust-embed`, tech-spec 09 §A.4).
/// Skips gracefully with a hint if `pnpm` is absent — the native build still works against whatever
/// `web/dist/` already exists (the server serves a build hint when it is empty).
fn build_web() -> bool {
    // The WASM viewer islands (tech-spec 09 §B.3) are built first so their pkg exists under
    // web/src/wasm/ before Vite bundles them into dist/ — one `rust-embed` step then ships both
    // the React bundle and the `.wasm` in the single binary (§A.4).
    if !build_wasm() {
        return false;
    }
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("../web");
    if which("pnpm").is_none() {
        eprintln!(
            "xtask web: `pnpm` not found — skipping web build (install Node/pnpm to build the UI)."
        );
        return true;
    }
    run_in(&web, "pnpm", &["install", "--frozen-lockfile"]) && run_in(&web, "pnpm", &["build"])
}

/// Build the `dam-viewer` WASM islands (3D + waveform, tech-spec 09 §B.3 / ADR 0009 §9) with
/// `wasm-pack` into `web/src/wasm/` (a gitignored build artifact the web client lazily imports).
/// Skips gracefully with a hint if `wasm-pack` is absent, mirroring the `pnpm` handling above — the
/// native build still works; only the browser 3D/waveform islands are unavailable until it is built.
fn build_wasm() -> bool {
    if which("wasm-pack").is_none() {
        eprintln!(
            "xtask wasm: `wasm-pack` not found — skipping island build (install with \
             `cargo install wasm-pack` to build the 3D/waveform viewers)."
        );
        return true;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    // wasm-pack's --out-dir is relative to the crate, so resolve it to an absolute path.
    let out_dir = match root.join("web/src/wasm").canonicalize() {
        Ok(p) => p,
        // The dir may not exist yet on a clean checkout; wasm-pack creates it, so fall back to the
        // (possibly non-canonical) join, which is still absolute enough for wasm-pack.
        Err(_) => std::fs::canonicalize(&root)
            .unwrap_or(root.clone())
            .join("web/src/wasm"),
    };
    let crate_dir = root.join("crates/3dam-viewer");
    run(
        "wasm-pack",
        &[
            "build",
            &crate_dir.to_string_lossy(),
            "--target",
            "web",
            "--out-dir",
            &out_dir.to_string_lossy(),
            "--out-name",
            "dam_viewer",
            "--release",
        ],
    )
}

fn which(bin: &str) -> Option<()> {
    Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| ())
}

fn run_in(dir: &Path, cmd: &str, args: &[&str]) -> bool {
    eprintln!("$ (cd {}) {cmd} {}", dir.display(), args.join(" "));
    Command::new(cmd)
        .current_dir(dir)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run(cmd: &str, args: &[&str]) -> bool {
    eprintln!("$ {cmd} {}", args.join(" "));
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
