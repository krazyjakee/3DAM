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
<tr><td>2</td><td><strong>Media depth</strong> — per-type decode + preview (waveform, image, 3D), thumbnail cache, convert pipeline (CLI-first) <span class="sub">Cheap per-type metadata, server image thumbnails + cache, interactive audio/3D islands, and the CLI convert pipeline all landed; deeper codec/format coverage staged. The interactive 3D island now shares the server's Assimp decode (a self-contained textured `DMSH` preview mesh), so every format — FBX/OBJ/DAE/glTF/… — previews in-browser with materials.</span></td><td><span class="pill done">Shipped</span></td></tr>
<tr><td>3</td><td><strong>Automation</strong> — feature extraction + embeddings, similarity search, auto-tag/categorise, dedup, review UX <span class="sub">The differentiator. Analyze pass (embeddings + tileability/pHash + auto-tag/-category suggestions), embedding-cosine <code>find_similar</code>, exact + near dedup grouping, and the accept/reject suggestion lifecycle all landed CLI-first over the same seam (embedded + <code>--connect</code>). Model-free v1 behind the <code>EmbeddingSpace</code> seam (ADR 0006): SigLIP/CLAP weights are a later feature-gated bump; the web review surface and HNSW-at-scale follow.</span></td><td><span class="pill done">Shipped</span></td></tr>
<tr><td>4</td><td><strong>Reach</strong> — SFTP + SMB sources, watch/auto-rescan, smart folders, export/manifests, CLI/GUI parity <span class="sub">The <code>FileSource</code> seam now spans local FS, <strong>SFTP</strong> (<code>russh</code>) and <strong>SMB2/3</strong> (pure-Rust <code>smb</code>) behind one <code>open_source</code>/<code>fetch</code> path; delta re-scan skips unchanged files and marks vanished ones absent; <strong>watch/auto-rescan</strong> (local FS events + remote polling) drives delta scans; <strong>smart folders</strong> (live saved queries) and manual collections; and <strong>export/manifests</strong> (JSON/CSV/sidecar, incl. attribution-only). All CLI-first over the same seam, embedded + <code>--connect</code>. Desktop GUI stays phase 5.</span></td><td><span class="pill done">Shipped</span></td></tr>
<tr><td>5</td><td><strong>Server &amp; web</strong> — <code>LibraryService</code> boundary, <code>3dam serve</code>, <code>3dam mcp</code>, <code>--connect</code>, web client, feature flags + Settings, basic auth <span class="sub">Complete for v1: the runtime feature-flag store + audited <code>/admin/api</code> + web Settings surface, basic access control (anonymous + token auth), and the MCP server (<code>3dam mcp</code> stdio + flag-gated <code>POST /mcp</code>) all landed on the one auth surface. Opt-in <strong>user accounts + OIDC</strong> layer on the same seam in phase 6.</span></td><td><span class="pill done">Shipped</span></td></tr>
<tr><td>6</td><td><strong>Federation &amp; auth</strong> — 3DAM-server source, federated fan-out + re-rank, cross-peer vector similarity, OIDC/OAuth2, opt-in accounts &amp; roles <span class="sub"><strong>Federation shipped</strong> (#39, #40): the <code>federated</code> source kind — a <code>3dam://</code> peer added behind a flag-gated <code>advertise()</code> handshake (semver protocol negotiation) — with query fan-out at the frozen 2.5&nbsp;s deadline, merge/re-rank + origin tagging + partial flagging (<code>total = None</code>), peer preview/detail proxying with a 7-day local cache, and cross-peer similarity <em>by vector</em> gated on an exact embedding-space match (mismatched spaces are omitted + flagged, never co-ranked). <strong>User accounts (#42) shipped</strong>: the <code>user_accounts</code> flag (off by default) brings full login accounts (argon2id, cookie sessions + CSRF + lockout, 14d/90d expiry), the first-run claim — first loopback signup becomes admin (ADR 0014) — fixed <code>admin/editor/viewer</code> roles mapped to scopes, flat groups, and source/collection <strong>sharing</strong> to users or groups, enforced as a visibility predicate inside the engine query path (search, similar, dedup, stats, folders, collections, previews, export, jobs, events all filter through one seam) with a leak-audit test per read path. <strong>OIDC/OAuth2 (#41)</strong> remains.</span></td><td><span class="pill active">In progress</span></td></tr>
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
- **Desktop GUI** — the native app. Built first as an egui client to substantive parity, then
  replaced by a Tauri webview shell over the embedded web client (ADR 0013) — one UI codebase,
  parity by construction. *(Phase 5 / spec §9 GUI parity.)*

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
(<code>key_attrs</code>) and the Inspector. <strong>Server-rendered thumbnails</strong> (downscaled PNG,
content-hash-keyed on-disk cache) serve at <code>GET /api/v1/assets/{id}/thumbnail</code>: a raster
downscale for images and a headless, <strong>textured PBR turntable render for 3D models</strong> across the professional
format range (FBX, OBJ/MTL, DAE, 3DS, glTF/GLB, PLY, STL, <code>.blend</code>, … via Assimp —
<a href="adr/0011-assimp-import-backend.md">ADR 0011</a>; USD family excepted), with a software-raster
fallback on GPU-less hosts per <a href="adr/0001-3d-render-backend.md">ADR 0001</a>. Audio waveforms
stay interactive WASM islands; the web grid/inspector render real previews with a graceful fall-back
to the honest typed tile whenever a render is unavailable. The <strong>convert pipeline</strong> (CLI-first,
<code>3dam convert</code>) decodes via the same handlers and re-encodes non-destructively — image
transcode/resize and audio→WAV — with dry-run planning, the <strong>source-safety invariant</strong> (never
writes into a registered source), atomic temp-write-then-rename, and collision policy. Deeper codec and
format coverage (DDS/KTX2, MP4/AAC decode, mesh optimise/compression, more encode targets) stages behind
the same seams.</p>
</div>

<div class="log">
<p class="when">2026-07-06</p>
<h4>Automation — analysis, similarity, dedup, review <span class="pill done">Shipped</span></h4>
<p>Capability <strong>phase 3</strong> — the differentiator — landed end-to-end
(<a href="tech-spec/05-analysis-similarity-dedup.md">tech-spec 05</a>). A new <strong><code>AnalysisRunner</code></strong>
(<code>3dam-core::analysis</code>) runs the versioned Plan→Extract→Derive→Classify→Index→Dedup pipeline as a
background job (<code>3dam analyze</code>, incremental via <code>analysis_version</code>; <code>--force</code> re-runs).
Per image it derives the <strong>tileability metric</strong> (edge-continuity vs internal gradient + autocorrelation
repeat-period, §6), a <strong>dHash perceptual hash</strong>, and dominant colours, and it emits
<strong>auto-tag / auto-category suggestions</strong> that are written <em>suggested</em>, never confirmed. Each media
type gets a normalised embedding in its own <strong><code>EmbeddingSpace</code></strong> (schema V3 <code>embedding</code>
table); <strong><code>find_similar</code></strong> ranks neighbours by cosine (brute-force exact in v1) and composes with the
same facet filters as search. <strong>Duplicate review</strong> surfaces exact (content-hash) and near (embedding-cosine,
union-find) groups with a suggested keep — grouping only, never deletion. The <strong>accept/reject suggestion
lifecycle</strong> (<code>3dam tag &lt;id&gt; &lt;name&gt; [--reject]</code>) promotes/negates tags reversibly and a reject
survives re-analysis. All four surfaces ride the <code>LibraryService</code> seam — embedded and remote
(<code>--connect</code>) return identical results. <strong>Model-free v1</strong> per
<a href="adr/0006-inference-runtime-candle.md">ADR 0006</a>: the default build runs offline with no weights, behind
the exact seam the SigLIP/CLAP path plugs into as a <code>model_version</code> bump; the web review UX and
HNSW-at-scale index are named follow-ups.</p>
</div>

<div class="log">
<p class="when">2026-07-06</p>
<h4>Reach — network sources, watch, smart folders, export <span class="pill done">Shipped</span></h4>
<p>Capability <strong>phase 4</strong> landed end-to-end (<a href="tech-spec/07-sources-and-federation.md">tech-spec 07</a>).
The <strong><code>FileSource</code> seam</strong> was generalised: every source resolves an entry's bytes to a local
path via <code>fetch</code> (in place for local, a downloaded temp file suffixed with the logical extension for
remote), and a <code>SourceConnection</code> model + <code>open_source</code> factory rebuild the backend from the
persisted connection blob (secret held server-side; clients only ever see the sanitised URI). Two real
network backends ride that seam: <strong>SFTP</strong> via <code>russh</code> + <code>russh-sftp</code> and
<strong>SMB2/3</strong> via the pure-Rust <code>smb</code> crate, driven from a private current-thread runtime
because the scan runs off the async workers. Unreachable hosts/bad creds mark the source offline and are
skipped, never fatal (fail-soft). <strong>Delta re-scan</strong> compares each entry's size+mtime change token
and only re-opens changed files, marking vanished ones absent (non-destructive). <strong>Watch/auto-rescan</strong>
uses OS change notification for local sources (debounced) and polling for remote, each triggering a delta scan.
<strong>Smart folders</strong> resolve a saved query live; manual collections hold an explicit set, surfaced on the
inspector record. <strong>Export/manifests</strong> emit JSON, CSV, or per-asset JSON sidecars over a selector
(ids / collection / query / whole library), with an <em>attribution-only</em> credits mode. All CLI-first
(<code>sources add sftp://…|smb://…</code>, <code>scan --delta</code>, <code>collections …</code>,
<code>export …</code>) over the <code>LibraryService</code> seam — embedded and <code>--connect</code> return
identical results. Remote-source <em>analysis/convert</em> (fetch-through) and the web-client surfaces for
collections/export are named follow-ups; the desktop GUI stays phase 5.</p>
</div>

<div class="log">
<p class="when">2026-07-06</p>
<h4>Server &amp; web — feature flags, basic auth, MCP <span class="pill done">Shipped</span></h4>
<p>Capability <strong>phase 5</strong>'s remaining half landed (<a href="tech-spec/10-auth-accounts-and-flags.md">tech-spec 10</a>,
<a href="tech-spec/11-mcp-server.md">11</a>, <a href="adr/0003-mcp-server.md">ADR 0003</a>/<a href="adr/0004-feature-flags-admin.md">0004</a>) — the
serve/<code>--connect</code>/web-client half shipped earlier. A <strong>server config store</strong> (<code>server.db</code>,
separate from the library file) holds a versioned <strong>feature-flag</strong> table, token records, and an append-only
<strong>audit log</strong>; the live flag state is seeded by a <code>serve.toml</code> config file (<code>config_authority =
seed-only</code>) and thereafter owned by the admin surface. Three flags gate real surfaces — <code>authentication</code>
(off/anonymous/token), <code>mcp_server</code> (off/read-only/read-write), <code>network_writes</code> — each live-toggleable,
with optimistic-concurrency versioning and a server-side <strong>confirm-on-exposure</strong> gate. <strong>Basic access
control</strong> resolves every request to an <code>AuthContext</code> + scope set through one auth layer over the whole
surface (API, admin, MCP); <code>Off</code> grants the localhost owner full trust, <code>Token</code> requires a bearer key,
and read/write/admin scopes gate handlers (writes further gated by the network ceiling beyond localhost). Bearer
<strong>API tokens</strong> are issued through the audited admin API (secret shown once, blake3-hashed at rest). The
<strong>admin API</strong> (<code>/admin/api/*</code>) is the single source of truth driven by both the CLI (<code>3dam admin
flags|flag|token|status|audit|maintenance</code>, embedded or <code>--connect</code>) and the web <strong>Settings / Administration</strong>
surface (grouped flag cards, warn-and-confirm, token management, audit trail, and a <strong>Storage &amp; maintenance</strong>
section — usage overview, clear thumbnail/3D-preview caches, clear analysis, VACUUM, reset-catalog, and factory-reset,
all audited and non-destructive to source files). <em>(The desktop shell renders the same web client
(ADR 0013), so the Settings surface is native too.)</em> The <strong>MCP server</strong> (ADR 0003,
hand-rolled JSON-RPC over <code>dyn LibraryService</code> — no subprocess) serves tools/resources/prompts over both
<code>3dam mcp</code> stdio (locally trusted) and <code>POST /mcp</code> on the shared port; the <code>mcp_server</code>
flag mounts/unmounts it (<code>Off ⇒ 404</code>) and a <code>WriteGate</code> (bind + flag + caller scope) filters the write
tools. Binding beyond localhost without TLS is refused unless <code>--insecure</code> (ADR 0009 §4). <strong>User accounts,
sessions, OIDC, and TLS</strong> layer on the same <code>AuthContext</code> seam in phase 6.</p>
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
- ~~Desktop GUI embeds a webview.~~ Originally **no** (fully native egui, [ADR 0009 §9](adr/0009-v1-scope-decisions.md)); **reversed 2026-07-16** — the desktop app is now a Tauri webview over the embedded web client ([ADR 0013](adr/0013-desktop-shell-tauri.md)).

---

See also: [PRODUCT_SPEC.md](PRODUCT_SPEC.md) · [MISSION.md](MISSION.md) · [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md)
