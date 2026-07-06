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
   the same API so it is usable from day one.
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

The actual `.github/workflows/release.yml` lands once the `3dam` crate exists; committing it
before there's anything to build would only produce a workflow that fails on first tag.

## Open questions

- Exact React stack (bundler, router, state) and how the WASM viewer islands are packaged
  and handed data from the DOM side.
- **Code signing & notarization:** unsigned macOS `.dmg`/Windows `.msi` trip Gatekeeper and
  SmartScreen (mogen ships unsigned). Whether v1 pays for an Apple Developer ID + notarization
  and a Windows signing cert, or accepts the warnings.
- Distribution channels beyond GitHub Releases (Homebrew, winget, AUR, `cargo-binstall`) —
  which, and when.
- How much view logic, if any, is worth sharing between the web client and the egui desktop
  GUI once both exist, versus letting each own its presentation over the shared API.
- Whether the desktop GUI ever embeds the web client (webview) for any surfaces, or stays
  fully native.

---

See also: [PRODUCT_SPEC.md](PRODUCT_SPEC.md) · [MISSION.md](MISSION.md) · [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md)
