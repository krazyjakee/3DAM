//! `cargo xtask` — dev automation, including the dependency-graph guard from tech-spec 01 §2.

use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_default();
    let ok = match task.as_str() {
        // `web` builds the React client first so its dist/ exists before `cargo build` embeds it
        // (tech-spec 09 §A.4, tech-spec 15 §15.5: build the web client before the native build).
        "ci" => {
            build_web()
                && check_deps()
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
        // Package the desktop app (deb/AppImage on Linux) via the Tauri bundler.
        "bundle" => bundle(),
        // Render the shell completions + man pages the .deb and the release archives install.
        // `--target <triple>` mirrors the release workflow, which cross-builds per matrix leg;
        // it is passed through to the `cargo run` that renders them, so the files come from the
        // binary that leg built.
        "packaging" => stage_packaging(flag_value("--target").as_deref()),
        "check-deps" => check_deps(),
        other => {
            eprintln!(
                "unknown xtask '{other}'. try: ci | web | wasm | bundle | packaging | check-deps"
            );
            false
        }
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Package the desktop app into OS bundles (tech-spec 15 §15.5). The layout is unusual and the
/// order matters: `crates/3dam-desktop` holds `tauri.conf.json` but is a *lib* crate — the product
/// binary is the `3dam` bin of the `dam` package (`crates/3dam`, ADR 0010). So we build the web
/// client (embedded via `rust-embed`), then the real `3dam` release binary, then run `cargo tauri
/// bundle`, which packages the *already-built* `target/release/3dam` — `mainBinaryName: "3dam"` in
/// `tauri.conf.json` points the bundler at it, and no second (Tauri-driven) cargo build happens.
/// Skips gracefully with a hint if `tauri-cli` is absent, mirroring the `pnpm`/`wasm-pack` handling.
fn bundle() -> bool {
    // Check the bundler first — without it the web/binary builds below would be minutes of work
    // just to announce a skip.
    if which_cargo_subcommand("tauri").is_none() {
        eprintln!(
            "xtask bundle: `tauri-cli` not found — skipping desktop packaging (install with \
             `cargo install tauri-cli --version '^2'` to bundle the desktop app)."
        );
        return true;
    }
    if !build_web() {
        return false;
    }
    let desktop = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/3dam-desktop");
    run("cargo", &["build", "-p", "dam", "--release", "--locked"])
        // `tauri.conf.json` lists the staged files under `bundle.linux.deb.files`, and the bundler
        // treats a missing source as a hard error ("… does not exist"), so this is a build
        // prerequisite of the .deb — not an optional extra. Always re-staged rather than only when
        // absent, so a renamed subcommand cannot leave a stale page behind.
        && stage_packaging(None)
        && run_in(&desktop, "cargo", &["tauri", "bundle"])
}

/// The value following `flag` in argv, e.g. `--target x86_64-unknown-linux-gnu`.
fn flag_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Staging directory for generated packaging assets, relative to the workspace root.
///
/// Deliberately **not** under `target/`, which would otherwise be the obvious home. The path has to
/// appear as a literal string in `crates/3dam-desktop/tauri.conf.json` (`bundle.linux.deb.files`),
/// and cargo's target directory is not a fixed location: `[build] target-dir` in `~/.cargo/config
/// .toml` or `CARGO_TARGET_DIR` relocates it wholesale, which is a normal thing for a developer to
/// set and is set on at least one machine here. Gitignored instead.
const PACKAGING_DIR: &str = "packaging";

/// The four shells ADR 0009 §10 commits to shipping, with the filename `clap_complete` gives each.
/// The names are conventions the completion loaders rely on, so they are asserted rather than
/// globbed — a rename upstream would otherwise quietly ship a .deb whose completions never load.
const COMPLETIONS: &[(&str, &str)] = &[
    ("bash", "3dam.bash"),
    ("zsh", "_3dam"),
    ("fish", "3dam.fish"),
    ("powershell", "_3dam.ps1"),
];

/// Render the shell completions and man pages into `packaging/` (ADR 0009 §10, tech-spec 15 §15.5).
///
/// The binary generates these itself (`3dam completions <shell>` / `3dam man`) so the clap tree in
/// `dam-cli` stays the only copy of the grammar; this task is just the staging layout that the
/// Tauri deb config and the release archives both point at.
///
/// Invoked through `cargo run` rather than by executing a path we construct. Cargo knows where its
/// own output lives; we do not — `[build] target-dir` and `CARGO_TARGET_DIR` both relocate it, and
/// `--target <triple>` moves it again. Against an already-built binary this is a cache hit, so the
/// cost is cargo's own no-op check.
///
/// The output lands in `packaging/` regardless of `target`, because `tauri.conf.json` has to name
/// those paths as a literal string. That is correct rather than merely convenient: staging renders
/// the *command tree*, which is identical across triples. `target` matters only so cargo runs the
/// binary this leg actually built. Cross-running a triple the host cannot execute is out of scope —
/// the release matrix runs each leg on its own OS.
fn stage_packaging(target: Option<&str>) -> bool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = root.join(PACKAGING_DIR);
    // Start clean: a stale page for a subcommand that has since been renamed would otherwise be
    // installed forever, since nothing else ever removes files from this directory.
    let _ = std::fs::remove_dir_all(&out);
    let (completions, man) = (out.join("completions"), out.join("man"));

    // `--` separates cargo's own flags from the binary's argv.
    let dam = |args: &[&str]| {
        let mut argv = vec!["run", "-p", "dam", "--release", "--locked", "--quiet"];
        if let Some(t) = target {
            argv.extend(["--target", t]);
        }
        argv.push("--");
        argv.extend(args);
        run("cargo", &argv)
    };

    for (shell, _) in COMPLETIONS {
        if !dam(&[
            "completions",
            shell,
            "--out",
            &completions.to_string_lossy(),
        ]) {
            return false;
        }
    }
    if !dam(&["man", "--out", &man.to_string_lossy()]) {
        return false;
    }
    for (shell, file) in COMPLETIONS {
        if !completions.join(file).exists() {
            eprintln!("xtask packaging: {shell} completions did not produce {file}");
            return false;
        }
    }
    gzip_man_pages(&man)
}

/// Compress the generated man pages in place (`3dam.1` → `3dam.1.gz`).
///
/// Debian policy §12.1 requires installed manual pages be compressed, and the Tauri bundler does
/// no compression of its own — it only gzips the changelog it generates. Shelling out to `gzip`
/// keeps xtask dependency-free; `-n` omits the timestamp so the output is reproducible.
fn gzip_man_pages(man: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(man) else {
        eprintln!(
            "xtask packaging: no man pages were generated in {}",
            man.display()
        );
        return false;
    };
    let pages: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "1"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if pages.is_empty() {
        eprintln!("xtask packaging: no *.1 man pages in {}", man.display());
        return false;
    }
    if which("gzip").is_none() {
        // Only the .deb needs the compressed form, and the deb bundler only runs on Linux, so a
        // gzip-less Windows/macOS box can still bundle. Say so rather than failing silently.
        eprintln!(
            "xtask packaging: `gzip` not found — leaving {} man pages uncompressed (fine unless \
             you are building the .deb, whose tauri.conf.json expects *.1.gz).",
            pages.len()
        );
        return true;
    }
    let mut args = vec!["-9", "-n"];
    args.extend(pages.iter().map(|s| s.as_str()));
    run("gzip", &args)
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    dependencies: Vec<CargoDependency>,
}

#[derive(Deserialize)]
struct CargoDependency {
    name: String,
}

/// Direct internal dependency edges allowed by the current architecture (tech-spec 01 §2 and
/// `CLAUDE.md`'s crate map). External crates are deliberately outside this graph-shape check.
const ALLOWED_DAM_EDGES: &[(&str, &str)] = &[
    ("dam", "dam-cli"),
    ("dam", "dam-desktop"),
    ("dam", "dam-frontend"),
    ("dam-cli", "dam-api"),
    ("dam-cli", "dam-client"),
    ("dam-cli", "dam-core"),
    ("dam-cli", "dam-frontend"),
    ("dam-cli", "dam-media"),
    ("dam-cli", "dam-server"),
    ("dam-client", "dam-api"),
    ("dam-core", "dam-api"),
    ("dam-core", "dam-client"),
    ("dam-core", "dam-media"),
    ("dam-core", "dam-render"),
    ("dam-core", "dam-sources"),
    ("dam-core", "dam-store"),
    ("dam-desktop", "dam-frontend"),
    ("dam-desktop", "dam-server"),
    ("dam-frontend", "dam-api"),
    ("dam-frontend", "dam-client"),
    ("dam-frontend", "dam-core"),
    ("dam-media", "dam-api"),
    ("dam-server", "dam-api"),
    ("dam-server", "dam-core"),
    ("dam-sources", "dam-api"),
    ("dam-store", "dam-api"),
    ("dam-store", "dam-sources"),
];

fn product_package(name: &str) -> bool {
    name == "dam" || name.starts_with("dam-")
}

fn unexpected_edges(metadata: &CargoMetadata) -> Vec<(String, String)> {
    let packages: HashSet<&str> = metadata
        .packages
        .iter()
        .map(|package| package.name.as_str())
        .filter(|name| product_package(name))
        .collect();
    let allowed: HashSet<(&str, &str)> = ALLOWED_DAM_EDGES.iter().copied().collect();
    let mut unexpected = metadata
        .packages
        .iter()
        .filter(|package| packages.contains(package.name.as_str()))
        .flat_map(|package| {
            let packages = &packages;
            let allowed = &allowed;
            package.dependencies.iter().filter_map(move |dependency| {
                let edge = (package.name.as_str(), dependency.name.as_str());
                (packages.contains(dependency.name.as_str()) && !allowed.contains(&edge))
                    .then(|| (edge.0.to_owned(), edge.1.to_owned()))
            })
        })
        .collect::<Vec<_>>();
    unexpected.sort();
    unexpected.dedup();
    unexpected
}

/// Assert the allowed internal-edge whitelist over `cargo metadata`. Cargo itself rejects cycles;
/// this guard catches a new upward or cross-layer edge before it becomes accepted architecture.
fn check_deps() -> bool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    eprintln!("$ cargo metadata --no-deps --format-version 1");
    let output = match Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!("check-deps: cargo metadata failed with {}", output.status);
            return false;
        }
        Err(error) => {
            eprintln!("check-deps: could not run cargo metadata: {error}");
            return false;
        }
    };
    let metadata: CargoMetadata = match serde_json::from_slice(&output.stdout) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("check-deps: invalid cargo metadata: {error}");
            return false;
        }
    };
    let unexpected = unexpected_edges(&metadata);
    if unexpected.is_empty() {
        eprintln!("check-deps: dependency directions are valid");
        return true;
    }
    eprintln!("check-deps: unexpected internal dependency edges:");
    for (from, to) in unexpected {
        eprintln!("  {from} -> {to}");
    }
    false
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

/// Like [`which`], but for cargo subcommands (`cargo-tauri` etc.), which only answer `--version`
/// when dispatched through `cargo`.
fn which_cargo_subcommand(sub: &str) -> Option<()> {
    Command::new("cargo")
        .args([sub, "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| ())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, dependencies: &[&str]) -> CargoPackage {
        CargoPackage {
            name: name.to_owned(),
            dependencies: dependencies
                .iter()
                .map(|name| CargoDependency {
                    name: (*name).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn dependency_guard_accepts_known_edges_and_ignores_external_crates() {
        let metadata = CargoMetadata {
            packages: vec![
                package("dam-core", &["dam-api", "serde"]),
                package("dam-api", &["serde"]),
            ],
        };
        assert!(unexpected_edges(&metadata).is_empty());
    }

    #[test]
    fn dependency_guard_reports_new_internal_edges() {
        let metadata = CargoMetadata {
            packages: vec![
                package("dam-api", &["dam-server"]),
                package("dam-server", &["dam-api"]),
            ],
        };
        assert_eq!(
            unexpected_edges(&metadata),
            vec![("dam-api".to_owned(), "dam-server".to_owned())]
        );
    }
}
