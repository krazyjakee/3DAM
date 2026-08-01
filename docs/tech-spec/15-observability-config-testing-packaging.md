# 15 — Observability, Config, Testing & Packaging

Status: **Draft v0.1** · Scope: the cross-cutting engineering hygiene — local logging/tracing, the error-handling taxonomy, config precedence, the testing/benchmark strategy, and packaging/release mechanics.

This file owns the **engineering-hygiene seams** that every area depends on but none owns
alone. It sits beneath the whole spec: how a running 3DAM (GUI, CLI, or `serve`) tells you
what it is doing (**locally**, never over the wire), how errors flow from the typed model into
logs and user-facing messages, how configuration is resolved across the three roles, how the
codebase is tested at scale, and how a tagged commit becomes signed-or-unsigned installers on
GitHub Releases.

Read this alongside its siblings, whose borders it respects:

- **[14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md)** owns
  the tokio/rayon execution model and the **performance *targets***. This file owns the
  **testing/benchmark *tooling*** that measures against those targets and guards regressions.
- **[10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md)** owns the **audit log**
  (who changed which flag/account, when — a security record). This file owns **general
  application logging/tracing** (what the engine is doing) — a different stream with a
  different purpose.
- **[03-library-service-and-api.md](03-library-service-and-api.md)** owns the **error
  *model/types*** (the `enum`s, the DTO error shapes on the wire). This file owns the
  **taxonomy *conventions*** — which crate uses which error style, how errors are logged, and
  how they degrade to a user.
- **[09-server-and-web-client.md](09-server-and-web-client.md)** owns the **serve config
  file** contents/schema. This file places that file within the **general config-precedence
  scheme** shared by all three roles.

Product grounding: local-first, no telemetry, no unsolicited network calls
([PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §8, [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md)
§1.5); graceful degradation and tested-at-scale (DESIGN_GUIDELINES §6); documented DB +
plain-text export, reproducible cross-platform binaries from a tag-triggered CI matrix
(PRODUCT_SPEC §8; the release plan is §15.5 below).

---

## 15.1 Observability — local logging & tracing

Observability in 3DAM means **local diagnostics only**. There is no telemetry, no crash
reporter, no metrics endpoint that phones home, and no unsolicited network call of any kind.
Everything below writes to the user's own disk or terminal and nowhere else.

### 15.1.1 Crate choices

- **`tracing`** is the single instrumentation facade across every crate (`3dam-core`,
  handlers, sources, render, server, CLI, GUI shell). Libraries emit `tracing` events and
  spans; they never configure a subscriber.
- **`tracing-subscriber`** installs the subscriber, but **only at the binary edge** (the
  `3dam` entry point, once, before anything else runs). Exactly one of GUI / CLI / `serve`
  main initialises it depending on the dispatched role (PRODUCT_SPEC §4.1).
- **`tracing-appender`** for non-blocking rolling file output in `serve` mode.
- No `log` crate direct use; dependencies that emit via `log` are bridged with
  `tracing-log` so their output joins the same stream.

### 15.1.2 Levels and what belongs where

| Level | Use for |
|-------|---------|
| `error` | An operation failed in a way the user should know about (a scan aborted, a source unreachable after retries). Always paired with a fail-soft outcome — see §15.2.4. |
| `warn` | Degraded but continued — one bad asset skipped, a missing optional dependency, a software-raster fallback engaged (PRODUCT_SPEC §6.8). |
| `info` | Lifecycle milestones — `serve` bound to an address, a scan started/finished with counts, N assets analysed. The default filter for `serve`. |
| `debug` | Per-batch / per-job detail — worker pool sizing, cache hits, query plans. |
| `trace` | Per-asset / per-frame firehose — individual handler calls, span open/close. Off unless explicitly requested. |

The default level is `info` for `serve`, `warn` for GUI/CLI (so scripts stay quiet — the CLI
already detects non-TTY and quiets down, DESIGN_GUIDELINES §5). All are overridable via
`RUST_LOG` / `--log-level` (§15.3).

### 15.1.3 Structured spans across the async/rayon pipeline

The pipeline crosses the tokio (I/O) ↔ rayon (CPU) boundary described in
[14](14-concurrency-performance-reliability.md); spans must survive that crossing so a log
line can be traced back to the asset and job that produced it.

- **Span hierarchy** mirrors the work: `scan{source_id}` → `ingest{asset_id}` →
  `extract{media, tier}` → `analyze{extractor, version}`. Each carries structured fields
  (`asset_id`, `source_id`, `job_id`, `extractor_version`) rather than interpolated strings,
  so logs are greppable and machine-filterable.
- **Crossing the rayon boundary:** a `tracing::Span` is captured before dispatching CPU work
  onto rayon and re-entered inside the worker closure (`let _g = span.enter();`), because
  rayon threads do not inherit the tokio task's span context. This is a convention every
  handler and analysis stage follows.
- **Job correlation:** every long-running job (scan, analyse, convert) gets a `job_id` set as
  a span field at creation; that id is the same one surfaced over the WS progress channel
  ([03](03-library-service-and-api.md)) and in CLI progress output, so a user-visible job and
  its log lines share one key.
- **`extractor_version`** is always logged on analysis spans, making the reproducibility bar
  (DESIGN_GUIDELINES §6) auditable from logs — you can see which extractor version produced a
  result and thus which re-analysis a model bump requires ([05](05-analysis-similarity-dedup.md)).

### 15.1.4 Where logs go

- **GUI / CLI (embedded):** human-formatted logs to **stderr** by default (never stdout —
  stdout is reserved for `--json`/`--csv` machine output, DESIGN_GUIDELINES §5). An optional
  rolling file under the OS state/cache dir (§15.3) when `--log-file` is set.
- **`serve`:** **structured** output — human-formatted to stderr for interactive launch, plus
  a rolling file (`tracing-appender`) under the state dir, with an opt-in **JSON lines**
  formatter (`--log-format json`) for operators piping into their own log aggregation. That
  aggregation, if any, is the operator's own infrastructure — 3DAM writes the file; it does
  not ship the logs anywhere.
- **Format:** never emit ANSI colour when the sink is not a TTY (auto-detected).

### 15.1.5 The no-telemetry / no-network invariant (enforced)

No-telemetry is a **hard invariant, not a default** (PRODUCT_SPEC §8, DESIGN_GUIDELINES §1.5).
It is enforced structurally rather than trusted:

- **No analytics/telemetry/crash-reporting dependency** may enter the tree. A CI
  **`cargo-deny`** advisories/bans list denies known telemetry SDKs; adding one fails the
  build. (This is the same `cargo-deny` invocation used for the dependency-direction guard,
  §15.4.4.)
- **All network egress is source-initiated.** The only crates permitted to open outbound
  connections are the source layer (SFTP/SMB/federated peer, [07](07-sources-and-federation.md))
  and the auth layer's configured OIDC/OAuth2 endpoints ([10](10-auth-accounts-and-flags.md)) —
  every one of them acting on an address the **user** supplied (a source they added, an IdP
  they configured). `3dam-core` and the handlers make no outbound calls.
- **No hidden update check, no "check for new version" ping.** Update discovery, if ever
  added, would be an explicit user action, tracked as an open question — not a background
  call.
- The logging subsystem itself has **no remote sink**; there is deliberately no
  `tracing` layer that ships events off-box.

---

## 15.2 Error handling — taxonomy & conventions

The typed error **model** (the concrete `enum`s and their variants, the wire DTO shapes) is
owned by [03-library-service-and-api.md](03-library-service-and-api.md). This section defines
the **conventions** that govern how errors are constructed, propagated, logged, and shown —
so every crate handles failure the same way.

### 15.2.1 Two-tier convention: `thiserror` in libraries, `anyhow` at edges

| Layer | Crate style | Rule |
|-------|-------------|------|
| **Libraries** (`3dam-core`, handlers, sources, render) | **`thiserror`** typed enums | Every fallible library API returns a specific, matchable error type. No `anyhow` in library public signatures — callers must be able to `match` on the failure to decide fail-soft vs abort. |
| **Binary edges** (CLI commands, `serve` request handlers, GUI action handlers, the `3dam mcp` shim) | **`anyhow`** (with context) | At the top of a call stack, where the only remaining job is to *report* the error, wrap with `anyhow`/`.context(...)` and format for the user. `eyre` is acceptable if a richer report is wanted, but the codebase standardises on **`anyhow`** for consistency. |

The seam is the `LibraryService` boundary: its methods return the typed `03` error model;
front-ends translate that into `anyhow`-level reporting or the on-wire DTO error.

### 15.2.2 Error taxonomy conventions

The `03` model groups failures into stable categories so both wire mapping and logging are
consistent. The **conventions** (categories and their intended semantics; `03` owns the exact
variants):

- **NotFound** — the asset/source/collection id does not exist. → HTTP 404, CLI exit code
  distinct from a usage error, `warn`-level at most.
- **Invalid / BadRequest** — malformed query, bad argument, unsupported format request. →
  HTTP 400, CLI usage exit code, `warn`.
- **Unsupported / Degraded** — a fail-soft outcome: unreadable format, missing optional
  dependency, GPU-less render fallback. Not an error to the caller of the *library* — it is a
  per-asset skip, logged `warn`, surfaced as a status on the asset, never crashing the scan
  (DESIGN_GUIDELINES §2 "fail soft").
- **Source / Io** — a source went offline, a read failed, a network share dropped. Retried
  per source policy ([07](07-sources-and-federation.md)); if terminal, the source is marked
  offline and cached results stay usable. Logged `error` if it aborts an operation, `warn` if
  degraded.
- **Auth / Forbidden** — from the auth layer ([10](10-auth-accounts-and-flags.md)); → HTTP
  401/403, never leaks which resource exists to an under-scoped caller.
- **Internal** — a genuine bug/invariant violation. → HTTP 500, logged `error` **with full
  context and the span chain**, and (for a bug) worth a backtrace. This is the only category
  that should be rare.

Each category maps once to (HTTP status, CLI exit code) in `03`/[13](13-cli.md); this file
just fixes the naming and log-level conventions so the mapping is uniform.

### 15.2.3 How errors are logged

- **Log at the boundary, once.** An error is logged where it is handled (the edge), not at
  every `?` on the way up — no duplicate lines for one failure. Library code attaches
  **context** (`.context("decoding thumbnail for {asset_id}")`) as the error propagates; the
  edge logs the fully-contextualised chain.
- **Errors inherit the active span**, so `asset_id`/`job_id`/`source_id` are present without
  re-stating them in the message.
- **`Internal` errors log a backtrace** (gated on `RUST_BACKTRACE`); expected errors
  (NotFound, Invalid, Degraded) do not — they are routine.

### 15.2.4 Fail-soft reporting to the user

The **fail-soft** invariant (DESIGN_GUIDELINES §2, PRODUCT_SPEC §8) is realised here: a
per-asset failure degrades that asset, never the operation.

- A scan/analyse loop **collects** per-asset errors rather than short-circuiting; the asset is
  recorded with an error/skipped status and a human-readable reason, and the loop continues.
- The user sees the outcome in three parallel channels — the **GUI** flags the asset with its
  reason in the inspector; the **CLI** prints a per-asset warning to stderr and reflects skips
  in its summary and exit code; the **API** returns the asset with an error status field
  ([03](03-library-service-and-api.md)). The message is the same across all three because it
  originates from one typed error.
- Batch operations report **N succeeded / M skipped** with the reasons, so a large messy
  library scans to completion with a legible tail of what it could not handle — the
  tested-at-scale expectation (§15.4.3).

---

## 15.3 Configuration precedence

3DAM has **one config scheme** shared by all three roles; `serve` adds a role-specific config
*file* on top (schema owned by [09](09-server-and-web-client.md)), but the **precedence rule
and the location conventions** are defined here so GUI, CLI, and `serve` resolve settings
identically. "Shared config and database with the desktop app" (DESIGN_GUIDELINES §5) is the
requirement this satisfies.

### 15.3.1 Precedence order

Lowest to highest — a later source overrides an earlier one, key by key:

```
  built-in defaults  <  config file  <  environment variables  <  CLI flags
```

1. **Built-in defaults** — compiled-in, safe-by-default (localhost bind, no auth, log level
   per role, all feature flags off — PRODUCT_SPEC §6.11).
2. **Config file** — the shared config (below). For `serve`, this is the config file
   [09](09-server-and-web-client.md) defines (sources to index, bind address, auth, analysis,
   feature flags); for GUI/CLI it is the smaller shared config (default library/DB path, log
   preferences, UI prefs). One loader, layered.
3. **Environment variables** — `3DAM_*` prefixed (e.g. `3DAM_DB_PATH`, `3DAM_BIND`,
   `3DAM_LOG`), plus the standard `RUST_LOG` honoured for the tracing filter. For containerised
   `serve` deployments env is the primary knob.
4. **CLI flags** — `--config`, `--db`, `--bind`, `--log-level`, `--connect`, etc. Highest
   precedence; an explicit flag always wins (DESIGN_GUIDELINES §5 "no surprises").

The **feature-flag store** is a special case and does **not** live purely in this precedence
chain: flags are persisted server-side (a versioned table beside the metadata DB) and the
config file *seeds* them, but the admin UI/API can edit the persisted state at runtime
([10](10-auth-accounts-and-flags.md), PRODUCT_SPEC §6.11). Config precedence applies at
**startup seeding**; runtime edits then own the state. How config-file vs admin-UI edits
reconcile when both change is an open question carried in `10`/PRODUCT_SPEC §10.

### 15.3.2 Config & database locations

Locations follow OS conventions via the **`directories`** crate (XDG on Linux, Known Folders
on Windows, Application Support on macOS), so GUI and CLI land on the **same** paths and share
one library by default:

| Kind | Linux (XDG) | Windows | macOS |
|------|-------------|---------|-------|
| Config file | `~/.config/3dam/` | `%APPDATA%\3dam\` | `~/Library/Application Support/3dam/` |
| Database + vector index | `~/.local/share/3dam/` | `%LOCALAPPDATA%\3dam\` | `~/Library/Application Support/3dam/` |
| Derivative/blob cache | `~/.cache/3dam/` | `%LOCALAPPDATA%\3dam\cache\` | `~/Library/Caches/3dam/` |
| Log files (`serve`) | `~/.local/state/3dam/` (or share) | `%LOCALAPPDATA%\3dam\logs\` | `~/Library/Logs/3dam/` |

- The **cache** dir holds only regenerable derivatives ([02](02-data-model-and-storage.md)) —
  safe to delete; the DB and vector index are the source of truth.
- **`serve`** typically takes an explicit `--config` / `--db` (a NAS or container path) rather
  than the per-user default, but resolves through the same loader and precedence.
- Credentials never live in any of these files — they go to the OS keychain via `keyring`
  ([10](10-auth-accounts-and-flags.md)).

---

## 15.4 Testing strategy

The bar is **tested at scale** — correctness and performance validated against *large, real,
messy* libraries, not tidy fixtures (DESIGN_GUIDELINES §6). The strategy has four tiers plus
CI guards.

### 15.4.1 Testing tiers

| Tier | What it covers | Where it lives | Runs |
|------|----------------|----------------|------|
| **Unit** | Pure math/core logic with no I/O or GPU — camera/orbit math, ray-pick (Möller–Trumbore + BVH), AABB, tileability edge-continuity math, hashing/ID derivation, query builders, config precedence resolution. Fast, deterministic. | `#[cfg(test)]` in `3dam-core` and pure modules | every push / pre-commit |
| **Integration (per-media)** | Each `MediaHandler` against real fixture files — detect → `extract_metadata` → `thumbnail`/`extract_features` for representative + malformed inputs of each format. Asserts the fail-soft path (a corrupt file yields a Degraded skip, not a panic). | `tests/` per handler crate, fixtures in `tests/fixtures/` | every push |
| **Service/API** | `LibraryService` end-to-end over a temp SQLite library — scan a fixture tree, search, tag, similar, convert dry-run; then the same operations over the HTTP/WS API to prove embedded/connected parity ([03](03-library-service-and-api.md)). | `tests/` in the service + server crates | every push |
| **Scale / perf fixtures** | Large, messy, realistic libraries (below) exercising 100k–1M asset paths, out-of-core datasets, and the fail-soft tail. The "tested at scale" bar and the input for benchmarks/regression guards. | dedicated `xtask`/harness, generated or curated corpora, kept out of the git tree | nightly / pre-release, not per-push |

### 15.4.2 Fixtures

- **Small format fixtures** (checked in): one-or-a-few files per supported audio/image/3D
  format, plus deliberately **corrupt/truncated/wrong-extension** files to exercise detection
  and fail-soft. Kept small so the repo stays lean.
- **Malformed-by-design set:** zero-byte files, valid header + truncated body, a `.png` that
  is actually a `.fbx`, a glTF referencing a missing buffer — each must degrade one asset, per
  §15.2.4.

### 15.4.3 Scale / perf fixtures (the "tested at scale" bar)

The `1M+ assets` design target (PRODUCT_SPEC §8) is only real if it is exercised:

- **Generated corpora:** an `xtask gen-corpus` produces a synthetic library of N assets
  (configurable mix of media types, with realistic size/attribute distributions and a
  controllable fraction of bad files) — cheap to scale to 1M rows for DB/query/out-of-core
  testing without shipping gigabytes.
- **Curated real messy libraries:** a small set of real-world-shaped trees (deep nesting,
  duplicate-heavy, mixed-format packs, wrong extensions, one blanket license over a pack) used
  to validate that dedup, auto-tag, and license handling behave on genuinely untidy input, not
  just generated uniformity.
- These corpora live **outside the git tree** (generated on demand or fetched from a
  developer-controlled location), referenced by the benchmark/scale harness. No corpus is
  fetched during a normal build — consistent with the no-unsolicited-network rule (§15.1.5).

### 15.4.4 Benchmark & regression guards

The **performance *targets*** (60 fps at 100k visible, instant search, cores saturated, 1M
out-of-core) belong to [14](14-concurrency-performance-reliability.md); this file owns the
**tooling that measures them and guards against regression**:

- **`criterion`** benchmarks for the hot pure paths (query building, similarity/ANN lookup,
  hashing, tileability, geometry stats) — statistically sound, with saved baselines.
- **Scale-harness timings:** the `xtask` scale harness measures end-to-end scan/analyse/query
  throughput against the generated corpora and emits a machine-readable report.
- **Regression guard:** benchmarks and scale timings run in a **nightly/pre-release CI job**
  (not per-push — they are slow and machine-sensitive) and **fail if a headline metric
  regresses past a threshold** against the committed baseline. The thresholds trace directly
  to the `14` targets, so a perf regression is caught as a build failure, not in the field.

### 15.4.5 CI dependency-direction guard (ADR 0002)

[ADR 0002](../adr/0002-3d-render-crate-boundary.md) requires that **`3dam-core` never depends
on `wgpu`, `winit`, or any GPU/windowing crate** — the boundary that keeps the engine linkable
into the CLI and API server without dragging in GPU deps (PRODUCT_SPEC §4.3). This is enforced,
not trusted:

- A **CI check** asserts `3dam-core`'s dependency tree contains no GPU/windowing crate —
  implemented as a **`cargo-deny` bans rule** (deny `wgpu`, `winit`, et al. in `3dam-core`'s
  graph) and/or a small `cargo metadata` graph test. It runs on every push and fails the build
  if a GPU crate leaks into core.
- This shares the single `cargo-deny` invocation with the **no-telemetry ban list** (§15.1.5)
  and the license/advisory checks — one dependency-hygiene gate covering all three.

---

## 15.5 Packaging & release

This is the canonical **release & distribution** plan (referenced from PRODUCT_SPEC §8), in
implementable CI detail, and describes the **live** pipeline at
[`.github/workflows/release.yml`](../../.github/workflows/release.yml) (issue #45). It runs four
jobs: `plan` → `web` → `build` (matrix) → `publish`.

### 15.5.1 Trigger & permissions

- **Trigger:** push a `v*` tag, plus manual `workflow_dispatch` for test runs.
  `permissions: contents: write` so the publish job can create the release. The `publish` job
  is gated on `startsWith(github.ref, 'refs/tags/')`, so a manual dispatch builds artifacts but
  never cuts a release.
- **`publish` needs `build`**, so with `fail-fast: false` a single failing platform still
  blocks the release: a tag can never publish a partial set of OSes.
- **Cost control.** The repository is private, so runner minutes bill with multipliers —
  **macOS 10x, Windows 2x, Linux 1x** — against a build that compiles Assimp from source on
  every target. A manual dispatch therefore takes a `targets` input (`linux` | `macos` |
  `windows` | `all`) that **defaults to `linux`**, so iterating on the pipeline costs the
  cheapest runner. A tag push always builds everything.

### 15.5.2 Version/tag agreement (checked before anything builds)

The Tauri bundler derives installer names from `[workspace.package] version` in `Cargo.toml`,
while the archives are named from the git tag. If those disagree, one commit ships
`3dam-v0.2.0-…tar.gz` alongside `3DAM_0.1.1_amd64.deb` and nothing downstream notices. `plan`
resolves the version with `cargo pkgid -p dam` (no `jq` dependency, so the check is
reproducible locally) and **fails the run** on a mismatch. A non-tag run is named
`dev-<short-sha>` so manual artifacts can never be mistaken for a published version.

### 15.5.3 Build matrix

`fail-fast: false`, one runner per target — a build failure on one OS does not cancel the
others. The matrix is emitted as JSON by `plan` so the dispatch input can narrow it:

| Target triple | Runner | Notes |
|---------------|--------|-------|
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | Oldest supported glibc, for the widest binary compatibility. Deprecation begins 2026-09-17 (longer queue times); **retired 2027-04-17**, with brownouts in the preceding March/April windows. Move to `ubuntu-24.04` before then. |
| `x86_64-apple-darwin` | `macos-15-intel` | Intel mac. Actions ends x86_64 macOS support in Aug 2027; `macos-26-intel` also exists if a newer base is wanted. |
| `aarch64-apple-darwin` | `macos-15` | Apple Silicon. Pinned rather than `macos-latest`, which migrated to macOS 26 over June–July 2026. |
| `x86_64-pc-windows-msvc` | `windows-2025` | MSVC toolchain. |

Toolchain via **`dtolnay/rust-toolchain`** pinned to the target triple; caching via
**`Swatinem/rust-cache`** (keyed per target, so the legs cannot collide).

### 15.5.4 Build the web client **once**, then embed it everywhere

A 3DAM release must **build the React web client and embed its static assets into the `3dam`
binary before `cargo build`**, so the single binary serves the web UI with no separate deploy.
The client is platform-independent, so it is built in **one** `web` job and consumed by every
build leg as an artifact — four identical builds would be four chances to diverge, and this
keeps pnpm/Node/wasm-pack off the macOS and Windows runners entirely.

1. Set up **pnpm** + **Node 20**, and a Rust toolchain with the `wasm32-unknown-unknown` target.
2. Install **`wasm-pack`** and run `cargo xtask wasm`, producing `web/src/wasm/`.
3. `pnpm install --frozen-lockfile && pnpm run build` in `web/`, producing `web/dist`.
4. Upload `web/dist` as an artifact; each build leg downloads it before `cargo build`.
5. `3dam` embeds `web/dist` via **`rust-embed`** at compile time, then
   `cargo build -p dam --release --locked --target <triple>`.

Two **silent-success traps** are asserted against rather than trusted, because both produce a
green build that is quietly broken:

- `cargo xtask wasm` **returns success when `wasm-pack` is missing** (it is designed to skip
  gracefully for local native-only builds). `web/src/wasm/` is gitignored and
  `web/src/islands/index.ts` imports `@/wasm/dam_viewer.js` behind a `@ts-ignore`, so `tsc -b`
  passes and only `vite build` fails — the job therefore asserts `dam_viewer.js` and
  `dam_viewer_bg.wasm` exist.
- `dam-server` **degrades to a "web client bundle is not present" placeholder** when `web/dist`
  exists but is *empty* — rust-embed only checks that the folder exists, so the binary compiles
  and serves a stub instead of the UI. (A *missing* `web/dist` is by contrast a hard compile
  error: rust-embed refuses unless `allow_missing` is set, and it is not. `web/.gitignore`
  ignores `dist/`, so on a clean checkout the directory does not exist and
  `actions/download-artifact` is what creates it — which makes the empty case the reachable
  failure.) Each build leg therefore asserts the downloaded client is present before building.

The `--locked` flag makes the build **reproducible** against the committed lockfile
(PRODUCT_SPEC §8 reproducibility).

### 15.5.5 Linux system dependencies

Installed with plain `apt-get` (not `awalsh128/cache-apt-pkgs-action`, which can cache an empty
package set after a resolution failure and then fail confusingly at the build step instead):

- **Tauri shell ([ADR 0013](../adr/0013-desktop-shell-tauri.md)):** `libgtk-3-dev`,
  `libwebkit2gtk-4.1-dev` (present in jammy universe), `libssl-dev`, `pkg-config`.
- **`librsvg2-dev` is load-bearing for the AppImage**, not merely for icons: linuxdeploy's gtk
  plugin aborts with `there is no 'libdir' variable for 'librsvg-2.0' library` without it.
- **No `libasound2-dev`** — `cpal` is not in this dependency tree. (The `xkbcommon`/`wayland`/
  `xcb` set the egui client needed is likewise gone; see ADR 0013.)
- The **headless-serve software rasteriser** (Mesa lavapipe/llvmpipe, PRODUCT_SPEC §6.8) is a
  **runtime** concern on the serve target, **not a build dependency** — it is not installed
  here.

### 15.5.6 Per-OS packaging

Every native installer comes from **one tool, `cargo tauri bundle`**, which the repo already
depends on and has fully configured in
[`crates/3dam-desktop/tauri.conf.json`](../../crates/3dam-desktop/tauri.conf.json) (identifier,
publisher, category, descriptions, all six icons). Using cargo-deb/cargo-wix instead would mean
maintaining that same app metadata a second and third time, in two more formats:

| OS | Artifacts | Tooling |
|----|-----------|---------|
| **Linux** | `tar.gz` of the binary **+** `.deb` **+** `.AppImage` | `tar`; `cargo tauri bundle --bundles deb,appimage` |
| **Windows** | `.zip` **+** `.msi` **+** NSIS `-setup.exe` | `7z`; `cargo tauri bundle --bundles msi,nsis` |
| **macOS** | `tar.gz` of the binary **+** `.dmg` | `tar`; `cargo tauri bundle --bundles app,dmg` (`app` builds the `.app` the `.dmg` wraps; only the `.dmg` is uploaded) |

On Windows the bundler **downloads its own toolchains** — WiX 3.14.1 (`wix314-binaries.zip`)
and NSIS 3.11 plus `nsis_tauri_utils.dll` — hash-verified and cached under `%LOCALAPPDATA%\tauri`.
No toolset need be preinstalled on the runner, but two things follow: the bundle step **needs
network** on a cold cache, and the MSI needs Windows' **VBSCRIPT optional feature**, which is
enabled on runner images today but has a deprecation announced. That is the part of this
pipeline most likely to break first.

Ordering constraint: **the portable archive must be cut before the bundler runs.** The bundler
rewrites the built binary in place (`Patching … with bundle type information: deb`), stamping a
fixed-width token that `tauri::process` reads back at runtime. Since tauri-bundler 2.9 it
snapshots and restores the pristine binary afterwards — but only on the success path, so a run
that fails partway leaves the stamp behind (confirmed by inspection: after a failed AppImage
bundle the binary still read `__TAURI_BUNDLE_TYPE_VAR_APP` rather than `…_UNK`). Archiving
first keeps the portable download clean regardless of how the bundler exits.

Each artifact is renamed `3dam-<tag>-<target>.<ext>` and uploaded via `actions/upload-artifact`
with **`if-no-files-found: error`** — with `warn`, a leg that produced nothing still passes but
creates no artifact object at all, and `publish` then fails while downloading it, a long way
from the cause.

### 15.5.7 Shell completions & man pages

[ADR 0009 §10](../adr/0009-v1-scope-decisions.md) commits to shipping `clap_complete` shell
completions and `clap_mangen` man pages. Both are **rendered at runtime by the binary the leg
just built**, through two hidden verbs — `3dam completions <bash|zsh|fish|powershell>` and
`3dam man` — each of which also takes `--out <DIR>` to write conventionally-named files instead
of streaming to stdout.

*Runtime verbs, not a `build.rs` and not an xtask that re-declares the grammar.* The clap tree in
[13](13-cli.md) is the single source of truth and a build script cannot see it — it would have to
parse or duplicate the derive types, which is exactly the second copy this avoids. Generating
from the shipped binary makes the artifacts by construction the ones that binary accepts, and it
gives users `eval "$(3dam completions zsh)"` with no download. The cost is that generation is a
*packaging* step rather than a build output, hence `cargo xtask packaging [--target <triple>]`,
which stages:

```
packaging/completions/{3dam.bash,_3dam,3dam.fish,_3dam.ps1}
packaging/man/{3dam.1.gz, 3dam-scan.1.gz, 3dam-admin-token-add.1.gz, …}
```

One wrinkle: `serve` and `mcp` are roles that `classify` intercepts *before* the CLI grammar
([01](01-architecture.md) §5), so their arguments live in their own `Parser` structs and are
invisible to `Cli::command()`. Completions describe the *binary*, not the CLI role, so the
generator grafts them back on — with a unit test guarding the graft, since losing it would
silently drop two of the four roles from every completion script. `clap_mangen` then recurses,
giving the deep `admin` tree a page each, which is the point: `man 3dam-admin-token-add` is
where a reader actually looks. Pages are gzipped (Debian policy §12.1; the Tauri bundler
compresses nothing but its own changelog), skipped with a notice where `gzip` is absent, since
only the `.deb` needs the compressed form.

Two consumers read that directory, and CI **asserts** the filenames rather than globbing them —
the same reasoning as `Collect installers` (§15.5.6). `clap_complete` chooses these names and the
shells' completion loaders look them up by name, so an upstream rename would otherwise ship a
package whose completions silently never load, with every job still green.

- **Every portable archive** carries `completions/` and `man/` beside the binary. The archive is
  the only channel on macOS and Windows, and for anyone on Linux who does not install the package.
- **The `.deb`** installs them to the usual paths via `bundle.linux.deb.files` in
  [`tauri.conf.json`](../../crates/3dam-desktop/tauri.conf.json):
  `/usr/share/bash-completion/completions/3dam`, `/usr/share/zsh/vendor-completions/_3dam`,
  `/usr/share/fish/vendor_completions.d/3dam.fish`, and the whole man directory to
  `/usr/share/man/man1`. Two things about that map are easy to get wrong: its *source* paths
  resolve relative to the `tauri.conf.json` directory (not the workspace root), and **a missing
  source is a hard bundler error**. Staging is therefore a prerequisite of `cargo tauri bundle`,
  not an optional extra — `cargo xtask bundle` stages before it bundles, every time, so a renamed
  subcommand cannot leave a stale page behind.

Deliberately **not** in the AppImage: its bundler keeps a separate `files` map that does not
inherit the deb's, and completions/man pages are inert inside an image that is never installed
system-wide. PowerShell completions are likewise absent from the `.deb`, which has no
conventional path for them — they ship in the archives.

`packaging/` is a gitignored **top-level** directory rather than the more obvious
`target/packaging/`, because `bundle.linux.deb.files` takes a *literal* string and cargo's target
directory is not a fixed location — `[build] target-dir` in `~/.cargo/config.toml` or
`CARGO_TARGET_DIR` relocates it wholesale, which is a normal thing for a developer to set. For
the same reason `xtask packaging` renders through `cargo run` rather than executing a binary path
it constructs: cargo knows where its own output lives, and we do not.

### 15.5.8 Smoke-testing the artifact

Acceptance for packaging is that the artifact *launches*, not that it builds. A CI runner is a
clean machine, so each leg unpacks its own archive into a fresh directory and, against that
copy (which also proves the archive is well-formed):

1. `3dam --version` matches the version this run claims to ship.
2. `3dam --data <tmp> stats --json` opens a fresh catalog — running every schema migration —
   and emits the documented JSON.
3. The unpacked archive carries `completions/3dam.bash` and a `man/3dam.1`, and the binary can
   still render them on demand (`3dam completions bash`). They are generated rather than
   compiled in, so without this a broken generator would only surface at the *next* release.
4. `3dam serve` binds, answers `/healthz`, and serves the **embedded** client: the response
   body is grepped for `assets/`, which the "bundle is not present" placeholder does not
   contain, so a binary built without `web/dist` fails here instead of shipping.

### 15.5.9 Publish

A `publish` job (`needs: [build]`, tag-gated):

1. Download all `release-*` artifacts (`merge-multiple: true`).
2. **Generate `SHA256SUMS`** over every archive (fails if none found — an empty release is a
   bug, not a success).
3. **Create the GitHub Release** with **`softprops/action-gh-release`**, attaching every
   archive **and `SHA256SUMS`** so users can verify downloads.

GitHub Releases is the **primary and only guaranteed channel for v1**. Mogen's itch.io/butler
push (game-store specific) is deliberately **not** carried over.

---

## Open questions

Carried from PRODUCT_SPEC §10 open questions, surfaced here because they gate this file's
packaging/release scope:

- ~~**Code signing & notarization.**~~ **Decided (2026-07-06): unsigned for v1.** Accept the
  Gatekeeper (macOS `.dmg`) and SmartScreen (Windows `.msi`) warnings as mogen does — no Apple
  Developer ID + notarization or Windows signing certificate, and no signing/notarization/stapling
  steps in §15.5.6 for v1. Revisit post-v1.
- **Distribution channels beyond GitHub Releases.** Homebrew tap, winget, AUR,
  `cargo-binstall` — which, and when. All TBD; GitHub Releases is the v1 channel (§15.5.9).

Owned in this file, surfaced for the roll-up ([00](00-overview.md) §Open questions):

- **Serve log rotation & retention policy** — file size/age caps and how many rolled files to
  keep for `serve`; whether these are config-file keys ([09](09-server-and-web-client.md)) or
  fixed defaults. (Retention only; there is no remote sink — §15.1.5.)
- **Config-file vs admin-UI reconciliation** at runtime for the feature-flag store — noted in
  §15.3.1, but the resolution is owned by [10](10-auth-accounts-and-flags.md) / PRODUCT_SPEC
  §10.

---

See also: [00-overview.md](00-overview.md) ·
[03-library-service-and-api.md](03-library-service-and-api.md) ·
[09-server-and-web-client.md](09-server-and-web-client.md) ·
[10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md) ·
[14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md) ·
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) · [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) ·
[ADR 0002](../adr/0002-3d-render-crate-boundary.md)
