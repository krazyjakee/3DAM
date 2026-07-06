# 3DAM — Design Guidelines

These guidelines govern how 3DAM is built and how it behaves. They apply to the desktop
app, the CLI, and the server/web client. When a decision is ambiguous, favour the principle
earlier in this document over the one later.

[[SHOWCASE]]

## 1. Product principles

### 1.1 Speed is non-negotiable
- Target **60 fps** scrolling through a grid of 100k assets. Thumbnails and metadata load
  lazily and are virtualised; the UI never blocks on I/O or analysis.
- All heavy work — scanning, decoding, feature extraction, thumbnailing — runs off the UI
  thread on a bounded worker pool and reports progress incrementally.
- Prefer **incremental** over **batch**: results appear as they are computed, never in one
  final dump. A partial index is usable immediately.
- Measure before optimising, but design for scale from the start. Assume libraries of
  **1M+ assets** and datasets that do not fit in RAM.

### 1.2 Automation is the default, not an add-on
- The system proposes; the user disposes. Auto-categorisation, auto-tagging, and duplicate
  detection run automatically on ingest and surface **suggestions**, never silent mutations.
- Every automated action is **reviewable, reversible, and explainable** — the user can see
  *why* an asset was tagged "metallic" or flagged as a near-duplicate.
- No destructive automation without explicit opt-in. 3DAM never deletes, moves, or
  overwrites source files unless the user directs it to.

### 1.3 Local-first and non-destructive
- The source of truth is a **local database**; sources (disks, shares) are treated as
  read-only unless the user asks otherwise.
- 3DAM catalogues **in place**. Importing an asset records a reference, generates
  derivatives (thumbnails, waveforms, embeddings), and never requires copying the original.
- All derived data lives outside the source tree, in 3DAM's own managed store.

### 1.4 One engine, many front-ends
- GUI, CLI, and web client are all front-ends over the **same engine** (`3dam-core`). No
  feature is GUI-only by architecture, and no front-end reaches past the engine's API.
- The CLI is first-class: scriptable, pipeable, machine-readable output (`--json`), stable
  exit codes, no interactive prompts required in non-interactive mode.
- **The client is also the server.** One binary provides the GUI client, the CLI client,
  and — via `3dam serve` — the server that hosts the API and the web client. The engine runs
  either **embedded** in a client (standalone, no network) or **as a service** that clients
  connect to. Front-ends therefore talk to a `LibraryService` interface, never to a local
  database directly, so the same UI code works whether the library is local or remote.
- Server mode is **config-driven and headless-friendly**: it reads a config file naming the
  sources to index and starts unattended — suitable for a NAS, a workstation, or CI.

### 1.5 Open and portable
- Database format is documented and stable. Metadata is exportable to plain text
  (JSON/CSV/sidecar files) at any time.
- No telemetry. No network calls the user did not initiate. No account required.

## 2. Architecture guidelines

- **Layered core.** `3dam-core` (database, scanning, analysis, search) knows nothing about
  the GUI, CLI, web, or transport. Front-ends depend on the core; the core depends on neither.
- **Transport-agnostic front-ends.** Clients depend on a `LibraryService` trait with two
  implementations — in-process (embedded engine) and API-client (remote `3dam serve`). The
  server exposes that same surface over HTTP/WebSocket. Adding server/web support is
  implementing the trait, not forking the UI. See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) §4.
- **Media plugins.** Each media type (audio, image, 3D) implements a common
  `MediaHandler` trait: detect, decode, thumbnail, extract features, extract metadata.
  Adding a new format or media type is adding an implementation, not editing the core.
- **One database, many media types.** A shared asset schema holds what is common (path,
  hash, size, tags, source, timestamps); media-specific attributes hang off it. See
  [PRODUCT_SPEC.md](PRODUCT_SPEC.md) for the model.
- **Sources are abstract, in two kinds.** *File sources* (local FS, SFTP, SMB) yield raw
  bytes the engine processes; *federated sources* (a remote 3DAM server) yield catalog
  results the engine merges. Both satisfy one `Source` trait so code above it treats them
  uniformly; the query engine fans out and merges results across all sources.
- **Federate, don't reprocess.** A federated peer already extracted features, built
  thumbnails, and computed embeddings — 3DAM queries that catalog and never re-decodes or
  re-analyses another instance's files. Federated assets are read-only references with
  remote-owned previews. This line is load-bearing: local processing is only ever for
  *your* file sources. See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) §4.4.
- **Auth is a first-class layer, designed in early.** Every remote interaction carries an
  auth context (anonymous, token, or OIDC/OAuth2); credentials live in the OS secret store,
  never in the library file. Start simple (tokens + anonymous read) but keep the seam clean
  so open-standard identity slots in without a rewrite. Prefer open standards over bespoke
  auth.
- **Capabilities are feature flags, off by default — not forks.** Every server capability (the
  MCP agent server, authentication, user accounts, inbound federation, remote connect, network
  writes) is a runtime flag on the one shared surface, not a separate build or daemon. A server
  ships locked down and each capability is switched on deliberately — from a config file or the
  web admin UI, which are coequal control planes over one persisted state. Turning a capability
  **off removes its surface** (route, tools, endpoint), so "off" actually shrinks exposure.
  Accounts and roles are opt-in and server-side, never written into the portable library file.
  See [PRODUCT_SPEC.md](PRODUCT_SPEC.md) §6.11.
- **Analysis is pluggable and optional.** Feature extractors (embeddings, perceptual
  hashes, spectral analysis) are modules that can be enabled/disabled and versioned, so a
  library can be re-analysed when a model improves.
- **Fail soft.** A corrupt file, an unreadable format, or a dropped network share degrades
  one asset — never the scan, never the app.

## 3. Interaction & UX guidelines

The desktop app draws on three proven layouts (see `docs/existing-product-screenshots/`):
a **hierarchical source/category sidebar** (Connecter), a **content grid + rich detail
panel** (Connecter / echo3D), and a **sortable attribute table with inline previews and
similarity search** (Sononym). 3DAM unifies these into one workspace.

### 3.1 Layout
- **Three-region workspace:** left navigation (sources, collections, tags, smart folders),
  centre browser (grid or table, user-toggleable), right detail/inspector panel.
- **Grid and table are equal.** Grid for visual scanning (thumbnails, turntables,
  waveforms); table for dense, sortable, attribute-driven work (BPM, dimensions, poly
  count). Any view switch preserves the current selection and filter.
- The **inspector** shows a large preview, full metadata, extracted features, tags, and
  source info for the selected asset. It is the single place to review and correct
  automated results.
- **License is prioritised in the inspector.** Because different assets carry different
  licenses, the license/rights of the selected asset appears high in the panel — directly
  under the title, as a colour-coded badge (e.g. permissive / attribution-required /
  restricted / unknown) with the key rights and attribution beside it — not buried among
  technical metadata. An **unknown or unverified** license is shown as exactly that, and is
  never styled as if it were safe. The goal: a user should never ship an asset without having
  seen what they're allowed to do with it.

### 3.2 Previews per media type
- **Audio:** waveform + scrubbable playback; spectral/feature readouts; space to play.
- **Image:** thumbnail → full-resolution zoom/pan; format, dimensions, colour info.
- **3D:** rendered thumbnail and an interactive orbit viewer; poly count, materials, bounds.
- Previews are generated once, cached, and regenerated only on source change.

### 3.3 Search and discovery
- **Text search** (names, tags, metadata) is always available and instant.
- **Similarity search** ("find more like this") is a first-class action on any asset,
  powered by content embeddings — the signature feature carried across all three media
  types, not just audio.
- **Faceted filtering** via chips/dropdowns (type, tags, format, source, size, and
  media-specific facets). Filters compose; the active filter set is always visible.
- **Smart folders / saved searches** persist a query as a live, self-updating collection.

### 3.4 Feedback and control
- Long operations always show progress, are cancellable, and never freeze the UI.
- Every automated suggestion has a visible, one-action **accept / reject**.
- Bulk operations (retag, convert, export) preview their effect and are undoable where
  physically possible.

### 3.5 Accessibility & platform fit
- Full keyboard navigation; keyboard shortcuts for every high-frequency action.
- Honour OS light/dark preference; ship a dark default (the working context for most
  game-dev tooling).
- Cross-platform first (Linux, Windows, macOS); no platform-exclusive core features.

### 3.6 Server administration & feature flags
The web client hosts an admin-only **Settings / Administration** area for the server's feature
flags, authentication, and user accounts (see [PRODUCT_SPEC.md](PRODUCT_SPEC.md) §6.11). It is a
first-class, beautifully presented surface — not a raw config editor:

- **Grouped toggle cards.** Capabilities are grouped by area (Access, Authentication, Accounts,
  Agents / MCP, Federation, Analysis). Each is a labelled toggle with a title, a one-line
  description of what it does, and an obvious on/off state.
- **Progressive disclosure.** Enabling a capability reveals its sub-options inline — turn on
  Authentication and the mode picker appears; turn on Accounts and the user list + role editor
  appear; turn on the MCP server and the read-only/writes choice appears. Nothing irrelevant is
  shown until it applies.
- **Warn before exposure.** Consequential toggles (exposing the server without auth, enabling
  MCP or network writes beyond localhost) show a clear risk note and require confirmation before
  taking effect — the no-surprises rule (§3.4) applied to configuration.
- **Live vs restart is explicit.** Flags that apply immediately do so; any that need a restart
  are labelled, never silently deferred.
- **Parity with config/CLI.** Everything here is equivalently settable from the config file and
  the CLI, so the admin UI is a convenience over one source of truth, never the only way in.

## 4. Visual design

- **Dark-first, low-chrome.** The content — thumbnails, waveforms, models — is the bright
  part of the screen; the UI recedes. One restrained accent colour for selection, focus,
  and primary actions.
- **Information-dense but calm.** Favour tables and tight grids over whitespace-heavy
  cards, but keep typography and spacing consistent and legible at a glance.
- **Consistent iconography** per media type and per action, used identically in GUI and in
  CLI output labels where applicable.
- **Content-truthful thumbnails.** Previews represent the real asset (correct aspect ratio,
  colour, framing); no decorative placeholders masquerading as content.
- **State-truthful controls.** In the admin surface (§3.6), a feature's on/off/at-risk state is
  carried by colour and a clear control, using the one accent for "on" and a distinct warning
  tone for exposure risk — the settings recede, the state reads at a glance, no decorative chrome
  dressing up a config screen.

## 5. CLI guidelines

- **Verb-noun command structure:** `3dam scan <source>`, `3dam search <query>`,
  `3dam similar <asset>`, `3dam convert <asset> --to <format>`, `3dam export`.
- **Human output by default, machine output on request:** readable tables interactively;
  `--json` / `--csv` for scripting. Detect non-TTY and quiet down automatically.
- **Composable:** results stream, exit codes are meaningful, and output can be piped into
  the next command or standard Unix tools.
- **No surprises:** destructive commands require confirmation or an explicit `--yes`;
  `--dry-run` is available for anything that writes.
- **Server from the CLI:** `3dam serve` launches server mode from a config file (sources to
  index, listen address, analysis settings, and **feature flags**) and runs headless;
  `--connect <host:port>` points any client command at a remote library instead of the local
  one. Feature flags and user accounts are readable and settable from the CLI, mirroring the web
  admin surface (§3.6), so a server is fully administrable without a browser.
- **Shared config and database** with the desktop app — run either against the same library.

## 6. Quality bar

- **Non-destructive by default**, everywhere. When in doubt, don't touch the user's files.
- **Reproducible analysis:** feature extractors are versioned so results can be explained
  and regenerated.
- **Graceful degradation:** missing optional dependency, offline share, or unsupported
  format produces a clear message and a still-usable app — never a crash.
- **Tested at scale:** performance and correctness are validated against large, real,
  messy libraries, not just tidy fixtures.

---

See also: [MISSION.md](MISSION.md) · [PRODUCT_SPEC.md](PRODUCT_SPEC.md)
