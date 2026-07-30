# 3DAM — High-Level Product Spec

Status: **Draft v0.1** · Scope: product vision → system shape. This is a high-level spec:
it defines *what* 3DAM is and the shape of its parts, not final APIs or schemas.

See [MISSION.md](MISSION.md) for the why and [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md)
for the rules that constrain the choices below.

---

## 1. Product summary

3DAM is a cross-platform (Linux/Windows/macOS) **Rust** application that catalogues,
analyses, searches, and converts game assets across **audio, image, 3D model, video, and
document** media types. It reads from local disks and network shares, builds one unified
database, and uses content-based analysis to auto-categorise, auto-tag, find similar assets,
and detect duplicates.

The first three are the *deep* media types — full decode, analysis, embeddings, conversion.
**Video** and **document** are deliberately shallower (§9, phase 2b): they exist so a
registered source can be described *completely*, because a project folder that is one-third
unindexable is not "one catalog for the project". Each is honest about what it can do rather
than pretending to parity — see the phase-2b entry for the specific limits.

It ships as **one executable with three roles**:

- **GUI client** — the native desktop app (`3dam` with no command).
- **CLI client** — scriptable commands (`3dam <verb>`), for pipelines and CI.
- **Server** — `3dam serve` reads a config, indexes the folders/sources it names, runs the
  engine as a long-lived service, and exposes an **API plus a browser-based web client**.

The client is also the server: the same `3dam-core` engine powers every role. A client can
run the engine **embedded** (standalone, single process, no network) or **connect** to a
remote `3dam serve` instance over its API — so the GUI, the CLI, and a web browser can all
drive a library that lives on another machine.

Crucially, **a 3DAM server is itself a kind of source**. Alongside local folders, SFTP, and
SMB, you can add another 3DAM instance and it behaves like an extension of your own
database: search, similarity, and smart folders **federate** across it. 3DAM does not fetch
and re-process a peer's files — it **queries the catalog the peer already built** and merges
the results into one unified view. An **authentication** layer (open standards such as
OIDC/OAuth2, plus simple tokens) sits in front of remote instances, so creators and stores
can host gated catalogs. Longer term, many federated instances compose into a **mesh** of
shared asset networks.

Everything the server exposes is **opt-in and administrable**. A server ships locked down —
bound to localhost, no authentication, no user accounts, agent (MCP) access and inbound
federation off, read-only to the network — and each capability is a named **feature flag** an
operator turns on, either declaratively in the config file or from a **beautifully presented
administration surface in the web client**. Authentication, user accounts, the MCP server, and
remote access are all toggles (see §6.11), so the *same binary* is a zero-config personal
library out of the box and a hardened multi-user host once configured — you never pay for a
capability you have not deliberately switched on.

## 2. Goals and non-goals

### Goals
- One database and one browser for sound, image, and 3D assets — plus the **video and
  documents** that live alongside them in a real project folder, so a source is catalogued
  completely rather than partially (§9 phase 2b).
- Content-aware automation: categorisation, tagging, similarity, de-duplication.
- Fast on very large libraries (design target: **1M+ assets**).
- Local-first, non-destructive, open, no subscription, no telemetry.
- One binary, three roles (GUI client, CLI client, server) over a shared engine, with
  parity between them.
- **Self-hostable server** with a browser-based web client — run the engine where the
  assets live, browse from anywhere on your network.
- **Federated sources:** add another 3DAM server as a source and query its catalog as if it
  were local; blend local + remote results in one view.
- **Authentication built in from the start** (basic tokens first, open standards such as
  OIDC/OAuth2 as the extension path) so remote catalogs can be public or gated.
- Broad format support and standard network sources (SFTP, SMB/Samba).
- **AI-agent access built in (and switchable off):** a comprehensive **MCP server** exposing
  the library to LLM agents, served on the **same port** as the rest of the server (§6.10) —
  and, because not everyone wants it, a first-class **feature flag** that turns the whole MCP
  surface off (§6.11).
- **Administrable by feature flag, safe by default.** Every server capability — the MCP agent
  server, authentication, **user accounts**, inbound federation, remote client access, network
  writes — is an explicit **feature flag**, off by default and toggled from a config file *or* a
  polished admin UI in the web client (§6.11). Nothing sensitive is exposed until an operator
  consciously turns it on.

### Non-goals (v1)
- Not a **hosted / managed cloud service** — 3DAM's server is something *you* run on *your*
  hardware. There is no 3DAM-operated cloud, no CDN, and no sign-up to any 3DAM service. The
  optional **user accounts** feature (§6.11) is an account *on your own self-hosted server*,
  enabled by you — never an account *with us* — and it stays off by default, so the local-first,
  no-account experience is untouched unless an operator turns it on.
- Not **remote reprocessing** — 3DAM never decodes or re-analyses a federated peer's files;
  it queries the catalog that peer already built. Local processing is for *your* sources.
- Not a **full mesh / relay network** in v1 — v1 does direct client→server federation
  (query a peer you connect to). Peer-to-peer relay, transitive fan-out, and instance
  discovery are a documented future direction (§9), not initial scope.
- Not a **content creation / editing** tool — 3DAM manages and converts assets; it does not
  author them (no DAW, no image editor, no modeller).
- Not a **general-purpose DAM**, and not a video or document management system. 3DAM
  catalogues the video and documents it finds *in a game project* (cutscenes, stingers,
  reference footage, design docs, licence files, store receipts) so a source is described
  completely — see §9 phase 2b. It does **not** aspire to parity with the deep media types:
  no video transcoding, no editing, no document authoring, no versioning or approval
  workflow. Audio, image, and 3D remain the media types 3DAM is *for*.
- Not a **real-time collaborative** editor — the server enables shared *access* to a
  library; concurrent multi-user editing and sync are out of scope for v1.

## 3. Primary users & use cases

- **Sound designer:** "Find footstep sounds similar to this one across my whole SFX
  library, regardless of folder or filename."
- **Tech artist:** "Show every low-poly prop under 5k tris with a PBR material set, from
  both my local drive and the studio NAS."
- **Indie generalist:** "Point it at ten years of asset packs and give me a clean,
  de-duplicated, tagged library I can actually search."
- **Build engineer:** "In CI, convert all source textures to KTX2 and export a manifest —
  headless, scripted, reproducible."
- **Shipping a commercial game:** "Show me only assets I'm licensed to use commercially, tell
  me which ones need attribution, and export the credits list — before I ship, not after a
  takedown."

## 4. System architecture (high level)

### 4.1 One binary, three roles

A single executable dispatches on how it is invoked:

```
                       one binary:  3dam
   ┌───────────────────┬───────────────────┬────────────────────┐
   │  3dam             │  3dam <verb>      │  3dam serve        │
   │  (GUI client)     │  (CLI client)     │  (server)          │
   └───────────────────┴───────────────────┴────────────────────┘
```

- **GUI client** — native desktop app; the default when launched with no command.
- **CLI client** — verb-driven, scriptable, machine-readable output; for pipelines and CI.
- **Server** — reads a config file, indexes the folders/sources it lists, runs the engine
  as a long-lived service, and exposes an HTTP/WebSocket **API** plus a **web client**.

### 4.2 The client is also the server: embedded vs connected

Every role is a thin shell around the same engine, `3dam-core`. What differs is *where the
engine runs*:

```
  EMBEDDED (standalone, single process, no network)

    ┌──────────────┐        ┌──────────────┐
    │  GUI client  │   or   │  CLI client  │
    └──────┬───────┘        └──────┬───────┘
           └──────────┬────────────┘
                      ▼  in-process call
              ┌───────────────┐
              │   3dam-core   │   ← engine linked directly in
              └───────────────┘

  CONNECTED (engine runs elsewhere; clients talk to it over the API)

    ┌───────────┐  ┌───────────┐  ┌───────────────┐
    │ GUI client│  │ CLI client│  │  web client   │
    └─────┬─────┘  └─────┬─────┘  │ (browser, UI  │
          │              │        │  served by    │
          │              │        │  the server)  │
          │              │        └───────┬───────┘
          └──────────────┴────────────────┘
                         ▼  HTTP / WebSocket API
                 ┌───────────────────┐
                 │   3dam serve      │
                 │  ┌─────────────┐  │
                 │  │  3dam-core  │  │   ← same engine, now a service
                 │  └─────────────┘  │
                 └───────────────────┘
```

Clients depend on a **`LibraryService` interface**, not on the engine directly. Two
implementations satisfy it: an **in-process** one (the embedded engine) and an **API
client** one (talks to a remote `3dam serve`). The GUI and CLI pick either; the web client
is always connected. This is what "the client is also the server" means in practice — the
same code runs your laptop's standalone library *and* the always-on instance on your
workstation or NAS.

### 4.3 The engine and its layers

Inside `3dam-core`, whether embedded or serving:

```
             ┌───────────────────────────────────┐
             │             3dam-core             │
             │  library / query API              │
             │  scan & watch engine              │
             │  analysis pipeline                │
             │  convert pipeline                 │
             │  ─────────────────────────────    │
             │  server extras (in `serve`):      │
             │   HTTP/WS API + MCP · web host    │
             └──┬───────────────┬─────────────┬──┘
                ▼               ▼             ▼
        ┌──────────────┐ ┌────────────┐ ┌──────────────┐
        │Media handlers│ │Source layer│ │   Storage    │
        │ audio/image/ │ │ local FS / │ │ metadata DB  │
        │     3D       │ │ SFTP / SMB │ │ + vector idx │
        │              │ │            │ │ + blob cache │
        └──────────────┘ └────────────┘ └──────────────┘
```

- **`3dam-core`** owns the library model, scanning, analysis orchestration, search, and
  conversion. It is UI- and transport-agnostic. In `serve` mode it additionally mounts an
  API, an MCP endpoint for agents (§6.10), and hosts the web client's assets — all on one
  port.
- **Media handlers** implement a common trait per media type (detect, decode, thumbnail,
  extract features, extract metadata). Formats plug in here.
- **Source layer** abstracts where content comes from behind one trait, spanning two
  categories (see §4.4): **file sources** (local FS, SFTP, SMB) that yield raw bytes the
  engine processes, and **federated sources** (a remote 3DAM server) that yield catalog
  results the engine merges without processing.
- **Storage** = the metadata database + a vector index for similarity + a managed cache of
  derived blobs (thumbnails, waveforms, embeddings).

Three ways to reach remote content, worth keeping straight:

- **Remote *files*** — mount another machine's files as a **file source** (SFTP/SMB). Your
  engine reads the bytes and processes them locally (decode, thumbnail, embed).
- **Remote *catalog*** — add a **3DAM server as a source** (§4.4). Your engine *queries* the
  peer's already-built index and merges the hits; no bytes are processed. Your library is
  the union of local + federated catalogs.
- **Remote *engine*** — run the GUI/CLI as a thin client of a single server via `--connect`;
  that one server *is* your entire backend (no local library).

### 4.4 Federation: a 3DAM server as a source

Adding a 3DAM server as a source turns search into a **federated query**: the local engine
fans a query (text, facets, or a similarity vector) out to each connected peer, each peer
runs it against the index *it* built, and results are merged and re-ranked into one view.

```
        your machine                    elsewhere on the network / internet
   ┌────────────────────┐
   │    3DAM (local)    │      federated query: search · similar · facets
   │  ┌──────────────┐  │   ┌──────────────────────────────────────────────┐
   │  │ query engine │──┼──▶│  auth ▶  3dam serve   (studio NAS catalog)    │
   │  │  fan-out +   │──┼──▶│  auth ▶  3dam serve   (asset-store catalog)   │
   │  │  merge/rank  │──┼──▶│  auth ▶  3dam serve   (teammate's library)    │
   │  └──────────────┘  │   └──────────────────────────────────────────────┘
   │  local index +     │   peers return catalog rows, previews & similarity
   │  file sources      │   hits from indexes THEY already built —
   └────────────────────┘   no files are transferred or re-processed
```

- **Query, don't reprocess.** The peer already extracted features, built thumbnails, and
  computed embeddings. The local engine asks; it never decodes or re-analyses the peer's
  files. This is the hard line between a *file source* and a *federated source*.
- **Read-only and referenced.** Federated assets are references, shown with their remote
  previews (fetched on demand and cached). The original file is downloaded only if the user
  explicitly asks and the peer's permissions allow it.
- **Similarity across peers.** "Find similar" sends the query embedding to each peer's
  similarity endpoint; each searches its own vector index and returns ranked hits, merged
  locally. (Cross-peer ranking needs compatible embedding spaces — see §10.)
- **Authenticated.** Each federated source carries an auth config — anonymous (public read),
  a token/API key, or an OIDC/OAuth2 identity — so a peer can be open or gated. The server
  side enforces it (see §6.7).
- **Composable.** Smart folders and saved searches can span local + federated sources, so a
  live collection can pull from your disk and a remote store at once.

## 5. Unified data model (conceptual)

The core is one **Asset** entity with shared fields plus a media-specific attribute set.

**Asset (shared)**

- Identity: stable id, content hash (for dedup and change detection).
- Location: source ref + path; original filename; size; created/modified times.
- Type: media type (audio | image | model) + concrete format.
- **License / rights** (a first-class, prioritised field — see below).
- Organisation: tags, collections, user rating/flags, notes.
- Derived: thumbnail ref, preview ref, embedding ref(s), analysis version.
- Provenance: which source, when scanned, when last analysed.

**Media-specific attributes**

- **Audio:** duration, sample rate, bit depth, channels; detected BPM, key, loudness;
  spectral descriptors (brightness, harmonicity, etc.); class/category (e.g. one-shot vs
  loop, SFX vs music).
- **Image:** dimensions, colour depth, alpha, colour space; dominant colours; classified
  type (texture / sprite / UI / concept); **tileability metric** (see below); perceptual
  hash.
- **3D model:** vertex/triangle count, mesh/material/texture counts, bounding box, has
  rig/animation, UV presence; category guess; per-view render embedding.

**Tileability metric (image)**

For textures, 3DAM records how well an image tiles — a common, tedious thing to check by
hand across a large texture library. It is a **score plus a classification**, not just a
boolean, so users can sort and filter ("show non-seamless textures I need to fix" or "only
truly seamless materials"):

- **`tileability` score (0–1):** how cleanly the image wraps. Derived from an
  **edge-continuity test** — compare opposite edges as if tiled (left↔right, top↔bottom) and
  score the seam discontinuity *relative to the texture's own internal gradient*, so noisy
  and smooth textures are judged fairly rather than against a fixed threshold. Analysis runs
  on a downscaled copy in linear colour space; normal maps are compared as decoded vectors,
  not raw RGB.
- **`repeat_period` (optional):** whether the image itself already contains internal
  repetition (e.g. a 512 tile packed 2×2 into a 1K file). Detected via autocorrelation / FFT
  peaks; absent when the content is non-repeating.
- **classification** derived from the two: **seamless** (wraps cleanly), **tiled/repeating**
  (contains internal repetition), or **non-tiling** (neither).

Cheap enough to run on every image at ingest (edge test is ~O(w+h); the periodicity pass
runs on a thumbnail), consistent with the cost-tiered extraction rule (§6.2). Feeds
auto-tagging (`seamless`, `tileable`) and becomes a **search facet** (§6.3).

**Cross-cutting**

- **License / rights** are first-class per-asset metadata, not a buried note — different
  assets in one library carry different licenses, and knowing what you may ship is as
  important as knowing what the asset *is*. Each asset records:
  - a **license identifier** (SPDX where it exists — e.g. `CC-BY-4.0`, `CC0-1.0`, `MIT`,
    `OFL-1.1` — or `Proprietary` / `Custom` / `Unknown`);
  - a **rights summary** of the permissions that matter for games: commercial use,
    modification, redistribution, and whether **attribution** is required;
  - **attribution details** (author/holder, and the credit string to reproduce);
  - **provenance** (license source URL / EULA link, and where the value came from — declared
    by the source, read from a sidecar/pack manifest, or set by the user).
  License is **prioritised in the inspector** (shown high, with a status colour) and is a
  **search facet** (see §6.3), so "show only assets I can use commercially without
  attribution" is one filter. Unknown/unverified licenses are flagged, never silently
  assumed permissive.
- **Tags** are shared and free-form, with a suggested/auto vs confirmed distinction.
- **Collections / smart folders**: manual sets and saved-query live sets that may span local
  and federated sources.
- **Sources** are first-class records (kind, connection info, online/offline state, and —
  for federated sources — auth config). Two kinds: **file sources** (local FS, SFTP, SMB)
  and **federated sources** (a remote 3DAM server).
- **Federated assets** are **read-only references** owned by a peer: 3DAM stores enough to
  list, filter, and rank them (identity, key attributes, tags, remote preview refs) but
  their derivatives were computed remotely and are fetched on demand, not regenerated. They
  carry which peer they came from so results stay attributable.

The next two are **server-side records**, and deliberately live *outside* the asset library —
in the server's own store, never in the portable library file or in any export — because they
are host configuration and identity, not catalog data:

- **Server settings / feature flags** are a first-class, persisted, versioned record: which
  capabilities are enabled (MCP server, auth mode, user accounts, inbound federation, remote
  connect, network writes), the bind address, and analysis settings. The config file seeds it
  and the admin UI edits it; both read and write the same state (§6.11).
- **User accounts** are optional and **off by default**. When the accounts flag is on, each is
  a server-side identity record: a name, hashed credentials, a **role** (admin / editor /
  viewer) that grants **scopes**, and an optional **visibility scope** (which sources and
  collections the account may see). Accounts gate every client uniformly through the one auth
  surface (§6.7, §6.11); an account's scope is the ceiling on what it can see anywhere.

Everything above is media-agnostic where it can be, so one grid, one search box, and one
inspector work across all three types.

## 6. Feature areas

### 6.1 Ingest & sources
- Add a **file source** (local folder, SFTP host, SMB share) and scan it — incrementally,
  with live progress, resumable, and cancellable. These bytes get processed locally.
- Add a **federated source** (a remote 3DAM server) — no scan or processing; the peer's
  catalog becomes queryable immediately (§6.7). Mix both kinds freely in one library.
- **Watch** local sources for changes; re-scan deltas automatically.
- Non-destructive: catalogue in place, never copy or move originals by default.
- Robust to offline/unreachable sources (mark offline, keep cached metadata/results usable).

### 6.2 Analysis & automation (the differentiator)
- On ingest, each asset is decoded and passed through its media handler's extractors:
  - **Audio:** decode → spectral/temporal features → audio embedding → BPM/key/class.
  - **Image:** decode → perceptual hash + colour analysis → image embedding → type classify.
  - **3D:** load → geometry stats → multi-view render → shape embedding → category classify.
- Outputs feed **auto-tagging**, **auto-categorisation**, **duplicate/near-duplicate
  detection**, and the **similarity index**.
- Extraction is **cost-tiered**, which matters most for 3D: base stats (vertex/triangle
  counts, mesh/material/texture counts, bounding box, rig/animation and UV presence) come
  from a cheap container/header scan **without decoding geometry or touching the GPU**, so
  they run on every asset at ingest scale (§8). Full decode + multi-view render + embedding
  is deferred to preview/analysis. The `MediaHandler` split between `extract_metadata` and
  `thumbnail`/`extract_features` exists to keep this cheap tier cheap.
- All results are suggestions surfaced in the inspector for one-action accept/reject;
  extractors are **versioned** so a library can be re-analysed when models improve.

### 6.3 Browse, search & discovery
- Grid and table views over the whole library or any filtered subset.
- Instant text search; faceted filtering; saved searches / smart folders.
- **License / rights is a facet:** filter by license or by usage right ("commercial use
  allowed", "no attribution required", "redistributable", "unknown license"), so a live
  smart folder like *safe-to-ship* is a first-class thing.
- **Similarity search** ("more like this") across every media type.
- Duplicate review view grouping exact and near-duplicates.
- Queries **fan out across federated sources** and merge, so results, facets, and
  "find similar" span local + remote catalogs; each result shows its origin peer.

### 6.4 Preview
- Media-appropriate previews (waveform + playback, image zoom/pan, 3D orbit viewer), cached
  and lazily generated. See [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md) §3.2.

### 6.5 Convert / compress / optimize
- Local, scriptable conversion between formats per media type (e.g. audio codec/rate,
  image format/compression such as PNG↔KTX2, 3D format such as FBX↔glTF plus mesh
  optimisation/compression).
- Batch conversion with preview and dry-run; outputs written to a user-chosen location,
  never overwriting sources silently.

### 6.6 Export & interop
- Export metadata and manifests (JSON/CSV/sidecar) for use in engines and pipelines.
- Stable, documented database format; the user can always get their data out.

### 6.7 Federation & authentication
- Add a remote 3DAM server as a **federated source** (`3dam source add 3dam://host …`); its
  catalog joins your library and every query fans out to it (§4.4). No files are pulled or
  re-processed — 3DAM queries the index the peer already built.
- **Federated query** covers text, facets, and similarity: the local engine forwards the
  query (including a similarity vector) to each peer's API, each searches its own index, and
  results merge + re-rank locally, tagged with their origin peer.
- **License travels with the asset.** Federated catalogs (especially creator/store
  instances) publish each asset's license and rights, so the same license facet works across
  peers — e.g. "commercial-use assets across every store I've connected". A store can gate or
  price by license; that stays the peer's concern.
- **Authentication, from the start (basic first):**
  - *Anonymous* read for public/open catalogs.
  - *Token / API key* per source — the simple default for private instances.
  - *Open standards* as the extension path — OIDC / OAuth2 for identity, so an instance can
    sit behind an existing SSO/identity provider.
  - Client credentials are stored in the OS keychain/secret store, per source; never in the
    library file. The server enforces auth and can scope what a caller may see.
  - *Enabled by feature flag.* On the server side, the auth mode (off / anonymous · token ·
    OIDC/OAuth2) and the optional **user accounts** layer are switched from the admin surface or
    config file (§6.11). Turning auth on is the single gate that covers the web client, the HTTP
    API, the MCP server, and inbound federation at once — one surface, one policy.
- **Non-goal reminder:** v1 federates with peers you directly connect to. Relay/mesh
  fan-out, transitive discovery, and trust/reputation are future directions (§9).

### 6.8 Server & web client
- `3dam serve` starts the engine as a long-lived service from a **config file** that names
  the folders/sources to index, the listen address/port, auth settings, and analysis
  settings. On start it scans and watches those sources, then stays up serving requests.
- Exposes an **HTTP/WebSocket API** covering the same library operations as the local
  clients (browse, search, similarity, tags, previews, convert jobs) — this same API is what
  federated clients query. Live updates (scan/analysis progress, new assets) push over
  WebSocket.
- Serves a **browser-based web client** — the same three-region workspace as the desktop
  GUI, delivered as a **separate React + CSS web app** (with WASM only for the 3D viewer and
  hot render paths; see §7), so a library can be browsed from any device on the network with
  no install. Being DOM-based, it degrades responsively to tablet/phone widths — desktop and
  tablet first, phones graceful-degrade: the three-region workspace collapses to a single
  scrollable column at tablet width, with 44×44px minimum touch targets. Full small-screen
  polish and real-device testing are tracked UX debt, not a v1 gate (responsive detail in
  [tech-spec 09](tech-spec/09-server-and-web-client.md)).
- The desktop GUI and CLI can **connect** to a running server as their whole backend
  (e.g. `3dam --connect host:port …`) — distinct from adding it as a federated *source*.
- Also exposes a **built-in MCP server for AI agents on the same port** (§6.10) — no separate
  daemon, behind the same auth as everything else, and **switchable off** like every other
  capability (§6.11).
- **Administered by feature flags.** Which capabilities the server exposes — remote access, auth
  mode, user accounts, the MCP server, network writes, inbound federation — are named flags,
  **off by default**, set in the config file *and* editable at runtime from a dedicated
  **Settings / Administration** area in the web client. The two are coequal: the config seeds
  the state, the admin UI edits the same state, and the API/CLI can read and set it too, so a
  headless operator never needs the browser (§6.11).
- **Access control:** binds locally by default; exposing it beyond localhost requires explicitly
  enabling the remote-access flag *and* choosing an auth mode (§6.7). The admin surface warns
  before any exposure without auth. TLS and finer scopes tracked in §10.
- **Headless rendering is not guaranteed.** 3D thumbnail/turntable generation is
  GPU-accelerated (§7), but a serve host — a NAS, a container, a CI runner — often has no
  display server and no GPU. Server mode must therefore run rendering on a **software
  rasteriser** (e.g. Vulkan lavapipe / Mesa llvmpipe) when no GPU is present, and where even
  that is unavailable, **degrade gracefully** (serve metadata, geometry stats, and previews
  rendered elsewhere; defer or skip on-server renders) rather than fail — consistent with the
  fail-soft rule (§8). The fallback strategy is an open question (§10).

### 6.9 CLI
- Full parity: `scan`, `search`, `similar`, `convert`, `tag`, `export`, `source` (incl.
  adding file *and* federated sources), and `serve` (server mode), plus `--connect
  <host:port>` to use a remote server as the backend.
- Machine-readable output (`--json`), `--dry-run`, meaningful exit codes; safe for CI.
- Includes `mcp` — run the MCP server over stdio for a local agent (§6.10).
- Administers a server headlessly: read and set **feature flags** and manage **user accounts**
  from the CLI (and the config file), mirroring the web admin surface so nothing about
  configuring a server requires a browser (§6.11).

### 6.10 MCP server (AI-agent interface)

3DAM exposes its library to LLM agents through a built-in **MCP (Model Context Protocol)**
server, so an assistant can search, inspect, tag, convert, and audit a library the same way
a person does — but programmatically. This is directly inspired by the sibling **MoGen**
project, which ships `mogen mcp` as a stdio server wrapping every CLI subcommand as a tool.
3DAM follows the same instinct — *the whole product surface, reachable by an agent* — and
goes further on two axes: it runs **on the same port as the rest of the server** (no second
daemon), and it is a **first-class in-process adapter over `LibraryService`**, not a
subprocess shim.

- **One port, one endpoint.** In `serve` mode the MCP server is mounted on the **same server
  and listen port** as the HTTP/WS API and web client (§6.8), at a dedicated path
  (e.g. `POST /mcp`) using the MCP **Streamable HTTP** transport. There is no separate MCP
  port, process, or config block — one bind address, one auth surface, one TLS cert, and one
  firewall rule cover the API, the web client, federated peers, *and* agents. This is the
  headline difference from MoGen (which is stdio-only): everything under one port.
- **Also stdio, for local agents.** `3dam mcp` runs the same tool surface over **stdio**
  against an embedded engine — no network, no running server — the drop-in mode for editors
  and desktop agents (Claude Desktop, Cursor, and similar) that spawn the binary directly.
  Same tools, same results, different transport; this mirrors what MoGen ships, offered
  alongside the flagship single-port HTTP transport.
- **A frontend over the engine, not a wrapper over the CLI.** Because every client already
  talks to the engine through one `LibraryService` boundary (§4.2), the MCP server is just
  another consumer of it — peer to the HTTP API and the web client. It calls the engine
  **in-process** and returns **structured results**, rather than (as MoGen must, to avoid
  `exit()`/`println!` in its command functions) spawning a subprocess and scraping stdout.
  It therefore inherits, for free: federation fan-out, authentication, and the
  non-destructive guarantees below.
- **Comprehensive surface — tools, resources, and prompts** (MCP's three primitives, where
  MoGen exposes only tools):
  - **Tools (actions).** *Read:* `search` (text + facets, including the license facet, §5),
    `find_similar` (by asset id or an uploaded reference), `get_asset`, `list_sources`,
    `list_tags`, `find_duplicates`, `library_stats`. *Write (gated — see safety):* `tag` /
    `untag`, `set_license`, `add_source`, `scan` / `rescan`, `convert` (a batch
    convert/compress/optimize job), `export` (manifest/metadata). Tool names mirror the CLI
    verbs (§6.9) so the two surfaces stay learnable together.
  - **Resources.** Assets and previews are addressable so an agent can pull them into
    context: `3dam://asset/{id}` (JSON metadata including the license/rights block),
    `3dam://asset/{id}/preview` (thumbnail PNG / waveform as MCP image content), plus
    sources, saved searches, and smart folders — via resource templates for parameterised
    addressing.
  - **Prompts.** A small set of canned workflows an agent can invoke directly — e.g.
    *audit licenses* (surface unknown / no-commercial assets and the missing-attribution
    list), *review duplicates*, and *find-similar-then-export-a-manifest*.
- **Federated by construction.** Because the tools sit on `LibraryService`, `search` and
  `find_similar` **fan out across federated peers** and merge / re-rank exactly as the HTTP
  API does (§4.4), each hit tagged with its origin peer. An agent pointed at one server
  transparently reaches the whole federation that server is connected to — and license
  travels with every result (§5).
- **Safe by default, non-destructive always.** Reads are always available; writes honour §8 —
  never overwrite a source silently, outputs go to a chosen location, and expensive or
  mutating tools support a **dry-run** and return structured diffs. When the server is exposed
  beyond localhost, the MCP surface defaults to **read-only** unless writes are explicitly
  enabled in config, and every call is subject to the same **auth + scope** as the rest of the
  API (§6.7) — an anonymous caller sees only what an anonymous browser would.
- **Switchable off entirely.** The served MCP endpoint is a **feature flag** (§6.11): one toggle
  in the admin surface or config turns the whole agent interface off, and turning it off
  *removes* the `POST /mcp` route and its tools from the server rather than leaving them dormant
  behind auth. An operator who does not want agent access carries none of its surface. (The flag
  governs the network endpoint; the local `3dam mcp` stdio mode is a separate, opt-in CLI
  invocation over an embedded engine and is unaffected.)

### 6.11 Server administration — feature flags, authentication & user accounts

A 3DAM server is administered through **feature flags**: named capabilities that are **off by
default** and switched on either declaratively in the `3dam serve` config file or from a
**Settings / Administration** area in the web client. The two control planes are **coequal** —
the config seeds the state, the admin UI edits the same persisted state (§5), and the API/CLI
can read and set it too — so nothing is browser-only and a headless operator is never blocked.

- **Safe-by-default posture.** Out of the box a server binds to localhost, requires no auth, has
  no user accounts, keeps the MCP agent server and inbound federation off, and is read-only to
  the network. Every capability beyond that is a flag an operator consciously enables — the
  local-first, zero-config experience (and the "no account" principle, §2) is the default, not a
  thing you have to switch back on.

- **The v1 flag set:**
  - **Remote access / bind** — localhost-only (default) vs a bind address reachable on the
    LAN/internet. Exposing beyond localhost requires choosing an auth mode first.
  - **Authentication** — off / anonymous · token / API key · OIDC/OAuth2 (§6.7). One gate for
    the web client, API, MCP, and inbound federation.
  - **User accounts** — off by default (single-tenant). When on, named accounts with roles and
    scopes (below).
  - **MCP agent server** — on/off, and when on, read-only vs writes-enabled (§6.10). This is the
    switch that turns the agent interface off entirely.
  - **Network writes** — server read-only vs writes-enabled (tag, set_license, convert, scan,
    export); defaults to read-only whenever bound beyond localhost, independent of transport.
  - **Inbound federation** — whether this server answers federated queries from peers, and under
    which auth (§4.4, §6.7).
  - **Remote client connect** — whether GUI/CLI `--connect` sessions are accepted as a backend.
  - **Analysis & watch** — which extractors run and whether sources are watched/auto-rescanned
    (§6.2), so a low-powered host can serve a static catalog without background work.

- **User accounts & roles (opt-in).** Off by default; when enabled, each account carries hashed
  credentials, a **role**, and an optional visibility scope:
  - **admin** — manage feature flags, sources, and other accounts;
  - **editor** — everything a viewer can do plus writes (tag / set_license / convert / scan /
    export), subject to the network-writes flag;
  - **viewer** — read-only browse, search, similarity, preview.
  - An optional **visibility scope** limits which sources and collections an account sees.
    Accounts gate the web client, the API, federated access, *and* the MCP server uniformly —
    everything sits on the one auth surface (§6.7, [ADR 0004](adr/0004-feature-flags-admin.md)) —
    and an account's scope is the ceiling on what it can reach in any client. Identity records
    live in the server's own store, never in the portable library file or in exports.

- **The admin surface — beautifully presented.** The web client's **Settings / Administration**
  area (admin-only) is where flags are toggled, and it is designed to be a first-class,
  legible surface, not a raw config dump:
  - Capabilities are grouped into **cards by area** — Access, Authentication, Accounts,
    Agents / MCP, Federation, Analysis — each a labelled **toggle** with a title, one-line
    description, and current state.
  - Enabling a capability **reveals its sub-options inline** — turning on Authentication reveals
    the mode picker; turning on Accounts reveals the user list and role editor; turning on the
    MCP server reveals the read-only/writes choice.
  - **Consequential toggles carry a warning and a confirm** — exposing the server without auth,
    or enabling MCP/network writes beyond localhost, shows a clear risk note before it takes
    effect (the no-surprises rule, DESIGN_GUIDELINES §3.4).
  - Flags that need a **restart** are marked as such; the rest apply live.
  - Visuals follow the dark-first, low-chrome, information-dense language (DESIGN_GUIDELINES §4):
    status colours for on / off / at-risk, no decorative chrome — the settings recede, the state
    is obvious at a glance.

- **Auditable & reversible.** Every flag change is logged with who and when. Turning a capability
  off **removes its surface** — the route, the tools, the endpoint — rather than leaving it
  dormant behind auth, so "off" genuinely reduces attack surface (§6.10, above).

## 7. Technology direction (candidate stack)

Choices to be validated by spikes; listed to establish direction, not to lock in.

- **Language:** Rust across core, CLI, and GUI.
- **GUI:** a native, GPU-accelerated Rust UI toolkit — **`egui`/`eframe`**
  ([ADR 0005](adr/0005-gui-toolkit-egui.md)) — with **wgpu** for the embedded 3D viewer and
  custom thumbnail/waveform rendering.
- **Database:** embedded **SQLite** (via `rusqlite`/`sqlx`) for metadata; the vector index for
  similarity is a **sidecar HNSW (`usearch`)** ([spike](../spikes/vector-index/README.md)), with
  `sqlite-vec` retained for small libraries / exact re-rank. Local, single-file, portable.
- **Audio:** `symphonia` (decode) + FFT/DSP crates (`rustfft`/`realfft`) for feature
  extraction; playback via `cpal`/`rodio`.
- **Image:** `image` + `imageproc`; perceptual hashing (`img_hash`); optional GPU decode.
- **3D:** `gltf` and format loaders (glTF/FBX/OBJ/etc.), `wgpu` for rendering turntable
  thumbnails and the interactive viewer; mesh optimisation via `meshopt`-style tooling.
- **ML / embeddings:** on-device inference via **`candle`** ([ADR 0006](adr/0006-inference-runtime-candle.md);
  ONNX Runtime `ort` a feature-gated fallback) to run CLIP-style image models, audio embedding
  models, and shape/multi-view models — all local, no cloud calls.
- **File sources:** SFTP via `russh`/`ssh2`; SMB/Samba via an SMB client crate; behind the
  common `Source` trait.
- **Federated sources:** a 3DAM-server `Source` implementation that satisfies the same trait
  by calling a peer's API (search, facets, similarity-by-vector, preview fetch) instead of
  reading bytes — so the query engine treats local index and remote peers uniformly, then
  merges and re-ranks.
- **Server & API:** an async HTTP/WebSocket server (e.g. `axum`/`hyper` on `tokio`) mounting
  the same operations the clients call locally; the client↔engine boundary is one
  `LibraryService` trait with in-process and API-client implementations.
- **MCP server:** the Rust MCP SDK (**`rmcp`**, the same crate MoGen uses) exposing library
  operations as tools/resources/prompts. Mounted on the shared `axum` server via the
  **Streamable HTTP** transport (one port, §6.10) and also runnable over **stdio** (`3dam
  mcp`). Unlike MoGen's subprocess-per-tool design, tools call `LibraryService` in-process.
- **Authentication:** token/API-key and OIDC/OAuth2 (open standards) via crates like
  `oauth2`/`openidconnect`; credentials in the OS secret store (`keyring`); TLS via
  `rustls`. Server enforces; each federated source carries its own auth config.
- **Feature flags & user accounts:** server capabilities are runtime flags persisted in the
  server's own config store (a small versioned table beside the metadata DB, seeded by the
  config file), read and written by both the config loader and the admin API so there is one
  source of truth. User accounts are server-side identity records with hashed credentials
  (e.g. `argon2`) and a role/scope, layered on the same auth crates. All of it is exposed over
  the serve API so the admin UI, the CLI, and the config file are interchangeable control
  planes (§6.11).
- **Web client:** a **separate front-end codebase** served by `3dam serve` — an ordinary
  **React + CSS** app for the workspace (browse grid, search, tags, panels, navigation, and the
  **Settings / Administration** surface for feature flags and accounts, §6.11),
  with **WASM/`wgpu` islands only for the parts that need it** (the interactive 3D viewer and
  any hot render paths like waveforms/thumbnails) embedded in the DOM layout. Talks to the
  engine purely over the serve API. The DOM shell gets responsive layout, touch, text input,
  and accessibility for free; WASM is reserved for the heavy canvas. See
  [ADR 0008](adr/0008-web-client-stack.md) and [ADR 0009 §9](adr/0009-v1-scope-decisions.md)
  for why this leads over egui-in-WASM and why it ships before the desktop GUI.
- **Concurrency:** bounded worker pools (`rayon` for CPU-parallel analysis, async runtime
  for I/O-bound source access) feeding an incremental, non-blocking pipeline.

## 8. Cross-cutting requirements

- **Performance:** 60 fps browsing at 100k+ visible-scale libraries; instant search;
  analysis saturates cores without blocking the UI. Design for 1M+ assets and
  out-of-core datasets.
- **Reliability:** fail-soft on bad files and offline sources; no crash from one bad asset.
- **Privacy:** no telemetry, no unsolicited network calls, no account.
- **Portability:** one engine codebase, three OSes; documented DB and plain-text export.
  Cross-platform binaries + native installers (`.deb`/`.msi`/`.dmg`) ship from a tag-triggered
  CI matrix — see [tech-spec 15](tech-spec/15-observability-config-testing-packaging.md) §15.5
  (Packaging & release).
- **Reproducibility:** versioned analysis; explainable automated results.

## 9. Phasing (indicative)

> This phasing tracks **capability maturity** (what works). The **front-end sequence is
> web-first** — the React + CSS web client ships before the egui desktop GUI, and the
> `LibraryService`/serve API is pulled early to support it (see §7 and
> [ADR 0008](adr/0008-web-client-stack.md)). This section is the single source of truth for
> build phasing; where a step mentions a GUI or web client, that web-first ordering applies.

1. **Foundation:** `3dam-core` skeleton, unified schema, local source scanning, SQLite
   store, basic grid/table browser + text search, CLI `scan`/`search`.
2. **Media depth:** per-type decode + preview (waveform, image, 3D viewer), thumbnail
   cache, conversion pipeline (CLI-first).
2b. **Media breadth — video + documents:** a fourth and fifth `MediaType` so a source is
   catalogued *completely*. Deliberately shallower than the three deep types, and numbered
   `2b` rather than inserted as a new phase because it re-enters phase 2's capability line
   (media handlers) and renumbering would invalidate the `phase-6`/`phase-7`/`phase-8`
   labels already in use on issues. In wall-clock it lands after phase 6.
   - **Video** — detect `mp4`/`mov`/`mkv`/`webm`/`avi`/`m4v`/`ogv`, disambiguating
     `mp4`/`m4a` from the audio matrix by inspecting tracks rather than trusting the
     extension; cheap tier (duration, dimensions, fps, codec, container, bitrate,
     has-audio); a poster-frame thumbnail at ~10% duration; preview is the browser's native
     `<video>` over the existing content route with range support — no WASM island. Decode
     is a **discovered `ffmpeg`/`ffprobe` binary**, degrading to a typed tile with
     filesystem-only metadata when absent ([ADR 0014](adr/0014-video-decode-backend.md)).
     No transcoding: video is not a convert-pipeline target.
   - **Documents** — detect `pdf`/`md`/`txt`/`rtf`/`docx`/`odt`; cheap tier (page/word
     count, title/author where the container carries it, encoding, plus a short excerpt).
     The tile is an **excerpt card rendered in the DOM** from that excerpt, and the preview
     is an inspector text panel — neither is a server-side raster and neither is a WASM
     island. Rasterising a PDF's first page server-side was considered and declined: it
     needs a native PDF renderer (pdfium) and a font stack — the same dependency class
     [ADR 0014](adr/0014-video-decode-backend.md) declined for video — to produce a tile
     strictly worse than typeset DOM text that themes and stays selectable.
     `csv`/`json` are **excluded** — they are structured data, not prose, and belong to a
     later "data" type if they are ever wanted. Ingest applies an **ignore policy** so a
     source tree's `README`s, `.gitignore`s, and vendored licence boilerplate cannot drown
     the catalog.
   - **Documents are the first media type whose content is language**, so they join search
     differently: extracted text becomes a column on the existing `asset_fts` index, ranked
     in two levels — a categorical tier that puts any filename/token/tag match above every
     body-text-only match, then weighted bm25 within each tier. The tier is not belt-and-braces:
     bm25 saturates term frequency, so a document that repeats a word enough times reaches the
     same score ceiling as a filename match and no fixed weight separates them. Similarity comes
     from a **text** embedding space. Cross-media similarity between a
     document and an image is meaningless and is never offered — the `EmbeddingSpace` seam
     already keys on media, and the UI scopes accordingly.
3. **Automation:** feature extraction + embeddings per media type, similarity search,
   auto-tag/auto-categorise, duplicate detection, review UX.
4. **Reach:** SFTP + SMB sources, watch/auto-rescan, smart folders, export/manifests,
   CLI/GUI parity hardening.
5. **Server & web:** factor the client↔engine boundary into `LibraryService`, add
   `3dam serve` (config-driven indexing + HTTP/WS API + MCP endpoint on the same port),
   `3dam mcp` (stdio), `--connect` for GUI/CLI, and the web client; **feature flags with the
   Settings / Administration surface** (§6.11) and basic access control (anonymous + token
   auth), including the toggle that turns the MCP server off.
6. **Federation & auth:** 3DAM-server source type, federated query fan-out + merge/re-rank,
   similarity-by-vector across peers, per-source credentials, OIDC/OAuth2 support, and
   **opt-in user accounts with roles/scopes** (§6.11) layered on the same auth surface.
7. **Polish & scale:** performance work at 1M assets, accessibility, packaging/distribution
   for all three OSes.
8. **Future — asset networks:** peer relay/mesh fan-out, instance discovery, and trust /
   reputation so communities and stores can form large shared catalogs (beyond v1).

## 10. Open questions

- ~~GUI toolkit final choice (egui vs Iced vs other).~~ **Decided:** `egui`/`eframe`
  ([ADR 0005](adr/0005-gui-toolkit-egui.md)); the perf spike is a validation follow-up.
- ~~Which concrete embedding models per media type.~~ **Researched** ([`spikes/embedding-models/`](../spikes/embedding-models/README.md)):
  **SigLIP 768-d** (image; + DINOv2 for dedup), **LAION-CLAP 512-d** (audio, via `ort`),
  **multi-view→SigLIP 768-d** (3D). Runtime `candle` ([ADR 0006](adr/0006-inference-runtime-candle.md));
  audio forces the `ort` fallback. A follow-up code spike validates on-domain quality + latency
  before dims freeze.
- ~~Vector index: embedded extension vs standalone crate; on-disk vs in-memory at scale.~~
  **Decided:** sidecar HNSW (`usearch`) as the primary index, `sqlite-vec` for small libraries /
  exact re-rank ([spike](../spikes/vector-index/README.md)).
- ~~Extent of write-back to sources (rename/relocate) vs strictly-read-only default.~~
  **Decided:** strictly **read-only default** in v1; write-back (rename/relocate) is opt-in and
  **post-v1**.
- ~~Format coverage matrix for v1 vs later.~~ **Decided** ([ADR 0009 §8](adr/0009-v1-scope-decisions.md)):
  v1 decodes PNG/JPEG/WebP/TIFF/GIF/BMP/DDS/KTX2, WAV/FLAC/OGG/MP3/AAC-MP4, glTF/OBJ/FBX(decode)/PLY/STL;
  encode = glTF family + OBJ; USD decode and FBX/USD encode post-v1.
- ~~Web client approach: shared Rust→WASM view code vs a separate web UI.~~ **Decided:**
  separate **React + TypeScript + Tailwind** web app on Vite/pnpm, WASM only for viewer/render
  islands, built before the desktop GUI ([ADR 0008](adr/0008-web-client-stack.md)). Remaining
  detail — how WASM islands are packaged/fed data — decided in
  [ADR 0009 §9](adr/0009-v1-scope-decisions.md).
- ~~Server auth/security model & TLS.~~ **Decided** ([ADR 0009 §4](adr/0009-v1-scope-decisions.md)):
  rate-limit + lockout + CSRF; static `rustls` cert/key (ACME post-v1); **bind beyond localhost
  without TLS is refused** unless `--insecure`.
- ~~**Feature-flag store & lifecycle.**~~ **Decided** ([ADR 0009 §2](adr/0009-v1-scope-decisions.md)):
  versioned `server.db` table, per-flag `config_authority` (default `seed-only`, `reconcile`
  opt-in, no auto-revert), live-by-default + a frozen restart-only set.
- ~~**User-accounts scope for v1.**~~ **Decided** ([ADR 0009 §3](adr/0009-v1-scope-decisions.md)):
  fixed `admin`/`editor`/`viewer`, source/collection visibility, config-bootstrap recovery,
  session 14d inactivity / 90d max. Custom roles and per-asset scoping are post-v1.
- ~~**Headless 3D rendering on GPU-less servers.**~~ **Decided:** wgpu renders headless with a
  **software-raster fallback** (Mesa lavapipe/llvmpipe); the fallback ladder is validated by
  [`spikes/headless-render/`](../spikes/headless-render/README.md) ([ADR 0001](adr/0001-3d-render-backend.md)).
- ~~**Cross-peer similarity.**~~ **Decided:** advertise an embedding-space id and **gate
  cross-peer ranking on an exact match**, falling back to **per-peer-ranked grouped results**
  when spaces differ; a shared/negotiated space is deferred ([spike](../spikes/cross-peer-similarity/README.md):
  same-space rank corr 0.817 vs ~0 for mismatched, zero-error gate).
- ~~**Federated query semantics.**~~ **Decided** ([ADR 0009 §5](adr/0009-v1-scope-decisions.md)):
  2.5 s fixed deadline, partial results flagged, `total = None`, accept cursor drift, LRU
  peer-cache min(2 GB, 10% disk) / 7-day TTL.
- ~~**Federation protocol & versioning.**~~ **Decided** ([ADR 0009 §5](adr/0009-v1-scope-decisions.md)):
  versioned subset of the read API + `advertise()` carrying `protocol_version` + `space_id`; newer
  peers degrade to the caller's version; bearer-token auth first (OIDC federation post-v1).
- ~~**MCP surface & safety.**~~ **Decided** ([ADR 0009 §6](adr/0009-v1-scope-decisions.md)): a small
  purpose-tool set + resources, read-only by default, per-tool opt-in writes gated on auth beyond
  localhost, no transitive peer MCP.
- ~~**License taxonomy & detection.**~~ **Decided** ([ADR 0009 §1](adr/0009-v1-scope-decisions.md)):
  **no defaults — unknown stays unknown, never inferred.** Hybrid representation (SPDX id |
  Proprietary | Custom | NULL) + tri-state rights flags; licences recorded only from explicit
  declarations; per-asset overrides via an `'inherited'` provenance. 3DAM records and surfaces
  licence — it is not legal advice.

---

See also: [MISSION.md](MISSION.md) · [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md)
