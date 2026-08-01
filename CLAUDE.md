# CLAUDE.md — 3DAM

Guidance for AI agents (and humans) working in this repository. Read this before making changes.

## What 3DAM is

3DAM is a cross-platform, Rust-first **game-asset manager** for **audio, image, and 3D** assets — plus the **video and documents** that sit alongside them in a project folder ([PRODUCT_SPEC §9 phase 2b](docs/PRODUCT_SPEC.md); those two are deliberately shallower than the three deep types). It unifies them into one local SQLite catalog and layers **content-based automation** on top: metadata extraction, thumbnails, embeddings, similarity search, auto-tagging, and deduplication. It is local-first, non-destructive, and federation-ready.

**One binary, four roles.** The `3dam` binary dispatches on `argv[1]`:
- (no arg) → **GUI** (native desktop: a Tauri webview shell over the embedded web client, [ADR 0013](docs/adr/0013-desktop-shell-tauri.md))
- `serve` → **HTTP/WS server** + embedded web client + MCP endpoint
- `mcp` → **MCP server** over stdio
- anything else → **CLI** (run-and-exit; `scan`, `search`, `convert`, …)

Every role talks to the same engine through one trait, `LibraryService`, so the CLI, server, GUI, and MCP surface are thin adapters over identical logic.

---

## Golden rules

1. **One UI codebase — the web client is the UI.** 3DAM has one user-facing UI, `web/` (React 19 + TypeScript + Tailwind v4), delivered through two shells: the browser (`3dam serve` embeds the built client) and the **native desktop app** (`crates/3dam-desktop`, a Tauri webview over an in-process server — [ADR 0013](docs/adr/0013-desktop-shell-tauri.md), superseding the egui client of ADR 0005). Web ↔ native parity is therefore **structural**: build a user-facing feature once in `web/` and it ships everywhere. What still needs deliberate care: both form factors (the desktop viewport *and* the responsive/touch collapse) must keep working, and desktop-only affordances (native dialogs, tray, OS integration) are additive Tauri work in `crates/3dam-desktop`, never a parallel UI. History: [docs/GUI_PARITY.md](docs/GUI_PARITY.md).

2. **The spec is the single source of truth.** `docs/PRODUCT_SPEC.md` **§9** is the authoritative capability roadmap. `docs/ROADMAP.md` only tracks *status* against it — it is not a separate plan. When scope is ambiguous, defer to §9.

3. **Non-destructive by default.** The convert/export pipeline must never write into a registered source. Writes go to a temp file then atomic-rename into an output dir; collisions follow an explicit policy. Preserve this invariant.

4. **Capabilities are off-by-default feature flags.** Remote access, auth, accounts, MCP, network writes, federation, and analysis are runtime flags in `server.db` (not Cargo features), per [ADR 0004](docs/adr/0004-runtime-feature-flags.md). Off means the surface disappears. Don't turn things on by default.

5. **Heavy work never blocks the UI — by construction.** `tokio` owns I/O-bound work (sources, server, SQLite); `rayon` owns CPU-bound work (decode, analysis). The async→CPU hand-off is one-shot (`spawn_blocking` / `rx.await`). See [ADR 0007](docs/adr/0007-concurrency-tokio-rayon.md). Long operations are **jobs** with progress events, not blocking calls.

6. **Fail-soft.** A corrupt asset, an offline source, or an unsupported format degrades gracefully (per-item error) — it never aborts a scan or crashes a client.

---

## Architecture

### The seams (stable abstraction boundaries)

| Seam | Defined in | Implementations | Why it exists |
|---|---|---|---|
| **`LibraryService`** trait | `crates/3dam-api/src/service.rs` | `EmbeddedLibrary` (`dam-core`), `ApiClient` (`dam-client`) | Every frontend holds `Box<dyn LibraryService>` and can't tell embedded from remote. Enables offline CLI/GUI *and* connected mode with no recompile. Carries `AuthContext` on every call. |
| **`FileSource`** trait + `open_source()` | `crates/3dam-sources/src/lib.rs` | `LocalFsSource`, `SftpSource` (russh), `SmbSource` (smb) | Decouples scan/ingest from the I/O protocol. `walk()` yields entries; `fetch()` materialises bytes (in-place for local, temp file for remote). New sources (S3, peers) plug in here. |
| **`Backend` / `open_backend()`** | `crates/3dam-frontend/src/lib.rs` | `Embedded { data_dir }` vs `Connected { endpoint, token }` | Single point where CLI/GUI resolve embedded-vs-remote from `--data` / `--connect` / `--token`. |
| **`Role` / `classify()`** | `crates/3dam-frontend/src/lib.rs` | `Gui` / `Serve` / `Mcp` / `Cli` | One binary, four entry points — chosen by peeking at `argv`, no features or rebuild. |
| **EmbeddingSpace** (data-level) | `embedding` table, `dam-store` schema V3+ | model-free v1 vectors; SigLIP/CLAP later | `space_id` = `model@version+media+dim+metric`. Isolates embedding generations so a model bump invalidates only its slice. Powers `similar` / dedup. Model-backed extractors are a later feature-gated bump behind this seam. |

### Crate map (`crates/`)

Packages are named `dam-*` (Cargo forbids leading digits); directories are branded `3dam-*`; the binary is `3dam` — see [ADR 0010](docs/adr/0010-cargo-package-naming.md). Imports use snake_case (`dam_core`, `dam_api`).

| Directory | Package | Role | Notes |
|---|---|---|---|
| `3dam-api` | `dam-api` | The seam: `LibraryService` trait, DTOs, `AuthContext`, `Scope`/`Scopes`, events | Foundational. serde-only, no other `dam-*` deps. |
| `3dam-store` | `dam-store` | Synchronous SQLite catalog (`library.db`); schema + migrations | Private to `dam-core`. Never exposed to frontends. |
| `3dam-media` | `dam-media` | `detect()` / `detect_for_ingest()` / `extract_metadata()` (cheap tier) / `render_thumbnail()` (images + video poster frames) / `extract_text()` / convert / feature extraction | symphonia (audio), image (rasters) + `dds`/`ktx2`/`texture2ddecoder` (GPU texture containers, issue #49), gltf (3D) + Assimp's own exporter for model→GLB convert (behind `model-convert`), a **discovered `ffprobe`/`ffmpeg`** (video, [ADR 0015](docs/adr/0015-video-decode-backend.md)), lopdf + zip/quick-xml (documents). |
| `3dam-sources` | `dam-sources` | `FileSource` + local/SFTP/SMB; `SourceConnection` | SFTP/SMB behind cargo features. |
| `3dam-core` | `dam-core` | **The engine.** `EmbeddedLibrary` impls `LibraryService`; scan/analyze/convert/export jobs, watch/auto-rescan, events, thumbnail cache | Pure logic — no UI, transport, or GPU. |
| `3dam-client` | `dam-client` | `ApiClient` impls `LibraryService` over HTTP/WS | The "connected" backend (CLI/GUI `--connect`). |
| `3dam-server` | `dam-server` | axum server: `/api/v1` REST + WS, embedded web client (`rust-embed`), auth/admin, MCP mount; owns `server.db` (tokens, flags, audit) | Wraps an `EmbeddedLibrary`. |
| `3dam-frontend` | `dam-frontend` | `Backend`/`open_backend` + `Role`/`classify` dispatch glue | Tiny; shared by CLI + GUI. |
| `3dam-cli` | `dam-cli` | clap command tree; also exports `serve()` and `mcp()` entry points | Human output by default, `--json`/`--csv` opt-in. |
| `3dam-desktop` | `dam-desktop` | Native desktop shell (Tauri 2 webview) | Boots `dam_server::serve_desktop` in-process (loopback, ephemeral port) and opens the embedded web client in a native window; `--connect` navigates to a remote server instead ([ADR 0013](docs/adr/0013-desktop-shell-tauri.md)). Replaced the egui client (`3dam-gui`, in git history). |
| `3dam` | `dam` | Binary entrypoint: `classify(argv)` → dispatch | GUI is sync; CLI/serve/mcp run on tokio. |
| `3dam-render` | `dam-render` | Headless wgpu render-to-PNG (textured-PBR turntable thumbnails) + the self-contained `DMSH` preview blob | Real; all formats via Assimp/russimp-ng, behind dam-core's `render` feature ([ADR 0011](docs/adr/0011-assimp-import-backend.md)). |
| `3dam-viewer` | `dam-viewer` | Browser WASM/wgpu viewer islands (3D model + audio waveform) | `cdylib`+`rlib`; GPU deps only under `cfg(target_arch = "wasm32")`, so native `cargo build` skips them. |

### Dependency flow

`dam-api` is the root (serde-only). `dam-media`/`dam-sources` are leaf handlers → `dam-store` → `dam-core` (engine). `dam-client` implements the same trait over HTTP. `dam-server` wraps `dam-core`. `dam-frontend` glues embedded+connected; `dam-cli` sits on `dam-frontend`+`dam-server`; `dam-desktop` sits on `dam-server` (in-process serve) + Tauri; `dam` dispatches into cli/desktop. `dam-render`/`dam-viewer` are independent GPU crates.

### Data & storage

- Data dir: platform default (see `dam-core/src/paths.rs`), overridable with `--data`.
- `library.db` — the catalog (assets, sources, jobs, collections, tags, embeddings). Owned by `dam-store`, private to the engine.
- `server.db` — server config, tokens, feature flags, audit log. Owned by `dam-server`.
- `scratch/` — where a remote (SFTP/SMB) fetch materialises downloaded bytes (`paths::scratch_dir`). **Never** `std::env::temp_dir()`: that is a tmpfs on most Linux hosts, and a remote video can be gigabytes ([issue #87](https://github.com/krazyjakee/3DAM/issues/87)). `open_source` therefore takes the scratch dir as a *required* argument — a default would silently reintroduce the bug. Emptied at engine open (`dam_sources::clean_scratch`), since a `kill -9` mid-fetch leaves files that nothing else will collect.
- **Schema is forward-only** (`PRAGMA user_version`), currently **V15**: V1 catalog → V2 audio codec/container → V3 `embedding` table → V4 blocklist → V5 model dependency bytes → V6 `asset_fts` FTS5 index → V7 FTS rebuilt with a `tags` column → V8 audio waveform peaks → V9 `job.sources` → V10 `video_attr` + `document_attr` → V11 FTS rebuilt with a `text` column → V12 `asset_note` + FTS rebuilt with a `note` column (and the dead V1 `asset.notes` column dropped) → V13 FTS rebuilt with a `folder` column → V14 `asset_comment` (per-asset discussion) → V15 `image_attr.texture_format` + `mip_levels` (GPU texture containers, issue #49). A DB from a *newer* schema is rejected rather than downgraded. When you change the schema, add a numbered migration in `dam-store/src/schema.rs` and bump the version — never edit an existing migration.

---

## Developer workflow

MSRV **1.91**, edition 2021, resolver 2. The floor is dictated by the locked dependency tree, not our own source — libsqlite3-sys 0.38's build script uses the `cfg_select!` macro (stable since 1.91), and image 0.25 / wgpu 30 / naga 30 want 1.87–1.88. Bump `rust-version` only after rebuilding to confirm the new floor, and keep this line + `Cargo.toml` + README in sync. No `rustfmt.toml`, `clippy.toml`, or `deny.toml` — Rust defaults apply. The one `.cargo/config.toml` exists solely to force Assimp's Draco decoder on at build time via `CMAKE_TOOLCHAIN_FILE` (see [ADR 0011](docs/adr/0011-assimp-import-backend.md)); it affects only the `russimp-sys-ng` cmake build.

### Build & run

```bash
cargo build --workspace                 # native build (skips wasm-only deps in dam-viewer)
cargo build -p dam --release            # the 3dam binary

# Run roles via cargo (or the built binary):
cargo run -p dam -- serve               # HTTP/WS server + web client (binds 127.0.0.1:7878)
cargo run -p dam -- scan <dir> --wait   # CLI: scan a folder and wait for the job
cargo run -p dam -- search "brick" --media image --json
cargo run -p dam -- --connect http://host:7878 --token <t> search "kick"
cargo run -p dam --                     # GUI role (Tauri desktop shell over the embedded web client)
```

Key CLI verbs (verb-noun, per tech-spec 13): `scan`, `search`, `get`, `stats`, `sources {list,add,remove}`, `folders`, `collections {…}`, `convert`, `analyze`, `similar`, `dedup`, `tag`, `note`, `export`, `jobs`/`job`, `admin {status,flags,flag,token,audit}`, plus `serve` / `mcp`. Global flags: `--connect`, `--token`, `--data`, `--json`.

### Test, lint, format

```bash
cargo test --workspace                  # integration tests live in crates/3dam-core/tests and crates/3dam-server/tests
cargo fmt --all --check                 # format check (use without --check to apply)
cargo clippy --all-targets -- -D warnings
```

Server tests exercise the axum router in-process via `ServiceExt::oneshot` (no socket bind). Core tests cover scan / media_depth / automation / reach.

### `xtask` (dev automation)

```bash
cargo xtask ci          # fmt --check + clippy -D warnings + tests + web build (the canonical pre-push gate)
cargo xtask web         # build the React client → web/dist/ (builds wasm first; skips gracefully if pnpm missing)
cargo xtask wasm        # wasm-pack build dam-viewer → web/src/wasm/ (skips gracefully if wasm-pack missing)
cargo xtask packaging   # render shell completions + man pages → packaging/ by *running* the
                        # built 3dam (`3dam completions <shell>` / `3dam man`, ADR 0009 §10).
                        # --target <triple> finds a cross-built binary, as release CI does.
cargo xtask bundle      # package the desktop app (deb/AppImage): web → release 3dam → packaging →
                        # `cargo tauri bundle` (skips gracefully if tauri-cli missing; the bundler
                        #  wraps the pre-built target/release/3dam via mainBinaryName — dam-desktop
                        #  itself stays a lib)
cargo xtask check-deps  # dependency-direction guard (placeholder, not yet enforced)
```

### Web client

Package manager is **pnpm** (v9 lockfile). From `web/`:

```bash
pnpm install --frozen-lockfile
pnpm dev          # Vite dev server on :5173, proxies /api → 127.0.0.1:7333 (see gotcha below)
pnpm build        # tsc -b && vite build → web/dist/
pnpm typecheck    # tsc -b --noEmit   (also what `pnpm lint` runs)
pnpm wasm         # cargo xtask wasm
```

`web/dist/` is baked into `dam-server` via `rust-embed` (debug reads from disk; release compiles in). SPA-fallback: unknown non-`api/` paths serve `index.html`; `assets/*` get `immutable` cache headers.

### Prerequisites for the full build

`wasm-pack` (WASM viewer) and `pnpm` (web) are optional for a native-only Rust build — xtask skips them gracefully — but required for a complete `serve` (and desktop shell) with a real web client. Linux builds need the usual system libs (GTK3 + **webkit2gtk-4.1** for the Tauri shell, ssl/pkg-config; see `.github/workflows/release.yml`).

---

## Web UI shape (this is the whole UI — browser and desktop shell)

- **Three-region workspace** (`web/src/components/Workspace.tsx`): **Navigation** (left, filters + sources), **Browser** (center, virtualised grid/table via `@tanstack/react-virtual`, 100k+ rows), **Inspector** (right, detail + preview), plus a bottom **StatusBar** (live jobs + stats).
- **State:** server state via **TanStack Query** (`web/src/api/queries.ts`); UI/view state (search, filters, selection, view mode) lives in **URL params** (`web/src/lib/view-state.ts`) for deep-linking; live updates via **WebSocket** (`web/src/api/ws.ts`) invalidating query keys.
- **API layer:** `web/src/api/client.ts` mirrors `dam-api` DTOs over `/api/v1`. Keep these in lockstep with the Rust DTOs.
- **WASM islands** (`web/src/islands/`): 3D model viewer + audio waveform, lazy-loaded from `@/wasm/dam_viewer.js` on first use only (thumbnail grid pulls zero WASM). WebGPU with WebGL2 fallback.
- **Responsive/touch:** three-region collapses below Tailwind `lg`; Navigation & Inspector become `Drawer` overlays; `coarse:` variant (`@media (pointer: coarse)`) enforces 44px tap targets.
- **Styling:** Tailwind v4 with inline `@theme` tokens in `web/src/index.css` (no `tailwind.config.js`). Dark-first, low-chrome, information-dense; one accent (sky `#38bdf8`); warn/danger reserved for exposure risk. Reuse the `.btn` / `.field` component classes and the design tokens rather than hardcoding colors.

---

## Conventions

- **Naming:** package `dam-*`, directory `3dam-*`, binary `3dam`, imports `dam_*`. React components PascalCase, flat per-domain dirs (`components/`, `api/`, `islands/`, `lib/`).
- **Branching:** work directly on `main` — do **not** create feature branches. Commit to `main` and (when asked) push there. This overrides any default "branch before committing" behaviour.
- **Commits:** imperative, capability-focused summaries (e.g. "Implement phase 2: Media depth"). End co-authored commits per repo convention.
- **DTOs are contracts:** `dam-api` types, the server routes, `dam-client`, and `web/src/api/types.ts` must agree. Change them together.
- **Respect the layer boundaries:** frontends depend on `dam-api` (the trait), never on `dam-store` or the engine internals. Don't reach around `LibraryService`.

---

## Gotchas

- **Dev-proxy port mismatch.** `3dam serve` binds **`127.0.0.1:7878`** by default, but `web/vite.config.ts` proxies `/api` to **`127.0.0.1:7333`**. For local web dev, either run `3dam serve --addr 127.0.0.1:7333` or set `VITE_API_TARGET=http://127.0.0.1:7878` before `pnpm dev`. (This inconsistency is real in the tree — don't "fix" one side without checking the other.)
- **The desktop app is a webview, not a native toolkit.** `dam-desktop` (Tauri) renders the embedded web client — "native GUI work" is almost always `web/` work. The webview loads an `http://127.0.0.1` origin, so Tauri IPC is deliberately unavailable in page JS; native affordances need explicit capability plumbing (ADR 0013). The egui client (`3dam-gui`) was removed at ADR 0013 — it lives in git history, don't resurrect pieces of it casually.
- **The native renderer is real (no longer a stub).** `dam-render` produces textured-PBR turntable thumbnails + the `DMSH` preview blob (consumed by the web/WASM viewer), behind dam-core's `render` feature.
- **Embeddings are model-free in v1.** Similarity/dedup work off a v1 vector behind the EmbeddingSpace seam. SigLIP (image/3D, candle) and CLAP (audio, ort/ONNX) are a later feature-gated bump — see `spikes/embedding-models/` and [ADR 0006](docs/adr/0006-inference-candle.md).
- **Video decode is an *optional external binary*, not a linked library.** `dam-media` discovers `ffprobe`/`ffmpeg` on `PATH` at runtime (override with `DAM_FFPROBE`/`DAM_FFMPEG`) and degrades in tiers: nothing installed → the video is still catalogued, browsable, and playable in the browser but has no metadata and shows a typed tile; `ffprobe` → full cheap tier; both → poster-frame thumbnails. There is deliberately **no cargo feature** for this — nothing is gated at build time, so the same binary upgrades itself when a user installs ffmpeg ([ADR 0015](docs/adr/0015-video-decode-backend.md)). Video is not a convert target.
- **`detect()` vs `detect_for_ingest()`.** The first answers "what is this file?"; the second answers "should the catalog hold it?" and is what a **scan** must call. It drops **documents** (never other media) found under dependency/build/VCS directories, so one `npm install` can't bury a project's assets under thousands of `README.md`s. A root `LICENSE.txt` is deliberately kept — the licence surface points at it.
- **Some searchable text lives *only* in the FTS index, not in any base-table column.** `asset_fts` carries `tokens` (filename sub-tokens), `tags`, `note`, `folder` (tokenised directory segments, written at scan), and `text` (extracted document body, written by the analyse pass). Search ranks in **two levels**: a categorical tier (`search::AUTHORED_TIER`) that puts any filename/token/tag/**note**/**folder** match above every body-text-only match, then weighted bm25 (`search::FTS_RANK`) within the tier. Don't collapse that to weights alone — bm25 saturates term frequency, so a sufficiently repetitive document ties any finite filename weight. And because `tokens`/`tags`/`text` have no home outside the index, **any FTS rebuild must stash and restore them** — V7, V11, V12, and V13 are the worked precedents, and `schema::tests::the_fts_rebuilds_preserve_the_index_only_columns` is the regression guard. Two columns are exceptions that a rebuild can *derive* rather than carry: `note` is mirrored in `asset_note`, and `folder` is a pure function of `asset.path` (V13 back-fills it in SQL, coarser than the Rust tokeniser, and the next scan refines it).
- **Newer-schema DBs are rejected.** If you bump the schema and then run an older binary against that DB, it refuses to open by design.
- **Release CI is live, and the web client is built once for all four targets.** `.github/workflows/release.yml` fires on `v*` tag pushes (issue #45). A tag whose version disagrees with `[workspace.package] version` is rejected before anything builds — the Tauri bundler names installers from Cargo.toml while the archives are named from the tag, so a mismatch would ship two versions of the same commit. The React client + WASM islands are built in **one** `web` job and downloaded by every build leg, so all four binaries embed identical assets and only that job needs pnpm/wasm-pack; note `cargo xtask wasm` **silently succeeds** when `wasm-pack` is absent, so the job asserts its outputs exist rather than trusting the exit code. Native installers (`.deb`/`.AppImage`, `.dmg`, `.msi`/NSIS) all come from `cargo tauri bundle`, which already owns the app metadata — there is no cargo-deb/cargo-wix. The portable archive must be cut *before* bundling: the bundler stamps a bundle-type token into the binary and only restores the pristine copy on its success path, so a bundler run that fails partway leaves the stamp behind. Because this is a **private** repo, runner minutes bill at 10x for macOS and 2x for Windows, so a manual run defaults to Linux only and takes a `targets` input to widen it.
- **`cargo tauri bundle` now depends on `cargo xtask packaging` having run.** Shell completions and man pages (ADR 0009 §10) are rendered by *running the built binary* — `3dam completions <shell>` and `3dam man` are hidden verbs, so the clap tree in `dam-cli` stays the only copy of the grammar and there is no build script duplicating it. `xtask packaging` stages the output under `packaging/`, which `tauri.conf.json` names under `bundle.linux.deb.files`; the Tauri bundler treats a **missing source file as a hard error**, and its source paths resolve relative to the `tauri.conf.json` directory, not the workspace root. So a bare `cargo tauri bundle` on a fresh clone fails until you stage — `cargo xtask bundle` does it for you, and release CI has its own step. Note `packaging/` is top-level and *not* under `target/`: the deb config needs a literal path, and a `[build] target-dir` override (one is set on this machine) moves `target/` somewhere else entirely — which is also why `xtask packaging` shells out to `cargo run` instead of executing a binary path it built by hand. The AppImage deliberately gets none of this (separate `files` map, and the files are inert in an image that is never installed system-wide).

---

## Where to read more

- `docs/MISSION.md` — vision, principles, success criteria.
- `docs/PRODUCT_SPEC.md` — product spec; **§9 is the authoritative roadmap**.
- `docs/ROADMAP.md` — status against §9 (phases 1–5 shipped; 6 federation/auth & 7 polish/scale later; 8 beyond v1).
- `docs/DESIGN_GUIDELINES.md` — dark-first, information-dense UI rules (governs both clients).
- `docs/tech-spec/00`–`15` — numbered deep specs. Crate descriptions cite them (e.g. 01 architecture, 03 LibraryService/API, 05 analysis, 09 server/web, 12 GUI, 13 CLI, 14 concurrency).
- `docs/adr/0001`–`0014` — decisions (render backend, crate split, MCP, feature flags, egui [superseded], candle, tokio+rayon, React stack, v1 scope, package naming, Assimp, prefetch, Tauri desktop shell, video decode backend).
- `spikes/` — validation experiments that gate/inform ADRs (vector-index, headless-render, embedding-models, cross-peer-similarity).
