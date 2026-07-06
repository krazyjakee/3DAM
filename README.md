# 3DAM — the 3D asset manager

![3DAM](assets/screenshot.png)

`3dam` is a cross-platform, Rust-first **game-asset manager** for **audio, image, and 3D**
assets. It unifies them into one local SQLite catalog and layers **content-based
automation** on top: metadata extraction, thumbnails, embeddings, similarity search,
auto-tagging, and deduplication.

Local-first, non-destructive, and federation-ready. No cloud account, no lock-in — the
catalog is a plain `library.db` on your disk, and your source files are never written to.

## Why

Game projects accumulate thousands of textures, sounds, and models across scattered
folders, drives, and remote shares. Finding "that brick texture" or "the kick drum that
sounds like this one" by filename doesn't scale, and neither does spotting the twelve
near-duplicate variants of the same asset.

3DAM indexes what the files actually *are* — decoded metadata, perceptual hashes, and
embeddings — so search, similarity, and dedup work on content instead of names. Heavy
work runs as background jobs with progress events; the catalog stays responsive whether
it holds a hundred assets or a hundred thousand.

## One binary, four roles

The `3dam` binary dispatches on its first argument — no separate installs, no recompile:

| Invocation        | Role                                                              |
| ----------------- | ---------------------------------------------------------------- |
| `3dam` (no arg)   | **GUI** — native desktop shell (egui) *(stub today — see Status)* |
| `3dam serve`      | **HTTP/WS server** + embedded web client + MCP endpoint          |
| `3dam mcp`        | **MCP server** over stdio                                        |
| anything else     | **CLI** — run-and-exit (`scan`, `search`, `convert`, …)          |

Every role talks to the same engine through one trait, `LibraryService`, so the CLI,
server, GUI, and MCP surface are thin adapters over identical logic — and each can run
either **embedded** (local `library.db`) or **connected** to a remote `3dam serve` with
no code change.

## Status

The engine, CLI, and web client are working; the native GUI and headless renderer are
stubs. Shipped so far (capability phases 1–5 of the [product spec](docs/PRODUCT_SPEC.md)
§9):

- **Catalog** — SQLite catalog with forward-only migrations, sources, jobs, collections, tags.
- **Media depth** — cheap metadata extraction (symphonia / image / glTF), image thumbnails, non-destructive convert/export.
- **Automation** — analyze pass (embeddings + tileability/pHash + auto-tag suggestions), cosine similarity search, exact + near-duplicate dedup.
- **Reach** — SFTP and SMB file sources behind a `FileSource` seam, delta scan + watch/auto-rescan, smart-folder saved queries, export manifests.
- **Server & web** — axum REST + WebSocket API, embedded React web client, runtime feature flags, token auth + scopes, MCP.

Not yet real: the **native egui GUI** (`crates/3dam-gui` is a ~14-line stub — the web
client is the only live UI today, by design) and the **headless wgpu renderer**
(`crates/3dam-render`). Embeddings are **model-free v1** vectors behind the
`EmbeddingSpace` seam; SigLIP/CLAP model-backed extractors are a later feature-gated bump.
See [`docs/ROADMAP.md`](docs/ROADMAP.md) for status against the spec.

## Install

### Build from source

Requires a recent stable Rust toolchain (MSRV **1.85**).

```sh
git clone https://github.com/krazyjakee/3DAM.git
cd 3DAM
cargo build -p dam --release
```

The binary lands at `target/release/3dam`. A native-only build skips the wasm-only viewer
deps; `wasm-pack` (WASM viewer) and `pnpm` (web client) are optional and only needed for a
complete `serve` with the live web UI. Linux GUI/audio builds need the usual system libs
(GTK, xkbcommon, wayland, xcb, ALSA, ssl/pkg-config).

## Quick start

```sh
# Scan a folder into the catalog and wait for the job to finish
3dam scan ~/assets/textures --wait

# Content search, scoped to a media kind, as JSON
3dam search "brick" --media image --json

# Analyze (embeddings + hashes + tag suggestions), then find look-alikes
3dam analyze
3dam similar <asset-id>

# Non-destructive convert — writes to an output dir, never the source
3dam convert <asset-id> --to wav --out ./out
```

Global flags: `--data <dir>` (override the catalog location), `--json` (machine output),
and `--connect <url> --token <t>` to run any command against a remote server instead of
the local catalog.

## CLI

```
3dam scan        <dir> [--wait]              # index a folder into the catalog (a background job)
3dam search      <query> [--media …] [--json] # content search across the catalog
3dam get         <asset-id>                   # show one asset's details
3dam stats                                    # catalog summary
3dam sources     {list, add, remove}          # manage scan roots (local, sftp://, smb://)
3dam collections {…}                          # manage collections + smart folders
3dam convert     <asset-id> --to <fmt> --out <dir>  # non-destructive format convert
3dam analyze                                  # embeddings + hashes + auto-tag suggestions
3dam similar     <asset-id>                    # cosine similarity search
3dam dedup                                    # find exact + near duplicates
3dam tag         <asset-id> …                 # accept/reject/apply tags
3dam export      … --manifest <fmt>            # export assets + manifests (json/csv/sidecar)
3dam jobs / job  [<id>]                        # list jobs / inspect one
3dam admin       {status, flags, flag, token, audit}  # server config + auth (feature-flagged)
3dam serve       [--addr 127.0.0.1:7878]      # HTTP/WS server + web client + MCP
3dam mcp                                       # run as a stdio MCP server
```

Run `3dam --help` for the full tree. Human-readable output is the default; `--json` /
`--csv` are opt-in.

## Server & web client

```sh
3dam serve                    # binds 127.0.0.1:7878 by default
3dam serve --addr 127.0.0.1:7333
```

`3dam serve` exposes a versioned `/api/v1` REST + WebSocket API and serves the embedded
web client — a three-region workspace (Navigation / Browser / Inspector) built with React
19 + TypeScript + Tailwind v4, with a virtualised grid that handles 100k+ rows and
lazy-loaded WASM islands for the 3D-model and audio-waveform previews. Live job progress
and catalog changes stream over WebSocket.

Remote access, auth, accounts, MCP, and analysis are **off-by-default runtime feature
flags** stored in `server.db` — off means the surface disappears. Turn them on through
`3dam admin` or the web Settings panel.

### Web dev

The web client lives in `web/` and uses **pnpm**:

```sh
cd web
pnpm install --frozen-lockfile
pnpm dev          # Vite dev server on :5173, proxies /api → 127.0.0.1:7333
pnpm build        # tsc -b && vite build → web/dist/
```

> **Note:** `3dam serve` binds `127.0.0.1:7878` by default, but the Vite dev proxy targets
> `127.0.0.1:7333`. For local web dev, either run `3dam serve --addr 127.0.0.1:7333` or set
> `VITE_API_TARGET=http://127.0.0.1:7878` before `pnpm dev`.

`web/dist/` is baked into the server via `rust-embed` (debug reads from disk; release
compiles it in).

## Design principles

- **Non-destructive by default.** Convert/export never write into a registered source — output goes to a temp file, then an atomic rename into an output dir.
- **Capabilities are off by default.** Remote access, auth, MCP, network writes, federation, and analysis are runtime flags, not compiled-in behaviour.
- **Heavy work never blocks the UI.** `tokio` owns I/O-bound work; `rayon` owns CPU-bound work; long operations are jobs with progress events, not blocking calls.
- **Fail-soft.** A corrupt asset, an offline source, or an unsupported format degrades to a per-item error — it never aborts a scan or crashes a client.
- **UI parity.** Any user-facing feature must reach both the web and (eventual) native GUI clients, with consistent behaviour and naming.

## Development

```sh
cargo build --workspace                       # native build
cargo test --workspace                        # integration + unit tests
cargo fmt --all --check                       # format check
cargo clippy --all-targets -- -D warnings     # lint

cargo xtask ci                                # the canonical pre-push gate (fmt + clippy + tests + web build)
cargo xtask web                               # build the React client → web/dist/
cargo xtask wasm                              # wasm-pack build the dam-viewer islands
```

The crate layout, seams, and data model are documented in
[`CLAUDE.md`](CLAUDE.md), the numbered deep specs under
[`docs/tech-spec/`](docs/tech-spec/), and the ADRs in [`docs/adr/`](docs/adr/).

## Contributing

Issues and PRs welcome. Good first targets:

- flesh out the native egui GUI (`crates/3dam-gui`) toward web-client parity
- new `FileSource` backends (S3, peers) behind the existing `open_source()` seam
- additional media handlers or convert targets in `dam-media`
- the headless wgpu renderer (`crates/3dam-render`)

The spec is the single source of truth: [`docs/PRODUCT_SPEC.md`](docs/PRODUCT_SPEC.md) §9
is the authoritative roadmap, and [`docs/ROADMAP.md`](docs/ROADMAP.md) tracks status
against it.

## 💖 Support Me
Hi! I’m krazyjakee 🎮, creator and maintain­er of the *NodotProject* - a suite of open‑source Godot tools (e.g. Nodot, Gedis, GedisQueue etc) that empower game developers to build faster and maintain cleaner code.

I’m looking for sponsors to help sustain and grow the project: more dev time, better docs, more features, and deeper community support. Your support means more stable, polished tools used by indie makers and studios alike.

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/krazyjakee)

Every contribution helps maintain and improve this project. And encourage me to make more projects like this!

*This is optional support. The tool remains free and open-source regardless.*

## License

MIT — see [LICENSE](LICENSE).
