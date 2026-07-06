# CLAUDE.md — 3DAM

Guidance for AI agents (and humans) working in this repository. Read this before making changes.

## What 3DAM is

3DAM is a cross-platform, Rust-first **game-asset manager** for **audio, image, and 3D** assets. It unifies them into one local SQLite catalog and layers **content-based automation** on top: metadata extraction, thumbnails, embeddings, similarity search, auto-tagging, and deduplication. It is local-first, non-destructive, and federation-ready.

**One binary, four roles.** The `3dam` binary dispatches on `argv[1]`:
- (no arg) → **GUI** (native desktop; currently a stub — see *Reality check*)
- `serve` → **HTTP/WS server** + embedded web client + MCP endpoint
- `mcp` → **MCP server** over stdio
- anything else → **CLI** (run-and-exit; `scan`, `search`, `convert`, …)

Every role talks to the same engine through one trait, `LibraryService`, so the CLI, server, GUI, and MCP surface are thin adapters over identical logic.

---

## Golden rules

1. **UI parity — web *and* native GUI.** 3DAM ships two user-facing clients and they must stay in sync:
   - **Web UI** — `web/` (React 19 + TypeScript + Tailwind v4), served by `3dam serve`.
   - **Native GUI** — `crates/3dam-gui` (`dam-gui`, egui/eframe per [ADR 0005](docs/adr/0005-gui-toolkit-egui.md)).

   Any user-facing feature, layout, or interaction change must be implemented in **both** clients, with consistent behaviour, naming, and workflow. Do not consider a UI task done until both are covered. **Reality check:** the native GUI is currently a ~14-line stub (`crates/3dam-gui/src/lib.rs` just prints "not implemented yet"); the web client is the only live UI today, by design (web-first phasing). So in practice: implement the web change now, and in the same breath either implement the egui equivalent or explicitly record the gap so the GUI reaches parity rather than silently trailing web-only. Never let a user-facing capability exist in one client with no plan for the other.

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
| **EmbeddingSpace** (data-level) | `embedding` table, `dam-store` schema V3 | model-free v1 vectors; SigLIP/CLAP later | `space_id` = `model@version+media+dim+metric`. Isolates embedding generations so a model bump invalidates only its slice. Powers `similar` / dedup. Model-backed extractors are a later feature-gated bump behind this seam. |

### Crate map (`crates/`)

Packages are named `dam-*` (Cargo forbids leading digits); directories are branded `3dam-*`; the binary is `3dam` — see [ADR 0010](docs/adr/0010-cargo-package-naming.md). Imports use snake_case (`dam_core`, `dam_api`).

| Directory | Package | Role | Notes |
|---|---|---|---|
| `3dam-api` | `dam-api` | The seam: `LibraryService` trait, DTOs, `AuthContext`, `Scope`/`Scopes`, events | Foundational. serde-only, no other `dam-*` deps. |
| `3dam-store` | `dam-store` | Synchronous SQLite catalog (`library.db`); schema + migrations | Private to `dam-core`. Never exposed to frontends. |
| `3dam-media` | `dam-media` | `detect()` / `extract_metadata()` (cheap tier) / `render_thumbnail()` (images) / convert / feature extraction | symphonia (audio), image (image), gltf (3D). |
| `3dam-sources` | `dam-sources` | `FileSource` + local/SFTP/SMB; `SourceConnection` | SFTP/SMB behind cargo features. |
| `3dam-core` | `dam-core` | **The engine.** `EmbeddedLibrary` impls `LibraryService`; scan/analyze/convert/export jobs, watch/auto-rescan, events, thumbnail cache | Pure logic — no UI, transport, or GPU. |
| `3dam-client` | `dam-client` | `ApiClient` impls `LibraryService` over HTTP/WS | The "connected" backend (CLI/GUI `--connect`). |
| `3dam-server` | `dam-server` | axum server: `/api/v1` REST + WS, embedded web client (`rust-embed`), auth/admin, MCP mount; owns `server.db` (tokens, flags, audit) | Wraps an `EmbeddedLibrary`. |
| `3dam-frontend` | `dam-frontend` | `Backend`/`open_backend` + `Role`/`classify` dispatch glue | Tiny; shared by CLI + GUI. |
| `3dam-cli` | `dam-cli` | clap command tree; also exports `serve()` and `mcp()` entry points | Human output by default, `--json`/`--csv` opt-in. |
| `3dam-gui` | `dam-gui` | Native desktop shell (egui/eframe) | **Stub today.** Follows the web client. |
| `3dam` | `dam` | Binary entrypoint: `classify(argv)` → dispatch | GUI is sync; CLI/serve/mcp run on tokio. |
| `3dam-render` | `dam-render` | Headless wgpu render-to-PNG + GUI viewer surface | **Stub today** (native render phase). |
| `3dam-viewer` | `dam-viewer` | Browser WASM/wgpu viewer islands (3D model + audio waveform) | `cdylib`+`rlib`; GPU deps only under `cfg(target_arch = "wasm32")`, so native `cargo build` skips them. |

### Dependency flow

`dam-api` is the root (serde-only). `dam-media`/`dam-sources` are leaf handlers → `dam-store` → `dam-core` (engine). `dam-client` implements the same trait over HTTP. `dam-server` wraps `dam-core`. `dam-frontend` glues embedded+connected; `dam-cli` sits on `dam-frontend`+`dam-server`; `dam` dispatches into cli/gui. `dam-render`/`dam-viewer` are independent GPU crates.

### Data & storage

- Data dir: platform default (see `dam-core/src/paths.rs`), overridable with `--data`.
- `library.db` — the catalog (assets, sources, jobs, collections, tags, embeddings). Owned by `dam-store`, private to the engine.
- `server.db` — server config, tokens, feature flags, audit log. Owned by `dam-server`.
- **Schema is forward-only** (`PRAGMA user_version`), currently **V3** (V1 catalog → V2 audio codec/container → V3 `embedding` table). A DB from a *newer* schema is rejected rather than downgraded. When you change the schema, add a numbered migration in `dam-store/src/schema.rs` and bump the version — never edit an existing migration.

---

## Developer workflow

MSRV **1.85**, edition 2021, resolver 2. No `.cargo/config.toml`, `rustfmt.toml`, `clippy.toml`, or `deny.toml` — Rust defaults apply.

### Build & run

```bash
cargo build --workspace                 # native build (skips wasm-only deps in dam-viewer)
cargo build -p dam --release            # the 3dam binary

# Run roles via cargo (or the built binary):
cargo run -p dam -- serve               # HTTP/WS server + web client (binds 127.0.0.1:7878)
cargo run -p dam -- scan <dir> --wait   # CLI: scan a folder and wait for the job
cargo run -p dam -- search "brick" --media image --json
cargo run -p dam -- --connect http://host:7878 --token <t> search "kick"
cargo run -p dam --                     # GUI role (prints the not-implemented stub today)
```

Key CLI verbs (verb-noun, per tech-spec 13): `scan`, `search`, `get`, `stats`, `sources {list,add,remove}`, `collections {…}`, `convert`, `analyze`, `similar`, `dedup`, `tag`, `export`, `jobs`/`job`, `admin {status,flags,flag,token,audit}`, plus `serve` / `mcp`. Global flags: `--connect`, `--token`, `--data`, `--json`.

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

`wasm-pack` (WASM viewer) and `pnpm` (web) are optional for a native-only Rust build — xtask skips them gracefully — but required for a complete `serve` with a real web client. Linux GUI/audio builds need the usual system libs (GTK, xkbcommon, wayland, xcb, ALSA, ssl/pkg-config; see `.github/workflows/release.yml`).

---

## Web UI shape (for parity work)

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
- **The native GUI and native renderer are stubs.** `dam-gui` and `dam-render` are placeholders. Don't assume egui code exists; parity work there means *building* it, not editing it.
- **Embeddings are model-free in v1.** Similarity/dedup work off a v1 vector behind the EmbeddingSpace seam. SigLIP (image/3D, candle) and CLAP (audio, ort/ONNX) are a later feature-gated bump — see `spikes/embedding-models/` and [ADR 0006](docs/adr/0006-inference-candle.md).
- **Newer-schema DBs are rejected.** If you bump the schema and then run an older binary against that DB, it refuses to open by design.
- **Release CI is a scaffold.** `.github/workflows/release.yml` is `workflow_dispatch`-only until the app is production-ready; it is not wired to tag pushes yet.

---

## Where to read more

- `docs/MISSION.md` — vision, principles, success criteria.
- `docs/PRODUCT_SPEC.md` — product spec; **§9 is the authoritative roadmap**.
- `docs/ROADMAP.md` — status against §9 (phases 1–5 shipped; 6 federation/auth & 7 polish/scale later; 8 beyond v1).
- `docs/DESIGN_GUIDELINES.md` — dark-first, information-dense UI rules (governs both clients).
- `docs/tech-spec/00`–`15` — numbered deep specs. Crate descriptions cite them (e.g. 01 architecture, 03 LibraryService/API, 05 analysis, 09 server/web, 12 GUI, 13 CLI, 14 concurrency).
- `docs/adr/0001`–`0010` — decisions (render backend, crate split, MCP, feature flags, egui, candle, tokio+rayon, React stack, v1 scope, package naming).
- `spikes/` — validation experiments that gate/inform ADRs (vector-index, headless-render, embedding-models, cross-peer-similarity).
