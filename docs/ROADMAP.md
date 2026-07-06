# 3DAM — Roadmap

Status: **Draft v0.1** · Scope: build order and delivery strategy. This is a
high-level roadmap — the *order* we build things and *why*, not detailed technical
design. It sits alongside the product spec's §9 phasing (which describes capability
maturity) and records the sequencing decisions that phasing leaves open.

See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) for what 3DAM is, [MISSION.md](MISSION.md) for
why, and [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md) for the rules.

---

## Guiding decisions

Two decisions shape the order of work:

1. **Web UI before the desktop GUI.** Build the browser-based web client first, then the
   native egui desktop app. A web front end has a much faster iteration loop — hot reload,
   browser devtools, no native rebuild/repackage cycle — so we get the layout, interaction,
   and workflow right against real libraries sooner. The desktop GUI follows once those
   patterns are settled, reusing the same engine and API.

2. **A hybrid web front end, not egui-in-WASM for everything.** The web client is its own
   codebase — an ordinary **React + CSS** app served by `3dam serve` — and uses **WASM only
   for the parts that genuinely need it** (the interactive 3D viewer, and any hot rendering
   like waveform/thumbnail work). Everything else — browse grid, search, tags, panels,
   navigation — is plain DOM. This mirrors the approach proven in the sibling *moghub*
   project: React/CSS for chrome and layout, WASM reserved for the heavy canvas.

   Rationale: a DOM front end gets responsive layout, touch handling, accessibility, and
   text input essentially for free, where an all-egui-WASM canvas would have to re-implement
   each of those by hand. It also keeps mobile/tablet degradation cheap (see below).

These supersede the earlier open question in spec §10 ("shared Rust→WASM view code vs a
separate web UI") in favour of the separate-web-UI path, and reorder spec §9 so the
web client leads rather than trailing the desktop GUI.

## Indicative build order

Coarse sequence — each stage assumes the engine work it depends on from spec §9.

1. **Engine + API core.** `3dam-core` (schema, local scan, SQLite store, search) behind a
   `LibraryService` boundary, exposed early over the `3dam serve` HTTP/WS API. The CLI rides
   the same API so it is usable from day one. *(First vertical slice landed — see **Progress** below.)*
2. **Web client (React + CSS).** The three-region workspace as a DOM app against the serve
   API: browse grid, search, facets, tags, previews. Fast feedback loop lives here. The
   admin-only **Settings / Administration** surface — feature flags (MCP on/off, auth mode,
   network access, analysis), and later user accounts — lands here as plain DOM riding the same
   API (spec §6.11); it is cheap to build well precisely because it is DOM, not canvas.
3. **WASM viewer islands.** Drop the interactive 3D viewer (and any heavy render paths) into
   the web client as focused WASM/`wgpu` components embedded in the DOM layout — not a
   full-page canvas.
4. **Responsive + touch pass.** Make the web client degrade gracefully to tablet/phone
   widths (see below) before it is considered done.
5. **Desktop GUI (egui).** The native app, reusing the settled interaction patterns and the
   same engine/API. Parity with the web client's core workflows.
6. **Everything after** follows spec §9 from automation depth through federation and scale.
   Opt-in **user accounts and roles** arrive with the federation & auth phase (spec §9 step 6),
   layered on the same auth surface the feature flags already gate.

### Progress

**2026-07-06 — stage 1, first vertical slice.** The Cargo workspace now exists (all crates from
[tech-spec 01](tech-spec/01-architecture-and-crates.md) scaffolded; packages are named `dam-*`
and the binary is `3dam`, since Cargo forbids a leading digit — [ADR 0010](adr/0010-cargo-package-naming.md)).
Working end-to-end:

- **Embedded engine** — `dam-core`'s `EmbeddedLibrary` over a SQLite `dam-store` (schema V1),
  with a background **scan** job (walk → BLAKE3 hash → extension-based media detection → upsert),
  live progress/asset events, and cursor-paginated **search** + faceted filters + library stats.
- **`3dam serve`** — an axum HTTP/WS server mirroring the `LibraryService` surface under `/api/v1`.
- **CLI parity** — `3dam scan`/`search`/`sources`/`stats`/`get`, riding *either* the embedded
  engine *or* a remote server via `--connect` — byte-identical results prove the seam holds.

Not yet built (next up the stage-1 tail and into stage 2): the analysis pipeline
(embeddings/similarity/auto-tag), tag/collection/license *writes*, convert, full-text search,
auth/flags/accounts, and the React web client. The `dam-render` (wgpu) and `dam-gui` (egui) crates
are scaffolded stubs pending their later stages.

## Mobile & tablet posture

Desktop/tablet-first; phones are graceful-degrade, not a separate product. Because the web
client is DOM-based this is cheap, and we follow moghub's pattern:

- Collapse the three-region workspace to a single scrollable column at tablet width.
- **44×44px minimum touch targets**; `touch-action: manipulation` on canvas overlay controls.
- Stack side-by-side panes (e.g. viewer + detail) vertically with viewport-unit heights and
  a `min-height` floor so the 3D canvas stays usable.
- Treat full small-screen polish and real-device testing as tracked UX debt, not a v1 gate.

## Release & distribution

Cross-platform releases follow the pattern proven in the sibling *mogen* project: a
tag-triggered GitHub Actions matrix that builds native binaries, packages an installer per
OS, and publishes one checksummed GitHub Release. Adapted to 3DAM's shape.

- **Trigger:** push a `v*` tag (plus manual `workflow_dispatch`). `contents: write` so the
  job can create the release.
- **Build matrix** (`fail-fast: false`), one runner per target:
  - `x86_64-unknown-linux-gnu` on `ubuntu-22.04`
  - `x86_64-apple-darwin` on `macos-*-intel`
  - `aarch64-apple-darwin` on `macos-latest`
  - `x86_64-pc-windows-msvc` on `windows-latest`
- **Toolchain & cache:** `dtolnay/rust-toolchain` for the target triple, `Swatinem/rust-cache`.
- **Web client first.** Unlike mogen, a release must **build the React web client (Node/pnpm)
  and embed its static assets into the `3dam` binary** (e.g. `rust-embed`) *before*
  `cargo build`, so the single binary serves the web UI with no separate deploy.
- **Native build:** one `cargo build -p 3dam --release --locked --target <triple>` — one
  binary, three roles, so packaging is simpler than mogen's two-binary case.
- **Linux system deps** are heavier than mogen's: the egui/eframe stack (GTK, xkbcommon,
  wayland, xcb, pkg-config, openssl) **plus** 3DAM's own — `wgpu`/Vulkan, audio
  (`libasound2-dev` for `cpal`), and image/3D loaders; the headless-serve software rasteriser
  (Mesa lavapipe/llvmpipe) is a runtime concern for the serve target, not the build.
- **Per-OS packaging** (mirrors mogen):
  - Linux — `tar.gz` of the binary + `.deb` via `cargo-deb`.
  - Windows — `.zip` + `.msi` via `cargo-wix`, with a generated `.ico`.
  - macOS — a `.app` bundle (iconset via `sips`/`iconutil`, `Info.plist`) wrapped in a
    `.dmg` via `hdiutil`.
- **Publish:** collect artifacts, generate `SHA256SUMS`, create the release with
  `softprops/action-gh-release`.
- **Not carried over from mogen:** the itch.io/butler push (game-store specific). 3DAM's
  dev-facing channels (Homebrew tap, winget, AUR, `cargo-binstall`) are TBD — GitHub Releases
  is the primary channel for v1.

The actual `.github/workflows/release.yml` lands once the build is release-worthy. The `3dam`
crate now exists (stage 1, above), but it is pre-web-client and pre-packaging; the workflow is a
fast-follow rather than something to commit before there is a shippable binary to tag.

## Open questions

Most items here were decided on 2026-07-06 — [ADR 0008](adr/0008-web-client-stack.md) (web stack)
and [ADR 0009](adr/0009-v1-scope-decisions.md) (the v1-scope tail).

- ~~Exact React stack.~~ **Decided: React + TypeScript + Tailwind** on Vite/pnpm
  ([ADR 0008](adr/0008-web-client-stack.md)). WASM-island packaging/handoff decided in
  [ADR 0009 §9](adr/0009-v1-scope-decisions.md) (`wasm-pack`, WebGPU + WebGL2 fallback, DOM owns data).
- ~~**Code signing & notarization.**~~ **Decided: unsigned for v1** — accept the Gatekeeper /
  SmartScreen warnings (as mogen does); revisit signing/notarization post-v1.
- ~~Distribution channels beyond GitHub Releases.~~ **Decided** ([ADR 0009 §10](adr/0009-v1-scope-decisions.md)):
  **GitHub Releases is the sole v1 channel;** Homebrew tap + `cargo-binstall` are fast-follow
  post-v1; winget/AUR are community-driven.
- ~~View-logic sharing web ↔ desktop.~~ **Decided** ([ADR 0009 §9](adr/0009-v1-scope-decisions.md)):
  each frontend owns its own presentation over the shared API in v1; no shared view crate.
- ~~Whether the desktop GUI embeds a webview.~~ **Decided: no** — the desktop GUI stays fully
  native (egui) in v1 ([ADR 0009 §9](adr/0009-v1-scope-decisions.md)).

---

See also: [PRODUCT_SPEC.md](PRODUCT_SPEC.md) · [MISSION.md](MISSION.md) · [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md)
