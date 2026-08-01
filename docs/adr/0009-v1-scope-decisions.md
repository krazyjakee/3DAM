# ADR 0009 — v1 scope decisions: resolving the remaining open questions

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §9–§10,
tech-spec files 02–15, [ADR 0004](0004-feature-flags-admin.md)

## Context

After the gate-level spikes closed (headless render, GUI toolkit, inference runtime, vector
index, cross-peer similarity — ADRs 0001/0005/0006 + spikes) and the web stack was fixed
([ADR 0008](0008-web-client-stack.md)), a long tail of **scope/decision** open questions remained
across the tech-spec area files and the two roll-ups (PRODUCT_SPEC §10, [00-overview](../tech-spec/00-overview.md#open-questions)).
None of them needed a spike — they needed a **v1 line drawn**. This ADR draws it in one place so
the area files stop carrying "unresolved" and implementation can start from decided ground.

Each area file's own "Open questions" section is **superseded by the matching section here**; the
files retain their bullets as detail/rationale but point to this ADR for the decision. The one
remaining *spike* — concrete embedding models per media type — is out of scope for this ADR and
is being researched separately (`spikes/embedding-models/`).

Guiding principle throughout: **v1 is minimal, safe-by-default, and never assumes.** Where a
decision below sets a numeric default, the value is *data* (config/table), revisitable without a
code change; "post-v1" means deliberately deferred, not rejected.

## Decisions

### 1. Licence taxonomy & detection — [02](../tech-spec/02-data-model-and-storage.md), [10](../tech-spec/10-auth-accounts-and-flags.md)

- **No defaults. Unknown is unknown.** 3DAM **never infers or assumes** a licence. A licence is
  recorded only from an **explicit, unambiguous declaration** — an SPDX id in a manifest/sidecar,
  a recognised `LICENSE`/EULA file, or store metadata that states terms. Absent that:
  `license_id = NULL`, `license_status = 'unknown'`, `license_provenance = 'unknown'`. No guessing
  from filename or heuristics, ever.
- **Representation is hybrid.** `license_id` = an SPDX id when the licence is a recognised SPDX
  licence; otherwise `'Proprietary'` (stated commercial/EULA terms), `'Custom'` (bespoke licence
  text), or `NULL` (unknown). `license_status ∈ {permissive, attribution, restricted, unknown}` is
  a derived badge bucket, set **only** when a licence is known. `rights_commercial` /
  `rights_attribution` are **tri-state** (true/false/unknown) and default **unknown**.
- **Per-asset overrides.** `license_provenance` gains `'inherited'`. A pack/source may carry a
  blanket licence that flows to its assets as `'inherited'`; an asset's own columns with any other
  provenance **override** the inherited value. Nothing declared → unknown, *not* inherited.
- **Provenance enum:** `'declared' | 'sidecar' | 'manifest' | 'store' | 'inherited' | 'user' | 'unknown'`.

### 2. Feature-flag store & lifecycle — [10](../tech-spec/10-auth-accounts-and-flags.md), [02](../tech-spec/02-data-model-and-storage.md), [13](../tech-spec/13-cli.md)

- **Store:** versioned `feature_flag` table in the server store (`server.db`), seeded by the
  config file (as already specified).
- **Two writers:** per-flag `config_authority`. **Default `seed-only`** — the config sets the
  initial value; the admin UI/CLI owns it thereafter. `reconcile` (config watched and re-applied)
  is **opt-in**. The config file is **never watched-and-reverted by default**.
- **Live vs restart:** flags are **live by default** via the router's live-remount; a **frozen
  restart-only set** = bind address/port, TLS config, auth-mode enable/disable, storage paths.
- **`admin` in embedded mode ([13](../tech-spec/13-cli.md)):** against a plain embedded library
  with no serve store, `admin` **may seed an initial config** (create the server store + flag rows)
  but **rejects runtime-only ops** (session/token management) that need a running server.

### 3. User accounts — [10](../tech-spec/10-auth-accounts-and-flags.md)

- **Fixed roles** `admin`/`editor`/`viewer` for v1; **custom roles are post-v1.**
- **Visibility bottoms out at source + collection level;** **per-asset scoping is post-v1.**
- **Recovery:** the **config-file bootstrap escape hatch** is the only recovery for a lost sole
  admin in v1 (re-open bootstrap via config). No email/recovery flow in v1.
- **Lifetimes (defaults, configurable):** session inactivity **14 days**, absolute max **90 days**;
  API tokens **long-lived, no expiry by default**, optional `expires`.

### 4. Server auth / security & TLS — [10](../tech-spec/10-auth-accounts-and-flags.md)

- **Hardening (ships when `UserAccounts` is on):** auth-endpoint rate limiting; account lockout
  after **10 failed attempts / 15-min window**; CSRF protection for the session cookie
  (SameSite=Strict + double-submit token).
- **TLS:** static cert/key via `rustls` in v1; **ACME is post-v1.**
- **Binding beyond localhost without TLS is refused by default** — an explicit `--insecure`
  override is required and logs a prominent warning. (Refuse-unless-overridden, not merely warn.)

### 5. Federation — [07](../tech-spec/07-sources-and-federation.md), [03](../tech-spec/03-library-service-and-api.md)

- **Query deadline:** **2.5 s** soft default; peers past it contribute nothing and the response is
  marked `partial: true` with the timed-out peer list. **Fixed, not adaptive** in v1.
- **Result caps:** per-peer over-fetch `k = requested_page_size`; merge top-N globally.
- **`total` under fan-out ([03](../tech-spec/03-library-service-and-api.md)):** **always `None`**
  (UI shows "N+") — no true cross-peer count in v1.
- **Cursor stability ([03](../tech-spec/03-library-service-and-api.md)):** **accept eventual drift**
  — no first-page query snapshot in v1; document that boundaries may drift if a peer re-ranks.
- **Catalog cache:** peer previews + listing metadata only, **LRU, cap = min(2 GB, 10% free disk)**,
  federated-preview **TTL 7 days**; the local cache **mints its own validators** (peer `ETag` not
  trusted end-to-end).
- **Protocol & versioning:** the stable federation API is a **versioned subset of [03](../tech-spec/03-library-service-and-api.md)'s
  read surface** (`search`, `get_asset`, `list_facets`, preview fetch) + `advertise()`.
  `advertise()` carries a semver `protocol_version` and the `space_id` set (ADR-0009 §? / cross-peer
  spike). Newer peers **degrade to the caller's version**; unknown fields ignored (forward-compatible).
  Auth: **bearer token first**, OIDC federation post-v1.

### 6. MCP surface & CLI parity — [11](../tech-spec/11-mcp-server.md), [13](../tech-spec/13-cli.md)

- **Granularity:** a **small set of purpose tools** mirroring the CLI verbs (search, find_similar,
  get_asset, source list/add, convert, export, edit_tags, set_license) — not one mega-tool, not
  dozens. Catalog browsing leans on **MCP resources**; **prompts** used sparingly.
- **Write gating:** **read-only by default.** Write tools are **opt-in per-tool** via config and
  **require an auth scope** when the server is bound beyond localhost; on a non-localhost bind,
  writes are disabled unless explicitly enabled **and** authenticated.
- **Transitive federation:** peers' MCP endpoints are **not** reachable transitively — this server
  exposes only its own fan-out tools over peer catalogs.
- **Verb ↔ tool parity:** **keep each surface idiomatic** (CLI `similar`/`source add`; MCP
  `find_similar`/`add_source`) but back both with **one internal verb registry**, and carry the
  explicit CLI↔MCP name map in [11](../tech-spec/11-mcp-server.md) §3 so they cannot silently drift.

### 7. Convert / export — [03](../tech-spec/03-library-service-and-api.md), [08](../tech-spec/08-convert-pipeline.md), [13](../tech-spec/13-cli.md)

- **`ExportRequest`** = `{ selection, profile, destination, manifest: bool }`; returns a
  **`JobHandle`** (async, progress on the event stream) whose terminal `JobSummary` carries the
  manifest.
- **Selection grammar unified** across all batch verbs (`convert`, `export`): **ids ∪ query ∪
  saved-search ref.**
- **Convert manifest is a first-class, persisted record** (queryable "what did I export where"),
  not a transient summary.
- **Commit idempotency:** a re-submitted job **skips already-produced identical outputs by
  `content_hash`** rather than re-encoding.
- **Profiles:** **built-in presets only** in v1; user-definable profiles post-v1.
- **Estimation:** `EstConfidence::Approx` heuristics for v1; real-encode-sample calibration and
  cross-media sampling are post-v1 (header-only at scale).

### 8. Format coverage matrix — [04](../tech-spec/04-media-handlers.md), [08](../tech-spec/08-convert-pipeline.md)

- **v1 decode:** images — PNG, JPEG, WebP, TIFF, GIF, BMP, **DDS, KTX2**; audio — WAV, FLAC,
  OGG/Vorbis, MP3, **AAC/MP4**; 3D — glTF/glb, OBJ, **FBX (decode-only)**, **PLY, STL**.
  **USD decode is staged post-v1** (crate maturity).
- **v1 encode targets:** **glTF family (.gltf/.glb) + OBJ** only; **FBX/USD encode post-v1.**
  (Confirms the "FBX↔glTF" framing = FBX decode, glTF encode.)
- **Metadata-before-decoder** staging retained: a format may be catalog-only (cheap sniff) before
  its preview/convert lands.
- **Geometry counts:** where accessor counts are absent, **mark approximate** (`counts_estimated`)
  rather than a second ingest pass; exactness is on-demand expensive-tier.
- **Cheap-tier trailer reads:** the **one bounded trailer seek** budget stands; formats needing
  more defer those fields to the expensive tier.

### 9. Web islands, GUI & transport — [09](../tech-spec/09-server-and-web-client.md), [12](../tech-spec/12-desktop-gui.md), [03](../tech-spec/03-library-service-and-api.md)

- **WASM model-viewer island:** `wasm-pack` ES module + `.wasm`, lazy dynamic `import()`, embedded via
  the same `rust-embed` step; **WebGPU with WebGL2 fallback.** The §B.3 wasm-bindgen contract
  (`init/load_model/set_camera/resize/drop`) is the v1 shape. Issue #148 refined the original
  packaging boundary: **waveforms use lightweight Canvas2D over server peaks** so audio never loads
  wgpu; thumbnails stay server-rendered previews.
- **View-logic sharing:** **each frontend owns its own presentation** over the file-03 API in v1;
  **no shared view-logic crate** (revisit post-v1 only if duplication proves costly). The
  `LibraryService` seam is the only shared layer.
- **Windowed-rows reconciliation:** **periodic coalesced window refresh** (re-fetch the visible
  range on live invalidation) rather than per-row splice into a sorted window.
- **Streamed-query transport ([03](../tech-spec/03-library-service-and-api.md)):** **NDJSON-over-HTTP**
  in v1 (simple, stateless); WS stays for live events; backpressure revisited post-v1.
- **Event delivery ([03](../tech-spec/03-library-service-and-api.md)):** **at-least-once with a
  resume cursor**; server buffers bounded history (≈ last 1000 events / 5 min) for reconnects;
  slow clients get coalesced progress but **`AssetAdded` is never dropped.**
- **Bulk-write result shape ([03](../tech-spec/03-library-service-and-api.md)):** **summary count +
  warnings**, not a streamed per-asset result.
- **`ByUpload` seed ([03](../tech-spec/03-library-service-and-api.md)):** the reference blob is
  embedded via a **server round-trip through the analysis pipeline**; federated peers accept only a
  **precomputed vector** (matching `space_id`), not a raw upload.
- **Desktop GUI stays fully native (egui)** — **no embedded webview** in v1.

### 10. CLI ergonomics & packaging — [13](../tech-spec/13-cli.md), [15](../tech-spec/15-observability-config-testing-packaging.md)

- **`--json` schema versioning:** per-record-type `schema` ids version **independently**; a
  `--schema-version` pin is **post-v1.**
- **Partial-failure exit code:** keep the single code `6`; the `summary` record's structured detail
  suffices (no finer code split).
- **Ship `clap_complete` shell completions + `clap_mangen` man pages** in release artifacts.
- **Distribution:** **GitHub Releases is the sole v1 channel.** A Homebrew tap and `cargo-binstall`
  metadata are fast-follow post-v1; winget/AUR are community-driven, not v1-owned.

### 11. Concurrency / storage knobs — [14](../tech-spec/14-concurrency-performance-reliability.md), [02](../tech-spec/02-data-model-and-storage.md)

- **`spawn_blocking` pool cap:** add an **explicit cap** on the blocking pool (bounds FDs under a
  source-storm), sized from the aggregate per-source I/O widths.
- **Single-huge-item resume:** **no intra-item checkpointing in v1** — cancellation's floor stays
  one item; chunked convert is post-v1.
- **CI baseline runner:** ratios are measured against a **fixed reference runner**
  (`ubuntu-latest`, 4-core) for CPU/IO; **GPU-render benchmarks run on a self-hosted/dev GPU box**,
  not GPU-less CI.
- **Change-detection strict mode ([02](../tech-spec/02-data-model-and-storage.md)):** offer an
  **opt-in `strict` content-hash-only mode**; default stays the `(size, mtime)` fast gate.
- **Cache size policy ([02](../tech-spec/02-data-model-and-storage.md)):** default cap
  **min(10 GB, 10% free disk)**, LRU per tier; the `embed/` tier is **evictable** (regenerable from
  vectors) but weighted to evict last.

## Still open after this ADR

- **Concrete embedding models per media type** — the one remaining *spike*, in research now
  (`spikes/embedding-models/`); gates the vector dims ([05](../tech-spec/05-analysis-similarity-dedup.md) §2.1)
  and the analysis-lane sizing ([14](../tech-spec/14-concurrency-performance-reliability.md)).
- **Calibration, not decisions** — dedup/tileability thresholds ([05](../tech-spec/05-analysis-similarity-dedup.md) §8),
  byte-budget fraction and runtime split ([14](../tech-spec/14-concurrency-performance-reliability.md)): these are
  tuned against the real scale-fixture / benchmark harness ([15](../tech-spec/15-observability-config-testing-packaging.md)),
  and stay standing perf work rather than one-off calls.

## Consequences

- The tech-spec area files can drop "unresolved / open / needs a spike" language for everything
  above; their Open-questions sections now point here.
- Every numeric default is table/config data, so tuning them post-v1 is not a code change.
- Deferred ("post-v1") items are explicit, so scope creep into v1 is visible in review.
