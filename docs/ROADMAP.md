# 3DAM — Roadmap

Status: **Draft v0.3** · Scope: build status and delivery strategy.

This page tracks **build status against the capability phasing defined in
[PRODUCT_SPEC.md](PRODUCT_SPEC.md) §9 — the single source of truth** for what 3DAM builds and in
what order. It layers progress notes and delivery detail on top of that phasing; it does **not**
define a second, competing sequence.

> An earlier version of this doc carried a parallel *front-end build order* with its own "stage"
> numbers running alongside the capability phases. That second axis was the source of confusion and
> has been removed. There is now **one set of numbers — the capability phases below (from spec §9)**.
> Front-end work (web client, viewer islands, responsive pass, desktop GUI) is **web-first** and is
> referred to **by name**, never renumbered as separate stages — see *Front-end sequence* below.

See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) for what 3DAM is, [MISSION.md](MISSION.md) for why, and
[DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md) for the rules.

## Capability phases

Straight from [PRODUCT_SPEC.md](PRODUCT_SPEC.md) §9 (capability maturity — *what works*), annotated
with current status. The spec is authoritative; this table just adds where we are.

<table class="phasetable">
<thead><tr><th>#</th><th>Phase (spec §9)</th><th>Status</th></tr></thead>
<tbody>
<tr><td>1</td><td><strong>Foundation</strong> — schema, local scan, SQLite store, grid/table browse + text search, CLI <code>scan</code>/<code>search</code> <span class="sub">Embedded engine, <code>3dam serve</code> API, and CLI parity landed.</span></td><td><span class="pill done">Shipped</span></td></tr>
<tr><td>2</td><td><strong>Media depth</strong> — per-type decode + preview (waveform, image, 3D), thumbnail cache, convert pipeline (CLI-first) <span class="sub">Cheap per-type metadata, server image thumbnails + cache, interactive audio/3D islands, and the CLI convert pipeline all landed; deeper codec/format coverage staged.</span></td><td><span class="pill done">Shipped</span></td></tr>
<tr><td>3</td><td><strong>Automation</strong> — feature extraction + embeddings, similarity search, auto-tag/categorise, dedup, review UX <span class="sub">The differentiator. Spikes done (embedding models, cross-peer similarity, vector index).</span></td><td><span class="pill next">Next</span></td></tr>
<tr><td>4</td><td><strong>Reach</strong> — SFTP + SMB sources, watch/auto-rescan, smart folders, export/manifests, CLI/GUI parity</td><td><span class="pill todo">Planned</span></td></tr>
<tr><td>5</td><td><strong>Server &amp; web</strong> — <code>LibraryService</code> boundary, <code>3dam serve</code>, <code>3dam mcp</code>, <code>--connect</code>, web client, feature flags + Settings, basic auth <span class="sub">Boundary, serve, <code>--connect</code>, and web client landed early; flags/MCP/auth remain.</span></td><td><span class="pill active">Partly done</span></td></tr>
<tr><td>6</td><td><strong>Federation &amp; auth</strong> — 3DAM-server source, federated fan-out + re-rank, cross-peer vector similarity, OIDC/OAuth2, opt-in accounts &amp; roles</td><td><span class="pill later">Later</span></td></tr>
<tr><td>7</td><td><strong>Polish &amp; scale</strong> — performance at 1M assets, accessibility, packaging/distribution for all three OSes</td><td><span class="pill later">Later</span></td></tr>
<tr><td>8</td><td><strong>Future — asset networks</strong> — peer relay/mesh, instance discovery, trust/reputation (beyond v1)</td><td><span class="pill later">Beyond v1</span></td></tr>
</tbody>
</table>

<div class="legend">
  <span><span class="pill done">Shipped</span> working end-to-end</span>
  <span><span class="pill active">In progress</span> actively being built</span>
  <span><span class="pill next">Next</span> immediately queued</span>
  <span><span class="pill todo">Planned</span> scoped, not started</span>
  <span><span class="pill later">Later</span> post-core / beyond v1</span>
</div>

> Because the serve API and web client were pulled forward (part of phase 5), some phase-5 work
> landed before phases 2–4 are fully polished. That is deliberate — see *Front-end sequence*. The
> phase numbers stay tied to capability, not to the order individual pieces happened to ship.

## Front-end sequence (web-first)

Two decisions shape the order of front-end work. Neither introduces a competing phase numbering —
they sit *inside* the capability phases above.

1. **Web UI before the desktop GUI.** Build the browser-based web client first, then the native
   egui desktop app. A web front end has a much faster iteration loop — hot reload, browser
   devtools, no native rebuild/repackage cycle — so the layout, interaction, and workflow settle
   against real libraries sooner. The desktop GUI follows, reusing the same engine and API.
2. **A hybrid web front end, not egui-in-WASM for everything.** The web client is its own codebase
   — an ordinary **React + CSS** app served by `3dam serve` — using **WASM only where it is genuinely
   needed** (the interactive 3D viewer and hot render paths like waveforms). Everything else — browse
   grid, search, tags, panels, navigation — is plain DOM, which gets responsive layout, touch,
   accessibility, and text input essentially for free.

Rationale and the frozen choices live in [ADR 0008](adr/0008-web-client-stack.md) (React + TypeScript
+ Tailwind) and [ADR 0009 §9](adr/0009-v1-scope-decisions.md) (WASM-island packaging, WebGPU + WebGL2
fallback, each frontend owns its presentation, native desktop GUI — no webview), and in
[PRODUCT_SPEC.md](PRODUCT_SPEC.md) §7. The front-end increments, **named not numbered**, are:

- **Engine + serve API core** — `3dam-core` behind a `LibraryService` boundary, exposed over the
  `3dam serve` HTTP/WS API; the CLI rides the same API. *(Part of phases 1 & 5.)*
- **React web client** — the three-region workspace (browse grid, search, facets, tags, previews)
  as DOM against the serve API; the admin **Settings / Administration** surface (feature flags, and
  later accounts) lands here too, cheap because it is DOM. *(Serves phases 2 & 5.)*
- **WASM viewer islands** — the interactive 3D viewer and waveform render as focused `wgpu`
  components embedded *in* the DOM layout (the `dam-viewer` crate → `web/src/islands/`), not a
  full-page canvas. *(Serves phase 2.)*
- **Responsive + touch pass** — the web client degrades gracefully to tablet/phone widths (see
  *Mobile & tablet posture*) before it is considered done.
- **Desktop GUI (egui)** — the native app, reusing the settled interaction patterns and the same
  engine/API. *(Phase 5 / spec §9 GUI parity.)*

### Progress

<div class="log">
<p class="when">2026-07-06</p>
<h4>Foundation — first vertical slice <span class="pill done">Shipped</span></h4>
<p>The Cargo workspace exists (all crates scaffolded; packages named <code>dam-*</code>, binary
<code>3dam</code> — <a href="adr/0010-cargo-package-naming.md">ADR 0010</a>). Working end-to-end: the
<strong>embedded engine</strong> (<code>EmbeddedLibrary</code> over a SQLite store, background scan job,
cursor-paginated search + facets + stats), <strong><code>3dam serve</code></strong> (axum HTTP/WS mirroring
the <code>LibraryService</code> surface), and <strong>CLI parity</strong> (<code>scan</code>/<code>search</code>/<code>sources</code>/<code>stats</code>/<code>get</code>
over either the embedded engine or a remote server via <code>--connect</code> — byte-identical results prove
the seam holds).</p>
</div>

<div class="log">
<p class="when">2026-07-06</p>
<h4>Web client, viewer islands, responsive pass — first cut <span class="pill done">Shipped</span></h4>
<p>The browser front-end exists as a <code>web/</code> codebase — <strong>React + TypeScript + Tailwind on
Vite/pnpm</strong> (<a href="adr/0008-web-client-stack.md">ADR 0008</a>), the sole non-Rust codebase —
talking to the engine purely over the serve API. It delivers the three-region workspace as DOM:
<strong>Navigation</strong> (facets + sources + add-source), a <strong>virtualised</strong> grid⇄table
<strong>Browser</strong> with search/sort/infinite-scroll and URL-driven view state, and an
<strong>Inspector</strong> fed by <code>get_asset</code>. One <code>/api/ws</code> socket invalidates the affected
caches so scans and new assets stream in live. <code>3dam serve</code> <strong>embeds the built bundle</strong>
(<code>rust-embed</code> over <code>web/dist/</code>) with SPA-fallback routing. The <code>dam-viewer</code> crate
(wgpu 3D + waveform islands, WebGPU/WebGL2 via <code>wasm-pack</code>) and the responsive/touch collapse
(single-column below <code>lg</code> with drawers, 44px touch targets) are built and the live viewer is
wired into the Inspector.</p>
</div>

<div class="log">
<p class="when">2026-07-06</p>
<h4>Media depth — cheap metadata, thumbnails, convert <span class="pill done">Shipped</span></h4>
<p>Capability <strong>phase 2</strong> landed end-to-end (<a href="tech-spec/04-media-handlers.md">tech-spec 04</a>,
<a href="tech-spec/08-convert-pipeline.md">08</a>). The <strong>cost-tiered media handlers</strong> now fill
real attributes at scan: audio via a <code>symphonia</code> container probe (sample rate, channels, bit
depth, duration, codec — no PCM decode), images via header dimensions + a PNG IHDR alpha/depth sniff,
and 3D by hand-walking the <strong>GLB JSON chunk</strong> / glTF / OBJ / STL / PLY for vertex, triangle,
mesh, material, texture, and rig/anim/UV counts — <strong>never decoding geometry or the BIN chunk</strong>
(the cheap contract, §4). These persist to the per-type attribute tables and surface on the grid rows
(<code>key_attrs</code>) and the Inspector. <strong>Server-rendered image thumbnails</strong> (downscaled PNG,
content-hash-keyed on-disk cache) serve at <code>GET /api/v1/assets/{id}/thumbnail</code>, with audio
waveforms and 3D turntables kept as the interactive WASM islands; the web grid/inspector render real
previews with a graceful fall-back to the honest typed tile. The <strong>convert pipeline</strong> (CLI-first,
<code>3dam convert</code>) decodes via the same handlers and re-encodes non-destructively — image
transcode/resize and audio→WAV — with dry-run planning, the <strong>source-safety invariant</strong> (never
writes into a registered source), atomic temp-write-then-rename, and collision policy. Deeper codec and
format coverage (DDS/KTX2, MP4/AAC decode, mesh optimise/compression, more encode targets) stages behind
the same seams.</p>
</div>

## Mobile &amp; tablet posture

Desktop/tablet-first; phones are graceful-degrade, not a separate product. Because the web client is
DOM-based this is cheap:

- Collapse the three-region workspace to a single scrollable column at tablet width.
- **44×44px minimum touch targets**; `touch-action: manipulation` on canvas overlay controls.
- Stack side-by-side panes (e.g. viewer + detail) vertically with viewport-unit heights and a
  `min-height` floor so the 3D canvas stays usable.
- Treat full small-screen polish and real-device testing as tracked UX debt, not a v1 gate.

## Release &amp; distribution

The **canonical release plan lives in
[tech-spec 15](tech-spec/15-observability-config-testing-packaging.md) §15.5** (implementable CI
detail); the scaffold is at [`.github/workflows/release.yml`](../.github/workflows/release.yml). In
brief, following the pattern proven in the sibling *mogen* project:

- **Trigger:** push a `v*` tag (plus manual `workflow_dispatch`); `contents: write` to cut the release.
- **Build matrix** (`fail-fast: false`): Linux (`x86_64-unknown-linux-gnu`), macOS (Intel +
  `aarch64`), Windows (`x86_64-pc-windows-msvc`).
- **Web client first.** A release builds the React web client and embeds its static assets into the
  `3dam` binary (`rust-embed`) *before* `cargo build`, so one binary serves the web UI with no
  separate deploy — then a single `cargo build -p 3dam --release --locked --target <triple>`.
- **Per-OS packaging:** Linux `tar.gz` + `.deb` (`cargo-deb`); Windows `.zip` + `.msi` (`cargo-wix`);
  macOS `.app` in a `.dmg`. Publish collects artifacts, generates `SHA256SUMS`, creates the release.
- **v1 decisions:** unsigned (accept Gatekeeper/SmartScreen warnings); **GitHub Releases is the sole
  v1 channel** (Homebrew tap + `cargo-binstall` fast-follow) — [ADR 0009 §10](adr/0009-v1-scope-decisions.md).

## Open questions

Most were resolved on 2026-07-06 in [ADR 0008](adr/0008-web-client-stack.md) (web stack) and
[ADR 0009](adr/0009-v1-scope-decisions.md) (the v1-scope tail); the remaining live list is
[PRODUCT_SPEC.md](PRODUCT_SPEC.md) §10. Settled here for the record:

- ~~Exact React stack.~~ **React + TypeScript + Tailwind** on Vite/pnpm ([ADR 0008](adr/0008-web-client-stack.md)).
- ~~WASM-island packaging/handoff.~~ **`wasm-pack`, WebGPU + WebGL2 fallback, DOM owns data** ([ADR 0009 §9](adr/0009-v1-scope-decisions.md)).
- ~~Code signing &amp; notarization.~~ **Unsigned for v1;** revisit post-v1.
- ~~Distribution channels.~~ **GitHub Releases only for v1** ([ADR 0009 §10](adr/0009-v1-scope-decisions.md)).
- ~~View-logic sharing web ↔ desktop.~~ **Each frontend owns its presentation over the shared API in v1;** no shared view crate ([ADR 0009 §9](adr/0009-v1-scope-decisions.md)).
- ~~Desktop GUI embeds a webview.~~ **No** — fully native egui in v1 ([ADR 0009 §9](adr/0009-v1-scope-decisions.md)).

---

See also: [PRODUCT_SPEC.md](PRODUCT_SPEC.md) · [MISSION.md](MISSION.md) · [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md)
