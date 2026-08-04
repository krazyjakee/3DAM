//! `cargo xtask` — dev automation, including the dependency-graph guard from tech-spec 01 §2.

use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, ExitCode};

mod perf;

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_default();
    let ok = match task.as_str() {
        // `web` builds the React client first so its dist/ exists before `cargo build` embeds it
        // (tech-spec 09 §A.4, tech-spec 15 §15.5: build the web client before the native build).
        "ci" => {
            build_web()
                && check_deps()
                && run("cargo", &["fmt", "--all", "--check"])
                && feature_matrix(None)
                && run("cargo", &["test", "--workspace"])
                && feature_tests()
        }
        // Strict Clippy coverage for every supported Cargo/target profile. Passing one of the
        // documented profile names runs only that group; no name runs the complete matrix.
        "feature-matrix" => feature_matrix(std::env::args().nth(2).as_deref()),
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
        "perf" => perf::run(std::env::args().skip(2).collect()),
        other => {
            eprintln!(
                "unknown xtask '{other}'. try: ci | feature-matrix | web | wasm | bundle | \
                 packaging | check-deps | perf"
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

struct FeatureProfile {
    name: &'static str,
    commands: &'static [&'static [&'static str]],
}

/// Non-default features whose behaviour is *executed*, not merely linted.
///
/// `feature_matrix` is Clippy-only and `cargo test --workspace` builds default features, so between
/// them nothing ever ran an off-by-default feature's tests. `dam-store`'s `ann` feature swaps the
/// exhaustive cosine scan for an HNSW index in `similar` / `nearest_in_space` (`analysis.rs:1081`,
/// `:1220`, `:1257`, `:1285`) — a different algorithm behind the same API, which is exactly the kind
/// of substitution a type check cannot vouch for. Its own tests in `src/ann.rs` were dead code here.
///
/// Deliberately only `ann`: it pulls one pure-Rust crate (`instant-distance`). The other
/// off-by-default features need external toolchains (Assimp for `render`/`model-convert`, live
/// servers for `sftp`/`smb`), so running their tests would make this gate depend on the host.
const FEATURE_TESTS: &[&[&str]] = &[&["test", "-p", "dam-store", "--features", "ann", "--locked"]];

fn feature_tests() -> bool {
    FEATURE_TESTS.iter().all(|args| run("cargo", args))
}

/// The supported compile profiles, kept here so hosted CI and the local pre-push gate execute the
/// exact same commands. Individual features are isolated with `--no-default-features`; the final
/// all-feature workspace pass covers the additive combination because none of the owning crates has
/// a cfg expression that depends on a particular pair of features.
const FEATURE_PROFILES: &[FeatureProfile] = &[
    FeatureProfile {
        name: "sources",
        commands: &[
            &[
                "clippy",
                "-p",
                "dam-sources",
                "--no-default-features",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[
                "clippy",
                "-p",
                "dam-sources",
                "--no-default-features",
                "--features",
                "sftp",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[
                "clippy",
                "-p",
                "dam-sources",
                "--no-default-features",
                "--features",
                "smb",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        ],
    },
    FeatureProfile {
        name: "media",
        commands: &[
            &[
                "clippy",
                "-p",
                "dam-media",
                "--no-default-features",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[
                "clippy",
                "-p",
                "dam-media",
                "--no-default-features",
                "--features",
                "model-convert",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        ],
    },
    FeatureProfile {
        name: "core",
        commands: &[
            &[
                "clippy",
                "-p",
                "dam-core",
                "--no-default-features",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[
                "clippy",
                "-p",
                "dam-core",
                "--no-default-features",
                "--features",
                "render",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[
                "clippy",
                "-p",
                "dam-core",
                "--no-default-features",
                "--features",
                "model-convert",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[
                "clippy",
                "-p",
                "dam-core",
                "--no-default-features",
                "--features",
                "semantic",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        ],
    },
    FeatureProfile {
        name: "viewer",
        commands: &[
            &[
                "clippy",
                "-p",
                "dam-viewer",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[
                "clippy",
                "-p",
                "dam-viewer",
                "--target",
                "wasm32-unknown-unknown",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        ],
    },
    FeatureProfile {
        name: "binary",
        commands: &[&[
            "clippy",
            "-p",
            "dam",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ]],
    },
    FeatureProfile {
        name: "workspace-all",
        commands: &[&[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--locked",
            "--",
            "-D",
            "warnings",
        ]],
    },
];

fn feature_matrix(requested: Option<&str>) -> bool {
    if let Some(name) = requested {
        if !FEATURE_PROFILES.iter().any(|profile| profile.name == name) {
            eprintln!(
                "unknown feature profile {name:?}; expected one of: {}",
                FEATURE_PROFILES
                    .iter()
                    .map(|profile| profile.name)
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            return false;
        }
    }

    FEATURE_PROFILES
        .iter()
        .filter(|profile| requested.is_none_or(|name| name == profile.name))
        .all(|profile| {
            eprintln!("feature-matrix: {}", profile.name);
            prepare_feature_profile(profile.name)
                && profile.commands.iter().all(|args| run("cargo", args))
        })
}

/// `rust-embed` accepts an empty web bundle (the server then serves its documented build hint) but
/// its derive macro still requires the ignored directory to exist. A clean checkout has no empty
/// directories, so lint-only binary/workspace profiles create the directory without manufacturing
/// an artifact. The web/release gates separately require the real `index.html` and assets.
fn prepare_feature_profile(name: &str) -> bool {
    if !matches!(name, "binary" | "workspace-all") {
        return true;
    }
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../web/dist");
    std::fs::create_dir_all(&dist)
        .map_err(|error| {
            eprintln!(
                "feature-matrix: cannot create rust-embed prerequisite {}: {error}",
                dist.display()
            );
        })
        .is_ok()
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
    optional: bool,
    /// `None` is a normal dependency; `dev` dependencies do not belong to the shipped crate graph.
    kind: Option<String>,
}

/// Direct internal dependency edges allowed by the current architecture (tech-spec 01 §2 and
/// `CLAUDE.md`'s crate map). The bool records Cargo's `optional` status; external crates are
/// deliberately outside this graph-shape check. When this changes, follow tech-spec 01's graph
/// review and update its graph, affected ADRs, CLAUDE.md, and README.md in the same change.
const ALLOWED_DAM_EDGES: &[(&str, &str, bool)] = &[
    ("dam", "dam-cli", false),
    ("dam", "dam-desktop", false),
    ("dam", "dam-frontend", false),
    ("dam-cli", "dam-api", false),
    ("dam-cli", "dam-client", false),
    ("dam-cli", "dam-core", false),
    ("dam-cli", "dam-frontend", false),
    ("dam-cli", "dam-media", false),
    ("dam-cli", "dam-server", false),
    ("dam-client", "dam-api", false),
    ("dam-core", "dam-api", false),
    ("dam-core", "dam-client", false),
    ("dam-core", "dam-media", false),
    ("dam-core", "dam-render", true),
    ("dam-core", "dam-sources", false),
    ("dam-core", "dam-store", false),
    ("dam-desktop", "dam-frontend", false),
    ("dam-desktop", "dam-server", false),
    ("dam-frontend", "dam-api", false),
    ("dam-frontend", "dam-client", false),
    ("dam-frontend", "dam-core", false),
    ("dam-media", "dam-api", false),
    ("dam-server", "dam-api", false),
    ("dam-server", "dam-core", false),
    ("dam-sources", "dam-api", false),
    ("dam-store", "dam-api", false),
    ("dam-store", "dam-sources", false),
];

fn product_package(name: &str) -> bool {
    name == "dam" || name.starts_with("dam-")
}

type OwnedDependencyEdge = (String, String, bool);
type DependencyDrift = (Vec<OwnedDependencyEdge>, Vec<OwnedDependencyEdge>);

fn dependency_drift(metadata: &CargoMetadata) -> DependencyDrift {
    let packages: HashSet<&str> = metadata
        .packages
        .iter()
        .map(|package| package.name.as_str())
        .filter(|name| product_package(name))
        .collect();
    let allowed: HashSet<(&str, &str, bool)> = ALLOWED_DAM_EDGES.iter().copied().collect();
    let actual: HashSet<(&str, &str, bool)> = metadata
        .packages
        .iter()
        .filter(|package| packages.contains(package.name.as_str()))
        .flat_map(|package| {
            let packages = &packages;
            package.dependencies.iter().filter_map(move |dependency| {
                (dependency.kind.as_deref() != Some("dev")
                    && packages.contains(dependency.name.as_str()))
                .then_some((
                    package.name.as_str(),
                    dependency.name.as_str(),
                    dependency.optional,
                ))
            })
        })
        .collect();
    let mut unexpected = actual
        .difference(&allowed)
        .map(|(from, to, optional)| ((*from).to_owned(), (*to).to_owned(), *optional))
        .collect::<Vec<_>>();
    let mut missing = allowed
        .difference(&actual)
        .filter(|edge| {
            let (from, to, _) = **edge;
            packages.contains(from) && packages.contains(to)
        })
        .map(|(from, to, optional)| ((*from).to_owned(), (*to).to_owned(), *optional))
        .collect::<Vec<_>>();
    unexpected.sort();
    missing.sort();
    (unexpected, missing)
}

/// Assert the exact internal-edge whitelist over `cargo metadata`. Cargo itself rejects cycles;
/// this guard catches new edges, removed edges, and optional-status changes before the manifest and
/// architecture guidance can silently diverge.
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
    let (unexpected, missing) = dependency_drift(&metadata);
    if unexpected.is_empty() && missing.is_empty() {
        eprintln!("check-deps: dependency directions are valid");
        return true;
    }
    if !unexpected.is_empty() {
        eprintln!("check-deps: unexpected internal dependency edges:");
        for (from, to, optional) in unexpected {
            eprintln!("  {from} -> {to} (optional: {optional})");
        }
    }
    if !missing.is_empty() {
        eprintln!("check-deps: stale or optional-status-mismatched whitelist edges:");
        for (from, to, optional) in missing {
            eprintln!("  {from} -> {to} (optional: {optional})");
        }
    }
    eprintln!("check-deps: follow the architecture review in tech-spec 01 §2");
    false
}

/// Build the web client into `web/dist/` (consumed by the server's `rust-embed`, tech-spec 09 §A.4).
/// Skips gracefully with a hint if `pnpm` is absent — the native build still works against whatever
/// `web/dist/` already exists (the server serves a build hint when it is empty). `wasm-pack` is not
/// optional when pnpm is present: Vite resolves the generated module while bundling, so fail before
/// starting the web build when that prerequisite is missing.
fn build_web() -> bool {
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("../web");
    if which("pnpm").is_none() {
        eprintln!(
            "xtask web: `pnpm` not found — skipping web build (install Node/pnpm to build the UI)."
        );
        return true;
    }
    // The WASM viewer islands (tech-spec 09 §B.3) are built first so their pkg exists under
    // web/src/wasm/ before Vite bundles them into dist/ — one `rust-embed` step then ships both
    // the React bundle and the `.wasm` in the single binary (§A.4).
    if !build_wasm() {
        return false;
    }
    run_in(&web, "pnpm", &["install", "--frozen-lockfile"])
        && run_in(&web, "pnpm", &["test"])
        && run_in(&web, "pnpm", &["build"])
        && require_artifacts("xtask web", &web.join("dist"), WEB_ARTIFACTS)
}

/// Build the `dam-viewer` WASM model viewer (tech-spec 09 §B.3 / ADR 0009 §9) with
/// `wasm-pack` into `web/src/wasm/` (a gitignored build artifact the web client lazily imports).
/// The generated module is a required Vite input, so unlike an entirely skipped web build this task
/// must fail when `wasm-pack` is absent. Otherwise a clean checkout reaches Vite before reporting a
/// misleading missing-module error.
fn build_wasm() -> bool {
    const WASM_PACK_VERSION: &str = "0.13.1";
    let installed_version = Command::new("wasm-pack")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok());
    let Some(installed_version) = installed_version else {
        eprintln!(
            "xtask wasm: required tool `wasm-pack` was not found; install it with \
             `cargo install wasm-pack --version {WASM_PACK_VERSION} --locked`, then retry \
             `cargo xtask wasm` or `cargo xtask web`."
        );
        return false;
    };
    let expected_version = format!("wasm-pack {WASM_PACK_VERSION}");
    if installed_version.trim() != expected_version {
        eprintln!(
            "xtask wasm: deterministic release artifacts require {expected_version}, but found \
             `{}`; install the pinned tool with `cargo install wasm-pack --version \
             {WASM_PACK_VERSION} --locked --force`.",
            installed_version.trim()
        );
        return false;
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
    ) && require_artifacts("xtask wasm", &out_dir, WASM_ARTIFACTS)
}

#[derive(Clone, Copy)]
enum ArtifactKind {
    File,
    Directory,
}

const WASM_ARTIFACTS: &[(&str, ArtifactKind)] = &[
    ("dam_viewer.js", ArtifactKind::File),
    ("dam_viewer_bg.wasm", ArtifactKind::File),
];

const WEB_ARTIFACTS: &[(&str, ArtifactKind)] = &[
    ("index.html", ArtifactKind::File),
    ("assets", ArtifactKind::Directory),
];

/// Verify the build outputs consumed by the next stage. Keeping these checks beside the local
/// builders means release builds cannot accidentally enforce a different artifact contract.
fn require_artifacts(task: &str, base: &Path, artifacts: &[(&str, ArtifactKind)]) -> bool {
    let missing = artifacts
        .iter()
        .filter_map(|(relative, kind)| {
            let path = base.join(relative);
            let exists = match kind {
                ArtifactKind::File => path.is_file(),
                ArtifactKind::Directory => path.is_dir(),
            };
            (!exists).then(|| path.display().to_string())
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return true;
    }
    eprintln!("{task}: build completed without required artifact(s):");
    for path in missing {
        eprintln!("  {path}");
    }
    false
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "3dam-xtask-{name}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn package(name: &str, dependencies: &[&str]) -> CargoPackage {
        CargoPackage {
            name: name.to_owned(),
            dependencies: dependencies
                .iter()
                .map(|name| CargoDependency {
                    name: (*name).to_owned(),
                    optional: false,
                    kind: None,
                })
                .collect(),
        }
    }

    #[test]
    fn feature_profiles_are_strict_and_the_binary_matches_release() {
        assert_eq!(
            FEATURE_PROFILES
                .iter()
                .map(|profile| profile.name)
                .collect::<Vec<_>>(),
            [
                "sources",
                "media",
                "core",
                "viewer",
                "binary",
                "workspace-all"
            ]
        );
        for command in FEATURE_PROFILES.iter().flat_map(|profile| profile.commands) {
            assert_eq!(command.first(), Some(&"clippy"));
            assert!(command.contains(&"--all-targets"));
            assert!(command.contains(&"--locked"));
            assert!(command.ends_with(&["--", "-D", "warnings"]));
        }

        // Release builds `cargo build -p dam --release --locked` with no feature selection. The
        // lint profile must keep that exact package/feature surface (Clippy adds only target/lint
        // flags), rather than accidentally using workspace feature unification or all-features.
        let binary = FEATURE_PROFILES
            .iter()
            .find(|profile| profile.name == "binary")
            .unwrap();
        assert_eq!(
            binary.commands,
            &[&[
                "clippy",
                "-p",
                "dam",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings"
            ][..]]
        );
    }

    #[test]
    fn dependency_guard_accepts_known_edges_and_ignores_external_crates() {
        let metadata = CargoMetadata {
            packages: vec![
                package("dam-core", &["dam-api", "serde"]),
                package("dam-api", &["serde"]),
            ],
        };
        let (unexpected, missing) = dependency_drift(&metadata);
        assert!(unexpected.is_empty());
        assert!(missing.is_empty());
    }

    #[test]
    fn dependency_guard_reports_new_internal_edges() {
        let metadata = CargoMetadata {
            packages: vec![
                package("dam-api", &["dam-server"]),
                package("dam-server", &["dam-api"]),
            ],
        };
        let (unexpected, missing) = dependency_drift(&metadata);
        assert_eq!(
            unexpected,
            vec![("dam-api".to_owned(), "dam-server".to_owned(), false)]
        );
        assert!(missing.is_empty());
    }

    #[test]
    fn dependency_guard_ignores_test_only_internal_edges() {
        let mut server = package("dam-server", &["dam-api", "dam-client"]);
        server.dependencies[1].kind = Some("dev".into());
        let metadata = CargoMetadata {
            packages: vec![
                server,
                package("dam-api", &[]),
                package("dam-client", &["dam-api"]),
            ],
        };
        let (unexpected, missing) = dependency_drift(&metadata);
        assert!(unexpected.is_empty());
        assert!(missing.is_empty());
    }

    #[test]
    fn dependency_guard_reports_optional_status_drift() {
        let mut core = package("dam-core", &["dam-render"]);
        core.dependencies[0].optional = false;
        let metadata = CargoMetadata {
            packages: vec![core, package("dam-render", &[])],
        };
        let (unexpected, missing) = dependency_drift(&metadata);
        assert_eq!(
            unexpected,
            vec![("dam-core".to_owned(), "dam-render".to_owned(), false)]
        );
        assert_eq!(
            missing,
            vec![("dam-core".to_owned(), "dam-render".to_owned(), true)]
        );
    }

    #[test]
    fn dependency_guard_reports_stale_whitelist_edges() {
        let metadata = CargoMetadata {
            packages: vec![package("dam-client", &[]), package("dam-api", &[])],
        };
        let (unexpected, missing) = dependency_drift(&metadata);
        assert!(unexpected.is_empty());
        assert_eq!(
            missing,
            vec![("dam-client".to_owned(), "dam-api".to_owned(), false)]
        );
    }

    #[test]
    fn artifact_guard_requires_each_file_and_directory() {
        let root = temp_dir("artifacts");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("index.html"), "<!doctype html>").unwrap();
        assert!(require_artifacts("test", &root, WEB_ARTIFACTS));

        std::fs::remove_file(root.join("index.html")).unwrap();
        assert!(!require_artifacts("test", &root, WEB_ARTIFACTS));

        std::fs::remove_dir_all(root).unwrap();
    }
}
