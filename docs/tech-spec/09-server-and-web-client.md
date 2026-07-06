# 09 — Server & Web Client

Status: **Draft v0.1** · Scope: the `3dam serve` axum service (config, one-port routing, WebSocket transport, embedded assets) and the React + CSS web-client architecture with its WASM/wgpu viewer islands.

This file specifies *how* `3dam serve` is wired and *how* the browser web client is built. It sits under [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.8 (server & web client), §4.2 (embedded vs connected), §7 (stack direction), §6.11 (admin surface), and follows the web-first build order and the hybrid React+CSS + WASM-islands decision in [ROADMAP.md](../ROADMAP.md).

**Borders — read before editing.** This file owns the *plumbing*, not the *semantics* of what flows through it:

- **File [03](03-library-service-and-api.md) owns the API surface** — the `LibraryService` trait, every DTO, the error model, pagination/streaming, and the WebSocket *message* shapes. This file mounts those routes and carries those messages; it does not define them. Where a route or payload is named here it is a reference, not a specification.
- **File [10](10-auth-accounts-and-flags.md) owns auth, feature flags, accounts, and the config↔admin-UI reconciliation.** This file *hosts* the admin UI and *reads* the flag state to decide which routes to mount; it does not define flag semantics, live-vs-restart classification, or the accounts model. Implements alongside [ADR 0004](../adr/0004-feature-flags-admin.md).
- **File [11](11-mcp-server.md) owns the MCP mount** (`rmcp`, the tool/resource/prompt surface, write-gating). This file only notes that `POST /mcp` shares the port and is mounted/unmounted by the same flag machinery. Implements alongside [ADR 0003](../adr/0003-mcp-server.md).
- **Files [06](06-3d-render.md) / [12](12-desktop-gui.md) own the wgpu viewer internals** (render graph, surface sharing, camera). This file owns only how that viewer is *packaged as a WASM island* into the web app and *handed data* from the DOM side.
- **File [02](02-data-model-and-storage.md) owns the server config/flags/accounts store** (the versioned table beside the metadata DB). This file's config *file* seeds it; it does not own its schema.
- **File [15](15-observability-config-testing-packaging.md) owns config precedence** across all roles and the release/packaging mechanics; this file describes only the serve-specific pieces (the serve config file, the rust-embed step) and defers the general precedence rules and CI matrix to 15.

Cross-cutting invariants (fail-soft, non-destructive, headless-friendly) come from [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §1–2 and are assumed throughout.

---

# Part A — The server (`3dam serve`)

`3dam serve` runs the shared `3dam-core` engine as a long-lived **axum/hyper/tokio** service. It is the *connected-mode* backend (PRODUCT_SPEC §4.2): the same engine the GUI/CLI run embedded, now behind one HTTP/WebSocket listener, hosting the web client, the API, and (when enabled) the MCP endpoint — all on **one port**.

## A.1 The serve config file

`3dam serve` is config-driven and headless-friendly (DESIGN_GUIDELINES §1.4): it reads one config file naming what to index and how to listen, then runs unattended. The file is the **declarative seed** for the server's own config/flags store (file 02, [ADR 0004](../adr/0004-feature-flags-admin.md) decision 2) — on start it is loaded and reconciled with the persisted state; **file 10 owns that reconciliation and the live-vs-restart classification.** This file specifies only the file's *shape* and *location*.

- **Location.** Path from `3dam serve --config <path>`; default `$XDG_CONFIG_HOME/3dam/serve.toml` (platform-appropriate; see file 15 for the config-dir resolution shared across roles). Format **TOML** (matches the Rust ecosystem default; the loader is `serde`-driven so JSON is trivially also accepted).
- **Precedence.** CLI flags > env (`3DAM_SERVE_*`) > config file > persisted store defaults > built-in safe defaults. General precedence rules live in file 15; the safe-by-default posture (localhost, no auth, MCP off, read-only) is the built-in floor from [ADR 0004](../adr/0004-feature-flags-admin.md).

### Schema sketch (indicative, not frozen)

```toml
# serve.toml — seeds the server config/flags store (file 02). Flag SEMANTICS: file 10.

[server]
bind          = "127.0.0.1"      # safe default; LAN/0.0.0.0 requires remote-access flag + auth (§10)
port          = 7333
# tls = { cert = "…", key = "…" }  # rustls; tracked as an open question (PRODUCT_SPEC §10)

[web]
# Static web client. Default: serve the assets embedded in the binary (rust-embed, §A.4).
# dev_proxy = "http://localhost:5173"   # dev only: proxy non-API routes to the Vite dev server

# --- Sources to index. Semantics/handlers: files 07 (sources) + 04 (media). ---
[[sources]]
kind = "local"
path = "/mnt/assets/sfx"
watch = true                     # file-watch + auto-rescan deltas (PRODUCT_SPEC §6.1)

[[sources]]
kind = "sftp"
host = "nas.local"
path = "/pool/textures"
# credentials resolved from the OS secret store / a referenced secret, never inline (§A.5, file 10)

[[sources]]
kind = "federated"               # a remote 3DAM peer (file 07); queried, not scanned
url  = "3dam://store.example"

[analysis]
# Which extractors run + watch policy (PRODUCT_SPEC §6.11 "Analysis & watch"). Pipeline: file 05.
extractors = ["audio", "image", "3d-cheap"]   # e.g. defer 3d full-render on a GPU-less host
watch      = true

# --- Feature flags: seed values only. Authoritative semantics + reconciliation: file 10. ---
[flags]
auth              = "off"        # off | anonymous | token | oidc   (one gate: web/API/MCP/federation)
remote_access     = false        # exposing beyond localhost gates on this + an auth mode
network_writes    = false        # read-only to the network by default
mcp               = "off"        # off | read-only | writes   (mounts/unmounts POST /mcp — file 11)
inbound_federation= false        # answer federated queries from peers?
remote_connect    = false        # accept GUI/CLI --connect sessions as a backend?
accounts          = false        # opt-in user accounts (file 10)
```

The `[flags]` block is deliberately thin here: it is a *seed*, and every key's meaning, its reconciliation with runtime admin-UI edits, and whether flipping it applies live or needs a restart are **file 10's** to define ([ADR 0004](../adr/0004-feature-flags-admin.md) decision 2, and its §10 open question on "two writers, one state").

## A.2 Routing layout — one port, one listener

Everything the server exposes is mounted on the **single** axum `Router` bound to `server.bind:server.port`. There is no second daemon and no second port for MCP (PRODUCT_SPEC §6.8, §6.10; [ADR 0003](../adr/0003-mcp-server.md)).

```
                    ┌──────────────────────────────────────────────┐
   HTTP / WS  ───▶  │   axum Router  @  bind:port   (one listener)  │
                    ├──────────────────────────────────────────────┤
                    │  /api/*            HTTP API      → file 03     │  ← LibraryService over HTTP
                    │  /api/ws           WebSocket     → file 03 DTOs│  ← live updates (§A.3)
                    │  /mcp   (POST)     MCP Streamable HTTP → file 11│  ← mounted IFF mcp flag ≠ off
                    │  /admin/api/*      admin API     → file 10     │  ← flags/accounts/audit
                    │  /                 web client (SPA)  → §A.4     │  ← React + CSS + WASM islands
                    │  /assets/*, /*.wasm  static bundle   → §A.4     │
                    └──────────────────────────────────────────────┘
                        one bind addr · one auth surface (file 10)
                        · one TLS cert · one firewall rule
```

- **Layering.** A shared **auth/scope middleware layer** (file 10) wraps `/api`, `/mcp`, and `/admin/api` uniformly — one policy gates the API, agents, federation, and admin at once ([ADR 0004](../adr/0004-feature-flags-admin.md)). This file provides the `tower`/axum layer *seam* (a `from_fn` / `FromRequestParts` extractor injecting the auth context); file 10 fills in the policy.
- **Route mounting is flag-driven.** "Off removes the surface" ([ADR 0004](../adr/0004-feature-flags-admin.md) decision 3): the router is built from the *current flag state*, not statically. When `flags.mcp = "off"`, the `/mcp` route is **not present** in the `Router` (404, not 403). When `remote_connect = false`, connect-backend endpoints are absent. This file exposes a `build_router(flags, services) -> Router` that assembles only the enabled routes; **file 10 decides which flag toggles rebuild the router live vs require a restart.**
- **Route ownership.** `/api/*` and `/api/ws` are file 03's surface; `/mcp` is file 11's; `/admin/api/*` is file 10's. This file owns the `nest()`/`route()` wiring, the fallback static handler, ordering (API prefixes before the SPA fallback), and CORS/compression/tracing `tower` layers.
- **SPA fallback.** Any unmatched GET that is not under an API prefix falls through to the web-client handler (§A.4), which serves `index.html` for client-side routes and the hashed asset for asset paths. API prefixes are checked first so a mistyped `/api/...` returns a JSON 404 (file 03's error DTO), never the HTML shell.

## A.3 WebSocket wiring for live updates

Live updates — scan/analysis progress, newly-ingested assets, job state — push to connected clients over a WebSocket at `/api/ws` (PRODUCT_SPEC §6.8). **This file owns the transport; file 03 owns the message DTOs** (the event envelope, event kinds, and subscription/ack shapes).

- **Transport.** axum's `WebSocketUpgrade`; each accepted socket is a task. Server→client events are serialized file-03 DTOs (JSON by default; the frame codec — text JSON vs binary — is file 03's to pick, this layer carries either).
- **Fan-out.** The engine publishes progress/asset events onto a broadcast bus (`tokio::sync::broadcast`); each socket task subscribes and forwards. This decouples the many-consumer socket layer from the single-producer pipeline (file 14 owns the pipeline; file 03 owns what an event *is*).
- **Backpressure & fail-soft.** A slow/stalled client must not stall the pipeline: per-socket sends are bounded and a lagging subscriber is dropped (with a "resync" hint per the file-03 protocol) rather than blocking the broadcast — the fail-soft rule (DESIGN_GUIDELINES §2) applied to transport.
- **Subscriptions & auth.** The socket carries the same auth context as `/api` (the shared middleware layer, file 10); an anonymous socket sees only what an anonymous client may. Which events a socket receives (all vs scoped to a query/source) is a file-03 protocol concern.
- **Headless note.** The WS layer is optional to the engine — a headless CLI operator polling `/api` never opens one, and the pipeline runs identically whether or not any socket is attached.

## A.4 Embedding the built web assets (`rust-embed`) + dev mode

One binary must serve the whole UI with no separate deploy (ROADMAP §Release). The built React bundle is embedded into the `3dam` binary at compile time via **`rust-embed`**, and a **dev proxy** swaps in a live Vite server during development.

**Production (embedded).**

- A build step compiles the web client (`pnpm build`, §B) into `web/dist/` (JS/CSS/`.wasm`/assets, content-hashed filenames). This runs **before `cargo build`** in the release matrix (ROADMAP §Release: "build the React web client and embed its static assets … *before* `cargo build`").
- A `#[derive(RustEmbed)]` struct with `#[folder = "web/dist/"]` bakes those files into the binary. The static handler resolves a request path against the embedded set, sets `Content-Type` from the extension, emits long-lived immutable cache headers for hashed assets (short/no-cache for `index.html`), and serves `.wasm` with `application/wasm` (required for `WebAssembly.instantiateStreaming`).
- **SPA fallback:** unmatched non-asset GETs return the embedded `index.html` so the client router (§B.2) owns in-app navigation. Result: one self-contained binary is the NAS/workstation deploy — copy it, run `3dam serve`, browse.

**Development (proxy).**

- With `[web].dev_proxy = "http://localhost:5173"` set (or `--dev`), the static/SPA handler is replaced by a **reverse proxy** to the Vite dev server. `/api/*`, `/api/ws`, `/mcp`, `/admin/api/*` stay served by axum in-process; everything else proxies to Vite, giving hot-module-reload and browser devtools against a real engine — the fast iteration loop the web-first order is built around (ROADMAP guiding decision 1). WebSocket upgrades for Vite HMR are proxied through.
- Dev mode is the only mode where the embedded bundle is bypassed; the same `build_router` chooses the proxy vs the `rust-embed` handler based on config, so nothing else in the routing differs.

```
  prod:  GET /              → rust-embed(web/dist/index.html)   (baked into binary)
         GET /assets/x.js   → rust-embed(web/dist/assets/x.js)
  dev:   GET /              → proxy → http://localhost:5173/    (Vite HMR)
         GET /api/*         → axum, in-process   (both modes, always local)
```

## A.5 Startup sequence & headless-friendliness

`3dam serve` startup, in order:

1. **Load config** (§A.1): resolve `--config`, parse TOML, apply precedence (env, CLI).
2. **Open stores & seed flags:** open the metadata DB and the server config/flags store (file 02); **reconcile the config file's `[flags]` seed with the persisted state** (file 10 owns which wins and what applies live). Resolve any source credentials from the OS secret store (never inline in the file — file 10 / PRODUCT_SPEC §6.7).
3. **Bring up the engine:** construct the in-process `LibraryService` (file 03) over `3dam-core`; start the bounded worker pools (file 14).
4. **Scan & watch sources:** for each `[[sources]]` entry, register it (file 07) and kick off an **incremental, non-blocking** scan; enable file-watching where `watch = true` (PRODUCT_SPEC §6.1, DESIGN_GUIDELINES §1.1). The listener comes up immediately — a partial index is queryable while scanning continues, with progress pushed over `/api/ws` (§A.3).
5. **Build the router** (§A.2) from the reconciled flag state — only enabled routes mounted — and **listen** on `bind:port`. Log the effective bind, auth mode, and enabled capabilities so an operator sees the posture at a glance (observability: file 15).

**Headless-friendliness** is a hard requirement (PRODUCT_SPEC §6.8, DESIGN_GUIDELINES §1.4). A serve host — NAS, container, CI runner — often has no display and no GPU:

- **No display server needed.** Nothing in the serve path opens a window; the web UI is HTTP-only, driven from a browser elsewhere.
- **No GPU:** 3D thumbnail/turntable rendering falls back to a **software rasteriser** (lavapipe/llvmpipe) and, where even that is unavailable, degrades — serve metadata/geometry stats and previews rendered elsewhere, defer/skip on-server renders — rather than failing. The **render backend selection and fallback are file [06](06-3d-render.md)'s** ([ADR 0001](../adr/0001-3d-render-backend.md)); serve mode just consumes it and honours the `[analysis].extractors` config to skip GPU-tier work on such hosts.
- **Fully administrable without a browser:** the admin API (`/admin/api/*`, file 10) and the CLI mirror the web admin surface, so a headless operator configures flags and accounts from the config file or CLI (DESIGN_GUIDELINES §5, PRODUCT_SPEC §6.9).

---

# Part B — The web client (React + CSS + WASM islands)

The web client is a **separate front-end codebase** (the sole non-Rust codebase in the workspace — file 00), served by `3dam serve`. Per the ROADMAP's hybrid decision, it is an ordinary **React + CSS** app for all chrome/layout, with **WASM/wgpu islands only** for the interactive 3D viewer and hot render paths (waveforms/thumbnails). It talks to the engine **purely over the serve API** (file 03) — it is always in *connected* mode (PRODUCT_SPEC §4.2), never touching a DB or the engine directly.

Rationale for the split (React/CSS chrome, WASM only for heavy canvas) is settled in [ROADMAP.md](../ROADMAP.md) and PRODUCT_SPEC §7 — not re-argued here. This part specifies the *architecture and packaging*.

## B.1 The three-region workspace as DOM

The desktop workspace (DESIGN_GUIDELINES §3.1) maps to plain DOM, not a canvas:

```
  ┌───────────────┬───────────────────────────────┬───────────────┐
  │  Navigation   │        Browser (centre)        │   Inspector   │
  │  (left)       │  grid  ⇄  table (toggleable)   │   (right)     │
  │  sources ·    │  virtualised, lazy thumbnails  │  large preview│
  │  collections· │  ┌─────┐┌─────┐┌─────┐         │  license badge│
  │  tags · smart │  │thumb││ wave││ 3D  │◀── WASM  │  metadata ·   │
  │  folders      │  └─────┘└─────┘└─viewer island  │  tags · source│
  └───────────────┴───────────────────────────────┴───────────────┘
        <aside>            <main>                        <aside>
```

- **Left / centre / inspector are DOM** — semantic HTML + CSS. The centre grid/table is **virtualised** (windowed rendering) so a 100k+ grid scrolls at 60fps with lazily-fetched thumbnails (DESIGN_GUIDELINES §1.1, §3.1). Grid and table are equal views over the same query, toggle preserves selection+filter.
- **Inspector** prioritises the license badge high (DESIGN_GUIDELINES §3.1), then preview, metadata, features, tags — all DOM, fed by `get_asset` (file 03).
- **All state comes from the API.** Every list, facet, tag edit, and preview is a file-03 call; live progress arrives on the `/api/ws` socket (§A.3). The DOM gets responsive layout, touch, text input, and accessibility for free (ROADMAP guiding decision 2).

## B.2 The React stack (choices framed, not frozen)

Per ROADMAP §Open questions, the exact stack is open. Framing the choices as an implementable starting point (to be confirmed in the web-client build, ROADMAP step 2):

- **Bundler / dev server: Vite.** Fast HMR (the web-first iteration loop, §A.4 dev proxy), first-class WASM + Web Worker support, and a simple `pnpm build → web/dist/` that the `rust-embed` step consumes. `pnpm` as the package manager (matches ROADMAP §Release).
- **Router: a lightweight client-side router** (e.g. React Router, or a minimal file-based router). URL owns view state (current source/collection/filter/selected asset) so views are linkable and back/forward works; the axum SPA fallback (§A.4) serves `index.html` for all such routes.
- **Data fetching / server state:** a query/cache layer (e.g. TanStack Query) over a thin typed API client wrapping file-03 endpoints — caching, background refetch, and request dedup for the grid, with the WebSocket (§A.3) invalidating/patching cached queries on live events. **UI state** (selection, view toggle, filter chips) stays minimal and local/URL-driven; a heavy global store is likely unnecessary.
- **Styling: CSS** (CSS Modules or a small utility layer), dark-first, low-chrome, information-dense per DESIGN_GUIDELINES §4 — one restrained accent for selection/focus/primary. No heavyweight component framework; the design language is dense tables and tight grids, not card-heavy chrome.

These are *candidates to validate*, mirroring the tech-spec convention that stack choices are directional until a spike confirms them.

## B.3 WASM / wgpu viewer islands — packaging & data handoff

The interactive 3D viewer and hot render paths (waveforms, thumbnails) are the parts DOM/CSS can't do well; they are **focused WASM/wgpu components embedded *in* the DOM layout — not a full-page canvas** (ROADMAP decision 2, step 3). The wgpu **viewer internals** are files [06](06-3d-render.md)/[12](12-desktop-gui.md); this file owns only how that Rust code becomes a DOM-embeddable island and how the DOM hands it data.

**Packaging.**

- The viewer is a Rust crate (sharing render code with file 06) compiled to `wasm32-unknown-unknown` and wrapped with `wasm-bindgen` (via `wasm-pack`/`trunk`-style tooling) into an ES module + `.wasm`. wgpu targets **WebGPU** in the browser (WebGL2 fallback where WebGPU is unavailable).
- The `.wasm` and its JS glue are built into `web/dist/` alongside the React bundle, so the **same `rust-embed` step (§A.4) ships them in the one binary**. Vite loads the island module **lazily** (dynamic `import()`), so the heavy WASM is fetched only when a 3D asset (or a waveform view) is actually shown — the grid of thumbnails needs none of it.
- **Not a full-page canvas.** Each island mounts into a specific DOM node (the inspector's viewer slot, a grid cell's waveform). A thin React wrapper component owns a `<canvas>` and the island's lifecycle (init on mount, teardown on unmount) so islands coexist with — and are laid out by — the surrounding DOM/CSS.

**Data handoff (DOM → island).**

```
  React component            wasm-bindgen boundary            wgpu island
  ────────────────           ─────────────────────            ───────────
  fetch model bytes/         init(canvas, opts)               create Surface
  preview via file-03  ──▶   load_model(&[u8]) / set_data ──▶ upload GPU buffers
  camera/props (DOM UI) ──▶  set_camera(...), resize(w,h) ──▶ render frame(s)
  unmount               ──▶  drop()                       ──▶ release Surface/GPU
```

- The **DOM side owns the data**: it fetches model bytes / waveform samples / preview data over the file-03 API (a bytes/preview endpoint) and *hands them to the island* through the wasm-bindgen boundary (`load_model(bytes)`, `set_waveform(samples)`), rather than the island doing its own networking. This keeps the island a pure renderer and keeps all API/auth on the DOM side (§B.1).
- **DOM owns interaction chrome and layout**; the island owns pixels. Camera controls, playback transport, and buttons are DOM (so they get accessibility/touch/keyboard for free); they call into the island (`set_camera`, `play`, `resize`). Islands are handed their canvas node and size from CSS layout and re-`resize()`d on container changes.
- **Exact packaging boundary and the data-handoff API are an open question** (below) shared with the ROADMAP — the shape above is the intended contract for files 06/12 to satisfy on the web target.

## B.4 Hosting the admin / Settings surface

The web client hosts the admin-only **Settings / Administration** area (PRODUCT_SPEC §6.11, DESIGN_GUIDELINES §3.6). **This surface's semantics — the flags, the toggle behaviour, warnings, live-vs-restart, accounts/roles — are file [10](10-auth-accounts-and-flags.md)'s** ([ADR 0004](../adr/0004-feature-flags-admin.md)). This file notes only how the web app *hosts* it:

- It is **plain DOM** riding the `/admin/api/*` endpoints (file 10) — grouped toggle cards, progressive disclosure of sub-options, warn-and-confirm on exposure-increasing toggles, explicit live/restart labels (DESIGN_GUIDELINES §3.6). Being DOM (not canvas) is exactly why it is cheap to build well (ROADMAP step 2).
- **Admin-scoped routing:** the Settings routes are gated client-side by the auth context and, authoritatively, by file 10's server-side admin scope on `/admin/api/*` — once auth is on, this surface is never reachable anonymously ([ADR 0004](../adr/0004-feature-flags-admin.md) consequences).
- It is a **coequal control plane** with the config file and CLI over one persisted state — a convenience, never the only way in (DESIGN_GUIDELINES §3.6). This file does not define that state; file 10 does.

## B.5 Responsive / touch degradation

Desktop/tablet-first; phones graceful-degrade — cheap precisely because the shell is DOM (ROADMAP §Mobile & tablet posture). Concretely:

- **Collapse the three regions** to a single scrollable column at tablet width via CSS breakpoints; navigation and inspector become drawers/sheets over the centre browser rather than always-visible columns.
- **44×44px minimum touch targets** on all controls; `touch-action: manipulation` on canvas-overlay controls (viewer camera, waveform scrub) so they don't fight browser gestures.
- **Stacked panes** (viewer + detail) go vertical with viewport-unit heights and a `min-height` floor so a 3D island stays usable on a phone (ROADMAP).
- Full small-screen polish and real-device testing are tracked as **UX debt, not a v1 gate** (ROADMAP) — the responsive pass is a build-order step (ROADMAP step 4) done before the web client is "done."

---

## Open questions

Carried from [ROADMAP.md](../ROADMAP.md) §Open questions (this file is where they bottom out for the server/web-client area):

- **Exact React stack** — bundler/router/state are framed in §B.2 as Vite + a client router + a query/cache layer, but not locked; to be confirmed during the web-client build (ROADMAP step 2).
- **WASM-island packaging & data handoff** (§B.3) — the precise wasm-bindgen boundary (`load_model` / `set_waveform` / `set_camera` shapes), WebGPU-vs-WebGL2 fallback policy, whether waveform/thumbnail rendering is a WASM island at all or stays a server-rendered preview, and how island lifecycle interacts with the virtualised grid. Depends on files 06/12 landing the shared render crate on the web target.
- **Serve config ⇄ flags-store reconciliation** — *deferred to file 10* ([ADR 0004](../adr/0004-feature-flags-admin.md) §10, "two writers, one state"): which control plane wins on conflict, whether the config file is watched and re-applied, and which flags flip live vs need a restart (this file's `build_router` must know the live set to rebuild routes safely).
- **Dev-proxy vs embedded parity** — ensuring routes/auth behave identically whether the SPA is proxied to Vite or served from `rust-embed`, especially for WebSocket upgrades and `.wasm` MIME/caching.
- **TLS termination** — whether serve terminates TLS in-process (`rustls`, §A.1) or expects a reverse proxy in front; shared with the PRODUCT_SPEC §10 auth/exposure question (file 10).

---

See also: [03-library-service-and-api.md](03-library-service-and-api.md) · [06-3d-render.md](06-3d-render.md) · [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md) · [11-mcp-server.md](11-mcp-server.md) · [12-desktop-gui.md](12-desktop-gui.md) · [15-observability-config-testing-packaging.md](15-observability-config-testing-packaging.md) · [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.8 · [ROADMAP.md](../ROADMAP.md) · [ADR 0003](../adr/0003-mcp-server.md) · [ADR 0004](../adr/0004-feature-flags-admin.md)
