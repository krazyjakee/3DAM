# 01 — Architecture & crates

Status: **Draft v0.1** · Scope: the Cargo workspace — the authoritative crate list and each crate's job, the dependency-direction rules and their CI enforcement, the embedded-vs-connected `LibraryService` wiring, the single-binary role dispatch, and compile-time feature gating.

This file is the low-level companion to [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §4 (system architecture) and §7 (candidate stack), and to [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §2 (architecture guidelines). The product spec establishes *one binary, three roles over a shared `3dam-core` engine*, clients depending on a `LibraryService` seam rather than the engine directly, and a safe-by-default server. This file turns that shape into a concrete crate layout, names the dependency edges that are allowed to exist (and how CI forbids the rest), and gives Rust-ish pseudocode for the two mechanisms that make the shape work: **role dispatch** (how one binary becomes GUI/CLI/server/MCP) and **`LibraryService` selection** (how a front-end picks the in-process engine vs a remote API client). It does not re-decide the render crate boundary — that is [ADR 0002](../adr/0002-3d-render-crate-boundary.md) — nor does it own the `LibraryService` trait's methods and DTOs, which belong to [03-library-service-and-api.md](03-library-service-and-api.md). It fixes the crate *names* the rest of the spec references.

---

## 1. The workspace at a glance

One Cargo workspace, one shipped binary (`3dam`), one non-Rust codebase (the web client, in `web/`, owned by [09-server-and-web-client.md](09-server-and-web-client.md)). Everything else is a library crate that the binary and its siblings compose.

```
3dam/                              (workspace root: Cargo.toml [workspace])
├── crates/
│   ├── 3dam-core/                 pure engine: library model, scan/watch,
│   │                              analysis + convert orchestration, query.
│   │                              NO UI, NO transport, NO GPU.
│   ├── 3dam-api/                  the LibraryService trait + shared DTOs + error
│   │                              model (the seam). Depended on by everyone.
│   ├── 3dam-render/               wgpu renderer (ADR 0002). Owns GPU, not windows.
│   ├── 3dam-media/                MediaHandler trait + audio/image/3D handlers.
│   ├── 3dam-sources/              Source trait + local FS / SFTP / SMB / federated.
│   ├── 3dam-store/                SQLite metadata store + vector index + blob cache.
│   ├── 3dam-client/               API-client LibraryService (HTTP/WS → remote serve).
│   ├── 3dam-server/              axum server: HTTP/WS API, web-asset host, MCP mount,
│   │                              auth, feature-flag/accounts store.
│   ├── 3dam-gui/                  native desktop shell (egui/eframe or Iced).
│   ├── 3dam-cli/                  clap command tree + human/--json/--csv rendering.
│   └── 3dam/                      THE binary. Role dispatch only; ~no logic.
├── web/                           React + CSS web client (non-Rust; file 09).
└── xtask/                         dev-only automation (CI checks, packaging).
```

`3dam-core` is deliberately *not* the crate every front-end imports. Front-ends import **`3dam-api`** (the seam) and one implementation behind it. That inversion — the trait in its own tiny crate — is what lets `3dam-cli` and `3dam-gui` link *either* the embedded engine *or* the API client without either front-end depending on `3dam-core` transitively when connected. See §4.

### Why these boundaries

Each crate is a seam the product spec already drew (DESIGN_GUIDELINES §2):

| Crate | Owns (this spec's file) | Key external deps (PRODUCT_SPEC §7) |
|-------|-------------------------|-------------------------------------|
| `3dam-api` | `LibraryService` trait, DTOs, error taxonomy — [03](03-library-service-and-api.md) | `serde` only (no runtime, no I/O) |
| `3dam-core` | library/query, scan/watch, analysis + convert orchestration — [05](05-analysis-similarity-dedup.md), [08](08-convert-pipeline.md) | `tokio`, `rayon`, `candle`/`ort` (feature-gated) |
| `3dam-render` | wgpu render-to-texture, software raster — [06](06-3d-render.md), [ADR 0002](../adr/0002-3d-render-crate-boundary.md) | `wgpu`, `glam` (**no `winit`**) |
| `3dam-media` | `MediaHandler` trait + format handlers — [04](04-media-handlers.md) | `symphonia`, `image`, `img_hash`, `gltf`, `realfft` |
| `3dam-sources` | `Source` trait, file + federated sources — [07](07-sources-and-federation.md) | `russh`, an SMB crate; federated source uses `3dam-client` |
| `3dam-store` | SQLite schema, migrations, vector index, blob cache — [02](02-data-model-and-storage.md) | `sqlx`/`rusqlite`, `sqlite-vec`/`usearch` |
| `3dam-client` | API-client `LibraryService` impl — [03](03-library-service-and-api.md) | `reqwest`, `tokio-tungstenite` |
| `3dam-server` | axum host, API, MCP mount, auth, flags/accounts — [09](09-server-and-web-client.md), [10](10-auth-accounts-and-flags.md), [11](11-mcp-server.md) | `axum`, `hyper`, `rmcp`, `rustls`, `oauth2`, `argon2`, `keyring` |
| `3dam-gui` | desktop shell, viewer embed — [12](12-desktop-gui.md) | `egui`/`eframe` or `iced`, `egui-wgpu` |
| `3dam-cli` | clap tree, output formats, exit codes — [13](13-cli.md) | `clap` |
| `3dam` | role dispatch (§5) | — (glue only) |

The trait crate `3dam-api` is intentionally the thinnest thing in the workspace: it depends on `serde` and nothing else, so it costs nothing to link everywhere and imposes no transitive weight (no runtime, no GPU, no HTTP) on a consumer that only needs the DTOs.

---

## 2. Dependency graph & direction rules

The whole architecture is one invariant: **dependencies point down, never up.** UI, transport, and GPU may depend on the engine; the engine depends on none of them. This is DESIGN_GUIDELINES §2 ("Layered core… the core depends on neither") and ADR 0002's core-has-no-GPU rule, expressed as a DAG.

```
                         ┌───────────────┐
                         │     3dam      │  (binary: role dispatch)
                         └──┬────┬────┬──┘
              ┌─────────────┘    │    └─────────────┐
              ▼                  ▼                  ▼
        ┌───────────┐      ┌───────────┐      ┌────────────┐
        │ 3dam-cli  │      │ 3dam-gui  │      │3dam-server │
        └─────┬─────┘      └──┬─────┬──┘      └──────┬─────┘
              │               │     │                │
              │               │     ▼                │
              │               │ ┌───────────┐        │
              │               │ │3dam-render│        │   (GUI + server render;
              │               │ │  (wgpu)   │        │    CLI too, feature-gated)
              │               │ └─────┬─────┘        │
              │               │       │              │
    ┌─────────┴───────────────┴───────┼──────────────┴──────────┐
    │                                 │                          │
    ▼           front-ends select a LibraryService impl:         ▼
┌───────────┐                                            ┌───────────────┐
│3dam-client│  (API-client impl: connected mode)         │   3dam-core   │
│ HTTP/WS   │                                            │ (in-proc impl:│
└─────┬─────┘                                            │  embedded)    │
      │                                                  └──┬────┬────┬──┘
      │                                                     ▼    ▼    ▼
      │                                          ┌──────────┐ ┌───────┐ ┌──────────┐
      │                                          │3dam-media│ │3dam-  │ │3dam-store│
      │                                          │(handlers)│ │sources│ │ (SQLite  │
      │                                          └────┬─────┘ └───┬───┘ │ +vec+blob│
      │                                               │           │     └──────────┘
      │                                               ▼           │
      │                                         (3dam-media uses  │
      │                                          3dam-render for  │
      │                                          3D thumbnails)   │
      │                                                           │
      └──────────────────► 3dam-api ◄──────────────────────────┘
             everyone depends on the seam crate (trait + DTOs).
             3dam-api depends on nothing but serde.

  3dam-sources' *federated* source depends on 3dam-client (a peer is queried
  over the same API a connected client uses). 3dam-server depends on 3dam-core
  (embedded engine it serves) + 3dam-render (headless thumbnails).
```

### The rules, stated so CI can check them

1. **`3dam-core` depends on nothing UI, transport, or GPU.** No `wgpu`, `winit`, `egui`, `iced`, `axum`, `hyper`, `reqwest`, `rmcp`, `tao`, or any windowing/HTTP crate anywhere in its dependency tree. (ADR 0002; PRODUCT_SPEC §4.3.) It *may* depend on `3dam-api`, `3dam-media`, `3dam-sources`, `3dam-store`, `tokio`, `rayon`, and the ML runtime.
2. **`3dam-render` owns wgpu but not the window.** It depends on `wgpu` and `glam`; it **must not** depend on `winit` (or `tao`/`eframe`/`iced` windowing). Windowing and the event loop belong to `3dam-gui` (ADR 0002). It does not depend on `3dam-core`'s I/O — only on the pure geometry/camera math types (which live in `3dam-core` per ADR 0002, GPU-free).
3. **`3dam-api` depends on nothing but `serde`.** No runtime, no I/O. It is the seam; if it grows a heavy dependency the seam has leaked.
4. **No front-end depends on `3dam-store` directly.** GUI/CLI/web reach data only through a `LibraryService` (DESIGN_GUIDELINES §1.4: "no front-end reaches past the engine's API"). Only `3dam-core` (and, transitively, `3dam-server`) touches `3dam-store`.
5. **The graph is acyclic.** The one edge that *looks* like a cycle — `3dam-sources`' federated source using `3dam-client` — is not: `3dam-client` depends on `3dam-api` only, not on `3dam-core`, so `core → sources → client → api` is a straight descent.

### Enforcing it in CI

Discipline alone is insufficient (ADR 0002 "Negative/risks"). Three layered guards, run in CI and available locally via `cargo xtask ci`:

- **`cargo-deny` bans, per crate.** A `deny.toml` with `[bans]` entries asserting forbidden crates never appear in a given crate's tree. The load-bearing one:

  ```toml
  # deny.toml — enforced by `cargo deny check bans`
  [bans]
  # 3dam-core (and thus the CLI/server engine path) must never pull in GPU/UI/HTTP.
  deny = [
    { name = "wgpu",  wrappers = ["3dam-render"] },  # only 3dam-render may depend on wgpu
    { name = "winit" },                               # nobody but the GUI shell; never render/core
    { name = "axum",  wrappers = ["3dam-server"] },
    { name = "reqwest", wrappers = ["3dam-client"] },
  ]
  ```

  Run once per crate root (`cargo deny --manifest-path crates/3dam-core/Cargo.toml check bans`) so a violation is attributed to the crate that introduced it.

- **A graph-shape test in `xtask`.** `cargo xtask check-deps` runs `cargo metadata`, builds the dependency DAG, and asserts the allowed-edge whitelist from §2 above — failing on any edge not in the list and on any cycle. This catches an *internal* edge (e.g. someone making `3dam-core` depend on `3dam-server`) that `cargo-deny`'s crate bans would miss.

- **The compile itself.** Because `3dam-api` carries the trait and `3dam-core` implements it, a front-end that tries to reach `3dam-store` directly simply won't have it in scope — the boundary is partly enforced by what each crate re-exports. `3dam-store` types are not re-exported from `3dam-api`.

CI runs `cargo deny check bans`, `cargo xtask check-deps`, `cargo clippy --all-targets --all-features -D warnings`, and the test suite on the full feature matrix (§6). Cross-cutting CI/packaging detail is [15-observability-config-testing-packaging.md](15-observability-config-testing-packaging.md).

---

## 3. The `LibraryService` seam (placement only)

Every front-end talks to one trait, `LibraryService`, living in **`3dam-api`**. Its methods, DTOs, error model, pagination, and streaming are **owned by [03-library-service-and-api.md](03-library-service-and-api.md)** — do not duplicate them here. This file cares only about *where it sits* and *which two crates implement it*:

```rust
// crate: 3dam-api — the seam. Signatures are illustrative; file 03 is authoritative.
#[async_trait]
pub trait LibraryService: Send + Sync {
    async fn search(&self, q: SearchQuery)   -> Result<Page<AssetHit>, Error>;
    async fn get_asset(&self, id: AssetId)   -> Result<Asset, Error>;
    async fn find_similar(&self, r: SimRef)  -> Result<Page<AssetHit>, Error>;
    // …tags, sources, convert jobs, live-update stream — all in file 03.
}
```

Two implementations satisfy it, in two different crates (this is the "embedded vs connected" split of PRODUCT_SPEC §4.2):

- **`EmbeddedLibrary` in `3dam-core`** — wraps the in-process engine (store + handlers + sources). This is the standalone, no-network path.
- **`ApiClient` in `3dam-client`** — implements the same trait by calling a remote `3dam serve` over HTTP/WebSocket. This is the connected path; the web client is always this shape (from JS, not this crate).

The server's job is to expose the *same* surface: `3dam-server` mounts an `EmbeddedLibrary` behind an axum router so the HTTP/WS API mirrors the trait method-for-method (file 03), and the MCP server ([11-mcp-server.md](11-mcp-server.md)) is *also* just an in-process consumer of `EmbeddedLibrary` — peer to the HTTP API, not a subprocess shim (PRODUCT_SPEC §6.10).

---

## 4. Embedded vs connected: front-end wiring

A front-end (GUI or CLI) does not know or care whether the engine is local. It receives a `Box<dyn LibraryService>` at startup, chosen by one selection function. The decision is: **did the invocation ask to connect to a remote server?** (`--connect host:port`, PRODUCT_SPEC §6.8) — if so, connected; otherwise, embedded.

```rust
// crate: 3dam-cli / 3dam-gui share this via a small helper (could live in 3dam-api
// as a constructor module, or a tiny 3dam-frontend crate — see Open questions).

pub enum Backend {
    /// Standalone: engine linked directly in-process. No network.
    Embedded { library_path: PathBuf },
    /// Thin client: talk to a remote `3dam serve` over its API.
    Connected { endpoint: Url, auth: AuthConfig },
}

/// Resolve a LibraryService from how the tool was invoked.
pub async fn open_backend(b: Backend) -> Result<Box<dyn LibraryService>, Error> {
    match b {
        Backend::Embedded { library_path } => {
            // Pulls in 3dam-core → store/handlers/sources. Heavy link, no network.
            let engine = dam_core::EmbeddedLibrary::open(&library_path).await?;
            Ok(Box::new(engine))
        }
        Backend::Connected { endpoint, auth } => {
            // Pulls in 3dam-client only. No engine, no store, no GPU for data ops.
            let client = dam_client::ApiClient::connect(endpoint, auth).await?;
            Ok(Box::new(client))
        }
    }
}
```

Consequences worth stating:

- **The front-end code above the trait is identical in both modes** — the whole point of the seam (DESIGN_GUIDELINES §1.4). A view calls `library.search(q)`; whether that is a function call or an HTTP round-trip is invisible.
- **Link weight differs by mode but not by build.** Both `3dam-core` and `3dam-client` are linked into the shipped binary (so `--connect` works without a reinstall); which one runs is a startup decision. A build that wants a *thin-client-only* binary can drop the embedded engine behind a feature (`embedded-engine`, §6) — but the default binary carries both.
- **Auth lives on the connected path only.** `AuthConfig` (anonymous / token / OIDC) is carried by `ApiClient`; the embedded engine has no auth surface because there is no boundary to guard (auth is a *serve*-side concern — [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md)).
- **Federation is not this.** Adding a peer as a *source* (PRODUCT_SPEC §4.4) happens *inside* the embedded engine via `3dam-sources`' federated source — it reuses `3dam-client` under the hood but is orthogonal to whether *this* front-end is embedded or connected. `--connect` picks your backend; `source add 3dam://…` adds a peer to whatever backend you have. See [07-sources-and-federation.md](07-sources-and-federation.md).

---

## 5. One binary, three (four) roles: dispatch

`3dam` is the only binary. It contains almost no logic — it parses enough of `argv` to pick a role, then hands off to the owning crate. The four roles (PRODUCT_SPEC §4.1, plus MCP stdio from §6.10):

| Invocation | Role | Handed to |
|------------|------|-----------|
| `3dam` (no args, or a GUI-ish arg) | GUI client | `3dam-gui` |
| `3dam <verb> …` (e.g. `scan`, `search`) | CLI client | `3dam-cli` |
| `3dam serve …` | Server | `3dam-server` |
| `3dam mcp …` | MCP stdio server | `3dam-server` (stdio transport) over an embedded engine |

`serve` and `mcp` are, at the CLI-grammar level, just clap subcommands of the CLI tree ([13-cli.md](13-cli.md)) — but they dispatch into `3dam-server`, not into ordinary CLI command handling, because they start long-lived services rather than run-and-exit verbs.

```rust
// crate: 3dam — src/main.rs. The entire binary is essentially this.
fn main() -> ExitCode {
    // Peek at argv to choose a role before doing heavy clap parsing.
    match dam_cli::classify(std::env::args_os()) {
        Role::Gui => dam_gui::run(),                 // default: no verb → desktop shell
        Role::Serve(cfg_args) => dam_server::serve(cfg_args), // long-lived axum service
        Role::Mcp(mcp_args) => dam_server::mcp_stdio(mcp_args), // stdio MCP, embedded engine
        Role::Cli(argv) => dam_cli::run(argv),       // verb-driven, run-and-exit
    }
}
```

```rust
// crate: 3dam-cli — the classifier. Keeps dispatch rules in one place.
pub fn classify(args: impl Iterator<Item = OsString>) -> Role {
    let mut args = args.skip(1).peekable();       // skip argv[0]
    match args.peek().and_then(|s| s.to_str()) {
        None                     => Role::Gui,     // `3dam`  → GUI
        Some("serve")            => Role::Serve(collect(args)),
        Some("mcp")              => Role::Mcp(collect(args)),
        Some(v) if is_verb(v)    => Role::Cli(collect_all()),   // scan/search/similar/…
        Some(_unknown)           => Role::Cli(collect_all()),   // let clap emit the error
    }
}
```

Notes on the dispatch:

- **GUI is the no-verb default** (PRODUCT_SPEC §4.1). A bare `3dam` launches the desktop shell; every other role is reached by a leading token.
- **`serve` and `mcp` route to `3dam-server`, not CLI verbs.** Both start a service. `serve` mounts the HTTP/WS API + MCP-over-HTTP + web host on one port ([09](09-server-and-web-client.md), [11](11-mcp-server.md)); `mcp` runs *only* the MCP tool surface over stdio against an embedded engine (no network, no running server — PRODUCT_SPEC §6.10). They share the tool implementations; only the transport differs.
- **A GUI/CLI role still picks a backend (§4).** After `classify` chooses `Cli` or `Gui`, that front-end parses `--connect` and calls `open_backend`. Role dispatch (which front-end) and backend selection (which `LibraryService`) are two independent decisions.
- **The classifier lives in `3dam-cli`** so the grammar has one owner; the binary crate stays a four-line match and pulls in no parsing logic of its own.

---

## 6. Compile-time feature gating

Optional capabilities are Cargo features so a build can be trimmed — a CI-only CLI need not link wgpu or an ML runtime; a metadata-only serve host need not carry embedding models. Features gate *compilation and dependencies*, distinct from the *runtime* feature flags an operator toggles on a running server ([10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md)). The two must not be confused: a capability the server can turn on at runtime must first be *compiled in* by a feature here.

Proposed feature axes (declared on the crates that own each dependency, then re-exported up through the binary's `Cargo.toml`):

| Feature | Gates | Default? | Lives on |
|---------|-------|----------|----------|
| `media-audio` | `symphonia`, FFT/DSP, `cpal`/`rodio` audio handler | yes | `3dam-media` |
| `media-image` | `image`, `imageproc`, `img_hash` handler | yes | `3dam-media` |
| `media-3d` | `gltf`/FBX/OBJ loaders + 3D handler | yes | `3dam-media` |
| `render` | `3dam-render` (wgpu) — thumbnails + viewer | yes (bin) | `3dam-media`, `3dam-gui`, `3dam-server` |
| `render-software` | software raster (lavapipe/llvmpipe) fallback for headless serve | server default | `3dam-render` ([06](06-3d-render.md)) |
| `ml` | `candle`/`ort` inference for embeddings ([05](05-analysis-similarity-dedup.md)) | yes | `3dam-core` |
| `embedded-engine` | link `3dam-core` into a front-end (embedded backend, §4) | yes | `3dam-cli`, `3dam-gui` |
| `server` | `3dam-server`: axum, auth, flags/accounts | yes (bin) | binary |
| `mcp` | `rmcp` + MCP tool surface ([11](11-mcp-server.md)) | yes | `3dam-server` |
| `source-sftp` | `russh`/`ssh2` SFTP source ([07](07-sources-and-federation.md)) | yes | `3dam-sources` |
| `source-smb` | SMB/Samba source | yes | `3dam-sources` |
| `federation` | federated-peer source (needs `3dam-client`) | yes | `3dam-sources` |
| `auth-oidc` | `oauth2`/`openidconnect` beyond token auth ([10](10-auth-accounts-and-flags.md)) | no | `3dam-server` |

Illustrative manifest wiring (the binary re-exports feature groups so packagers pick a profile):

```toml
# crates/3dam/Cargo.toml
[features]
default   = ["full"]
full      = ["gui", "cli", "server", "all-media", "render", "ml",
             "all-sources", "federation", "mcp"]
all-media = ["media-audio", "media-image", "media-3d"]
all-sources = ["source-sftp", "source-smb"]

# A trimmed CI build:  cargo build -p 3dam --no-default-features \
#     --features "cli,media-image,media-3d"   # no audio, no server, no ML, no GPU

gui    = ["3dam-gui", "embedded-engine", "render"]
cli    = ["3dam-cli"]
server = ["3dam-server", "3dam-server/mcp"]
render = ["3dam-media/render", "3dam-gui?/render", "3dam-server?/render"]
ml     = ["3dam-core/ml"]
# …media-*, source-*, federation forward to the owning crate similarly.
```

Rules for features:

- **A missing media feature degrades, never breaks** (DESIGN_GUIDELINES §6, fail-soft). A binary built without `media-audio` reports audio files as an unhandled type, it does not fail the scan.
- **`render` off ⇒ no wgpu anywhere.** Geometry stats still come from the cheap header-scan tier (PRODUCT_SPEC §6.2) via `3dam-core` pure math (ADR 0002); only thumbnails/turntables/viewer are lost. A GPU-less serve host builds `render` + `render-software` and falls back at runtime ([06](06-3d-render.md)).
- **Server-side runtime flags presuppose their feature.** The admin surface only offers the MCP toggle if the binary was built with `mcp`; likewise OIDC requires `auth-oidc`. Absent-feature capabilities are shown as unavailable, not merely off (detail: [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md)).
- **CI tests a matrix, not just `--all-features`.** At minimum: default, `--no-default-features --features cli`, a metadata-only serve profile, and `--all-features`. This keeps the fail-soft paths honest (§2, and [15](15-observability-config-testing-packaging.md)).

---

## Open questions

- **Frontend-shared helper crate.** `open_backend`/`Backend` (§4) and `classify`/`Role` (§5) are shared by `3dam-cli` and `3dam-gui`. Options: put them in `3dam-cli` (GUI depends on CLI — mild coupling), add a tiny `3dam-frontend` crate, or hang them off `3dam-api`. Leaning `3dam-cli` since it already owns the clap grammar, but a `3dam-frontend` crate is cleaner if the GUI should not link CLI. To settle when [12](12-desktop-gui.md)/[13](13-cli.md) are written.
- **Where ADR-0002's pure geometry/camera math lives.** ADR 0002 places `Mesh`/`Aabb`/camera/pick math in `3dam-core` (GPU-free) so both `3dam-render` and headless logic reuse it. If that math grows large it may warrant its own `3dam-geometry` crate below `3dam-core`; deferred to [06](06-3d-render.md).
- **Default binary size vs thin-client builds.** The default binary links both `3dam-core` and `3dam-client` (§4) so `--connect` needs no reinstall. Whether we also ship an official *thin-client* profile (`--no-default-features --features "gui,cli"` without `embedded-engine`) for size-sensitive distribution is a packaging call — [15](15-observability-config-testing-packaging.md).
- **`3dam-store` visibility.** Rule 4 forbids front-ends depending on `3dam-store`. Whether `3dam-store` is a fully private implementation detail of `3dam-core` (not published, not in the front-end lockfile path) or a workspace crate others *could* import but are CI-forbidden from, affects how strict the guard in §2 must be. Coordinate with [02](02-data-model-and-storage.md).
