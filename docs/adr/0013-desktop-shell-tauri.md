# ADR 0013 — Desktop shell: Tauri webview over the in-process server

Status: **Accepted** · Date: 2026-07-16 · Deciders: 3DAM core
Supersedes: [0005 — GUI toolkit: egui/eframe](0005-gui-toolkit-egui.md) · Related:
[0008 — web client stack](0008-web-client-stack.md), [tech-spec 12](../tech-spec/12-desktop-gui.md),
[tech-spec 09](../tech-spec/09-server-and-web-client.md), [tech-spec 10](../tech-spec/10-auth-accounts-and-flags.md)

## Context

ADR 0005 chose egui/eframe for the native desktop client, and that client was built to
substantive parity with the web UI (`docs/GUI_PARITY.md`). But the parity model it created was
**manual**: every user-facing feature had to be implemented twice (React and egui), tracked in a
checklist, and kept behaviourally in sync by hand (golden rule 1). The web client leads; the egui
shell permanently trailed it, and each new web feature carried a hidden second implementation cost.

Meanwhile the server side already contains a complete native-delivery path: `dam-server` embeds the
built React client (`rust-embed`), serves the full `/api/v1` + WebSocket surface, and the web UI
runs the whole product — browse, search, inspector with WASM 3D/waveform islands, collections,
dedup, admin. The desktop app was the only surface not reusing it.

## Decision

**Replace the egui client with a Tauri 2 webview shell (`crates/3dam-desktop`, package
`dam-desktop`).** The GUI role of the `3dam` binary now:

- **Embedded mode** (bare `3dam`, `--data <dir>`): boots the same axum server `3dam serve` runs —
  in-process, **loopback only, ephemeral port** (`dam_server::serve_desktop`) — and opens a native
  webview on it. The UI is the embedded React client, byte-for-byte.
- **Hosted mode** (`--connect <url> [--token <t>]`, issue #70): the webview navigates straight to
  the remote server, which serves its own embedded client.
- **Auth** rides the front-door design (tech-spec 10) unchanged: with the `Authentication` flag Off
  the local anonymous caller already resolves to owner trust; when it is on, the shell mints a
  per-launch owner-scoped "desktop shell" token in-process (revoking the previous launch's by
  label) and seeds it into the web client's own credential store
  (`localStorage["3dam.server"]`) via a webview initialization script.

Consequences for the architecture rules:

- **Web ↔ native parity is structural, not maintained.** There is one UI codebase (`web/`); the
  desktop shell *is* the web client in a native window. `docs/GUI_PARITY.md` is closed out; golden
  rule 1 becomes "the web client is the UI — keep it excellent on both form factors".
- The desktop shell talks to the engine **over HTTP like every other client** — it no longer holds
  a `Box<dyn LibraryService>` directly. The seam is unchanged; the shell just sits on its served
  side (as the web client always has).
- The interactive 3D viewer and waveform are the **WASM islands** (`dam-viewer`) in the webview —
  the egui-side wgpu-24 viewer, rodio audio path, and DMSH native decoder are retired with the
  crate. `dam-render` (server-side turntable thumbnails + DMSH blob) is unaffected.

> **Dependency-graph amendment (2026-08-01):** the corresponding direct internal edges are
> `dam-desktop → dam-server` and `dam-desktop → dam-frontend`; there is no `dam-gui` package and no
> direct desktop-to-core/client/render edge. The desktop reaches the engine and server-enabled
> optional renderer transitively through `dam-server`. Tech-spec 01 and the `check-deps` whitelist
> are authoritative for the complete shipped graph.

## Alternatives considered

- **Keep egui and continue manual parity** — rejected: double implementation cost per feature,
  permanent trailing gap, and two divergent interaction models to test.
- **Tauri IPC bindings (a TS `LibraryService` over `invoke`)** — rejected for v1 of the shell:
  the HTTP surface already exists, carries auth/scopes/audit, and the webview loads an
  `http://127.0.0.1` origin where Tauri IPC is deliberately unavailable (a security posture, not a
  limitation). Native affordances (folder pickers, tray, deep OS integration) can be added later
  behind Tauri capabilities without changing this decision.
- **wry/tao directly (no Tauri)** — viable and lighter, but Tauri buys the packaging/bundling,
  updater, and capability story we want for distribution (tech-spec 15), for one extra config file.

## Costs / risks

- **WebKitGTK runtime dependency on Linux** (`libwebkit2gtk-4.1`) — new system dep for the GUI
  role; headless servers (`serve`/CLI/MCP) do not load it (the webview links lazily at role
  dispatch, and those roles never reach `dam-desktop`).
- Rendering fidelity/perf now matches the browser (it *is* a browser). The 100k-row virtualised
  grid was already proven in the web client (`@tanstack/react-virtual`, ADR 0008).
- Two concurrent desktop shells on one data dir rotate the same shell token label — the second
  launch signs the first out (accepted edge; same class as today's concurrent-session gotcha).
