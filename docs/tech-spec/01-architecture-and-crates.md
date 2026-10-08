# 01 — Architecture & crates

Status: **Shipped** · Scope: the Cargo workspace, its direct internal dependency graph, role
dispatch, and embedded/connected service wiring.

This file records the architecture that is present in the manifests. The Cargo package names are
`dam-*`, their directories are `crates/3dam-*`, Rust imports use `dam_*`, and the shipped binary is
`3dam` ([ADR 0010](../adr/0010-cargo-package-naming.md)). `Cargo.toml` is authoritative for whether
an edge exists; the whitelist in `xtask/src/main.rs` is the enforcement copy of that graph.

---

## 1. Workspace and ownership

One Rust workspace produces one shipped binary and one React client. The desktop application is a
Tauri shell over that same web client; the removed egui `3dam-gui` crate is historical and is not
part of the current graph ([ADR 0013](../adr/0013-desktop-shell-tauri.md)).

| Directory | Package | Ownership |
|---|---|---|
| `crates/3dam-api` | `dam-api` | `LibraryService`, DTOs, auth context, events, and public errors |
| `crates/3dam-store` | `dam-store` | SQLite catalog, migrations, queries, and optional ANN index |
| `crates/3dam-media` | `dam-media` | media detection, metadata, thumbnails, features, and conversion |
| `crates/3dam-sources` | `dam-sources` | local, SFTP, and SMB file-source implementations and connection types |
| `crates/3dam-render` | `dam-render` | headless wgpu model rendering; no window or event loop |
| `crates/3dam-viewer` | `dam-viewer` | browser WASM viewer islands; GPU dependencies are wasm-target-only |
| `crates/3dam-core` | `dam-core` | embedded engine and `LibraryService` implementation; jobs, federation, and orchestration |
| `crates/3dam-client` | `dam-client` | remote HTTP/WebSocket `LibraryService` implementation |
| `crates/3dam-server` | `dam-server` | axum API, embedded web assets, auth/admin, MCP, and `server.db` |
| `crates/3dam-frontend` | `dam-frontend` | backend selection and role classification shared by CLI and desktop |
| `crates/3dam-desktop` | `dam-desktop` | Tauri shell; boots an in-process server or opens a remote server |
| `crates/3dam-cli` | `dam-cli` | clap command tree, command execution, and output rendering |
| `crates/3dam` | `dam` | `3dam` binary entry point and role dispatch |
| `xtask` | `xtask` | development, dependency, web, and packaging automation |

`dam-api` is intentionally small, but it is not serde-only. Its public async service and DTO
contract directly uses `serde`, `serde_json`, `thiserror`, `uuid`, `async-trait`, and `futures`.
It has no internal `dam-*` dependency, transport implementation, database, UI, or GPU dependency.

---

## 2. The shipped direct graph

The following is the complete allowlist of **direct internal package dependencies**. An asterisk
marks an optional Cargo dependency; all other edges are required manifest dependencies.

```text
dam
├── dam-cli
├── dam-desktop
└── dam-frontend

dam-cli
├── dam-api
├── dam-client
├── dam-core
├── dam-frontend
├── dam-media
└── dam-server

dam-desktop
├── dam-api                 (desktop update IPC DTOs)
├── dam-frontend
└── dam-server

dam-frontend
├── dam-api
├── dam-client
└── dam-core

dam-server
├── dam-api
└── dam-core                (enables `render` and `model-convert`)

dam-core
├── dam-api
├── dam-client              (federated peer transport)
├── dam-media
├── dam-render *            (`render` feature)
├── dam-sources
└── dam-store

dam-store
├── dam-api
└── dam-sources             (connection persistence types; network features disabled)

dam-media   ──► dam-api
dam-sources ──► dam-api
dam-client  ──► dam-api
```

`dam-api`, `dam-render`, and `dam-viewer` have no direct internal dependencies. External crates are
not part of the internal-edge whitelist; their placement is governed by the rules below and the
owning manifest.

### Direct, optional, and transitive rules

- A **direct edge** is a `dam-*` dependency declared by one package manifest. Every such edge must
  appear above and in `ALLOWED_DAM_EDGES`; a new or removed edge is an architecture change, not a
  routine manifest edit.
- An **optional edge** is still a direct edge and is checked as such. Currently only
  `dam-core → dam-render` is optional. The `dam-core/render` feature activates it.
- A **transitive dependency** is inherited through a permitted direct edge and is not itself added
  to the internal whitelist. For example, `dam-desktop` reaches `dam-core`, `dam-render`, and the
  store transitively through `dam-server`; those are not direct desktop edges.
- Cargo feature unification can activate an optional transitive dependency without changing the
  direct graph. `dam-server` enables `dam-core/render`, so the full `dam` binary contains wgpu even
  though a default standalone `dam-core` consumer does not.
- No frontend may add a direct `dam-store` dependency. CLI, desktop, and web functionality cross
  the `LibraryService` boundary; persistence types are not a frontend API.
- The graph must remain acyclic. Cargo rejects dependency cycles; `cargo xtask check-deps` rejects
  direct internal edges outside the exact reviewed whitelist.

### Deliberate engine exceptions

The engine is UI- and server-agnostic, but “no transport or GPU” is not an accurate statement about
every build of its dependency graph. Two reviewed exceptions implement shipped capabilities:

1. **Federation: `dam-core → dam-client` is required.** Federated query fan-out runs in the querying
   engine. A peer is called through the same versioned HTTP API as any connected client, so core
   reuses `ApiClient` instead of growing a second protocol implementation. This gives core an
   outbound transport dependency, but not an inbound server dependency: `dam-client` descends only
   to `dam-api`, so there is no `core ↔ server` cycle. Federation remains an off-by-default runtime
   server capability; the Cargo edge exists in every core build.
2. **Server rendering: `dam-core → dam-render` is optional.** Thumbnail orchestration belongs to the
   engine and calls the renderer behind `dam-core/render`. `dam-server` deliberately enables that
   feature so serve can create 3D turntable thumbnails. A consumer of `dam-core` without `render`
   does not pull wgpu. `dam-render` owns GPU code and remains window-free; no wgpu type is part of
   core's public data/service contract ([ADR 0002](../adr/0002-3d-render-crate-boundary.md)).

These exceptions do not permit `dam-core` to depend on `dam-server`, Tauri/windowing, or frontend
crates. New transport, UI, or GPU edges require an ADR amendment and an explicit whitelist change.

### Enforcement and drift review

`cargo xtask check-deps` reads `cargo metadata`, compares all direct internal dependencies (including
optional status) with `ALLOWED_DAM_EDGES`, and fails for both unreviewed manifest edges and stale
whitelist entries. It runs inside `cargo xtask ci`.

When an internal edge or its optional status changes, the same change must review and update:

1. the affected `Cargo.toml`;
2. `ALLOWED_DAM_EDGES` in `xtask/src/main.rs`;
3. this section and any affected ADR amendment; and
4. the summary crate maps in `CLAUDE.md` and `README.md`.

This four-file review is deliberately lightweight: the detailed graph has one design source, while
summary guidance is explicitly included in review rather than silently generated and forgotten.

---

## 3. `LibraryService` and backend selection

`LibraryService` lives in `dam-api`. It has two Rust implementations:

- `EmbeddedLibrary` in `dam-core` executes against the in-process catalog and workers.
- `ApiClient` in `dam-client` calls a remote `3dam serve` over HTTP/WebSocket.

`dam-frontend::open_backend` selects between those implementations for CLI use. The browser client
uses the HTTP API directly. The Tauri desktop shell does not hold a `LibraryService`: in embedded
mode it starts `dam-server` on a loopback ephemeral port and loads its web client; in connected mode
it navigates to the remote server ([ADR 0013](../adr/0013-desktop-shell-tauri.md)).

Federation is separate from frontend connected mode. `--connect` selects the service used by this
frontend. A federated source makes the selected engine fan out to peer servers, which is why the
engine itself owns the `dam-client` edge ([07](07-sources-and-federation.md)).

---

## 4. One binary, four roles

`dam-frontend::classify` selects the role; the `dam` package performs the final dispatch:

| Invocation | Role | Implementation path |
|---|---|---|
| `3dam` | Desktop | `dam-desktop`; Tauri over local or remote `dam-server` |
| `3dam <verb> …` | CLI | `dam-cli`; embedded or connected `LibraryService` |
| `3dam serve …` | HTTP/WS server | CLI grammar dispatches to `dam-server` |
| `3dam mcp …` | stdio MCP server | CLI grammar dispatches to `dam-server` over an embedded engine |

Role selection is a runtime decision. The shipped `dam` package directly links CLI, desktop, and
frontend dispatch glue; it does not use the old proposed role-level Cargo feature matrix.

---

## 5. Compile-time and runtime gates

The current Cargo features are narrow dependency/build gates:

| Package feature | Effect | Default |
|---|---|---|
| `dam-core/render` | activates optional `dam-render`; enabled by `dam-server` | off in core |
| `dam-core/model-convert` | forwards to `dam-media/model-convert`; enabled by `dam-server` | off in core |
| `dam-core/semantic` | adds Candle, tokenizers, image, and ORT model runtimes | off |
| `dam-media/model-convert` | adds Assimp model export | off in media |
| `dam-sources/sftp`, `smb` | adds the corresponding network source backend | on by default in sources; explicitly enabled by core |
| `dam-store/ann` | adds the optional HNSW similarity index | off |

The web viewer's dependencies are selected with `cfg(target_arch = "wasm32")`, not a Cargo feature.
Server feature flags such as federation, auth, accounts, MCP, network writes, and analysis are
runtime configuration in `server.db`; they do not remove Cargo dependencies from a built binary.
Do not describe runtime flags as compile-time graph boundaries.

---

## 6. Boundary checklist

- Public service and DTO changes begin in `dam-api` and stay synchronized with server, client, and
  `web/src/api/types.ts`.
- Frontends use `LibraryService`/HTTP, never `dam-store` internals.
- `dam-render` may use wgpu but not Tauri, winit, or another window/event-loop owner.
- `dam-core` may use outbound HTTP only through the reviewed `dam-client` federation edge.
- GPU code reaches core only through the optional `dam-render` edge and `render` feature.
- Any direct internal graph change follows the four-file review in §2 before the whitelist changes.
