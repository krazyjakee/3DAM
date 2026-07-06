# 3DAM — Technical Specification

Status: **Draft v0.1** · Scope: engineering design beneath the product spec.

This tech spec is the **low-level design layer** for 3DAM. Where
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) defines *what* 3DAM is and the shape of its parts,
this document set defines *how* it is built: crate boundaries, traits and their methods,
the database schema, the API surface, the analysis pipeline, the render path, the server,
and the cross-cutting engineering concerns. It is written to be **implementable** — a
contributor should be able to read one area file and start building that area.

It does not restate the product rationale (read the product spec for that) and it does not
re-decide anything already settled in an [ADR](../adr/) — it *cites* those and fills in the
mechanics.

---

## How to read this

The spec is split into area files so each can be owned and evolved independently. Read the
overview first, then the area(s) you are working in. Cross-file dependencies are noted at
the top of each file.

| # | File | Owns |
|---|------|------|
| 00 | [00-overview.md](00-overview.md) | This index; conventions; the layered picture; open-questions roll-up. |
| 01 | [01-architecture-and-crates.md](01-architecture-and-crates.md) | Cargo workspace, crate boundaries and dependency direction, embedded-vs-connected wiring, compile-time feature gating. |
| 02 | [02-data-model-and-storage.md](02-data-model-and-storage.md) | SQLite schema (assets, media attributes, tags, collections, sources, license, jobs), IDs & hashing, migrations, vector-index storage, derivative/blob cache, the separate server config/flags/accounts store. |
| 03 | [03-library-service-and-api.md](03-library-service-and-api.md) | The `LibraryService` trait and its DTOs, the error model, pagination/streaming, and the HTTP/WebSocket API that mirrors it. |
| 04 | [04-media-handlers.md](04-media-handlers.md) | The `MediaHandler` trait, format detection, the cost-tiered `extract_metadata` vs `thumbnail`/`extract_features` split, and the per-media format/codec matrix. |
| 05 | [05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md) | The analysis pipeline, per-media embeddings and inference runtime, the similarity/vector index and ANN, exact + near-duplicate detection, auto-tag/auto-categorise, tileability, and extractor versioning/re-analysis. |
| 06 | [06-3d-render.md](06-3d-render.md) | The `3dam-render` (wgpu) crate: headless render-to-PNG, the software-raster fallback, surface sharing with the GUI viewer, and the multi-view render feeding shape embeddings. Implements [ADR 0001](../adr/0001-3d-render-backend.md) / [ADR 0002](../adr/0002-3d-render-crate-boundary.md). |
| 07 | [07-sources-and-federation.md](07-sources-and-federation.md) | The `Source` trait; file sources (local FS, SFTP, SMB) and watching; the federated (3DAM-server) source; query fan-out, merge/re-rank, cross-peer similarity, and offline/partial-result handling. |
| 08 | [08-convert-pipeline.md](08-convert-pipeline.md) | The convert/compress/optimise job model per media type, batching, dry-run, and the non-destructive output policy. |
| 09 | [09-server-and-web-client.md](09-server-and-web-client.md) | `3dam serve` (axum), the serve config file, routing and WS live updates, embedding the built web assets, and the React + CSS web-client architecture with its WASM/wgpu viewer islands. |
| 10 | [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md) | The auth layer (anonymous/token/OIDC-OAuth2), credential storage, scopes; the feature-flag store and its live-vs-restart lifecycle; opt-in user accounts/roles; and the admin API + audit log. Implements [ADR 0004](../adr/0004-feature-flags-admin.md). |
| 11 | [11-mcp-server.md](11-mcp-server.md) | The `rmcp`-based MCP server: Streamable HTTP on the shared port + `3dam mcp` stdio, the tools/resources/prompts inventory, the in-process `LibraryService` adapter, and write-gating/flag-removal. Implements [ADR 0003](../adr/0003-mcp-server.md). |
| 12 | [12-desktop-gui.md](12-desktop-gui.md) | The native GUI shell, the three-region workspace, the virtualised grid/table, the inspector, embedding the wgpu 3D viewer, and keyboard/dark-mode behaviour. |
| 13 | [13-cli.md](13-cli.md) | The CLI command tree (clap), human vs `--json`/`--csv` output, exit codes, `--dry-run`, `--connect`, and the `serve`/`mcp` subcommands. |
| 14 | [14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md) | The tokio (I/O) + rayon (CPU) split, bounded worker pools, the incremental non-blocking pipeline, out-of-core data at 1M+ assets, cancellation, fail-soft, and the performance targets/benchmarks. |
| 15 | [15-observability-config-testing-packaging.md](15-observability-config-testing-packaging.md) | Logging/tracing, the error taxonomy, config precedence, the testing strategy and scale fixtures, and packaging/release mechanics (the canonical §15.5 release section). |

---

## Conventions used across the spec

- **Rust, one workspace.** Everything is Rust unless stated; the web client (React + CSS) is
  the sole non-Rust codebase and is covered in [09](09-server-and-web-client.md).
- **Crate names** are written `3dam-core`, `3dam-render`, etc. (see
  [01](01-architecture-and-crates.md) for the authoritative list).
- **Trait-first.** Seams are traits — `LibraryService`, `MediaHandler`, `Source`. Area files
  give method signatures in Rust-ish pseudocode; they are indicative, not frozen APIs.
- **Spec references** cite the product spec by section, e.g. (PRODUCT_SPEC §6.2), and ADRs by
  number, e.g. ([ADR 0001](../adr/0001-3d-render-backend.md)).
- **Open questions** raised inside an area file are collected under an "Open questions" heading
  at the foot of that file, and rolled up in §Open questions below so they are findable in one
  place.
- **Non-destructive and fail-soft** ([DESIGN_GUIDELINES](../DESIGN_GUIDELINES.md) §1.3, §2) are
  invariants, not features — every area assumes them.

## The layered picture (orientation)

```
        front-ends                     transports
   ┌──────────┬──────────┬──────────┐
   │ desktop  │   CLI    │   web    │   ──►  in-process call  (embedded)
   │  GUI(12) │  (13)    │ client(9)│   ──►  HTTP/WS API (03) (connected)
   └────┬─────┴────┬─────┴────┬─────┘
        └──────────┴──────────┘
                   ▼   depends on the LibraryService trait (03)
        ┌───────────────────────────────────────────────┐
        │                  3dam-core                     │
        │  library/query · scan/watch · analysis(05)     │
        │  · convert(08) · serve extras: API/MCP/web(9,11)│
        └──┬───────────────┬───────────────┬─────────────┘
           ▼               ▼               ▼
   ┌───────────────┐ ┌───────────┐ ┌──────────────────────┐
   │ media handlers│ │  sources  │ │       storage        │
   │   (04) +      │ │   (07)    │ │  metadata DB + vector │
   │ 3dam-render(6)│ │           │ │  index + blob cache(2)│
   └───────────────┘ └───────────┘ └──────────────────────┘
```

Auth, feature flags, and accounts (10) wrap the serve-mode surfaces (03, 09, 11).
Concurrency/perf (14) and observability/testing/packaging (15) are cross-cutting.

## Cross-file contracts (canonical names)

The area files define traits and types that neighbours depend on. Because the files were
drafted independently, each shared name has **one authoritative owner**; other files consume
it and must not redefine it. This registry is the tie-breaker when two files disagree.

| Type / trait | Canonical name & shape | Owner | Consumed by |
|---|---|---|---|
| Engine facade | `LibraryService` (async trait) — method names here are authoritative | [03](03-library-service-and-api.md) | 01, 07, 09, 11, 12, 13 |
| Error type | `LibError` (typed enum) + wire `ErrorBody { code, message, detail }` | [03](03-library-service-and-api.md) | 09 (HTTP mapping), 13 (exit codes), 15 (categories) |
| Result envelope | `Page<T>` + opaque `Cursor`; `PartialStatus`/`PeerStatus`/`ItemWarning` (fail-soft carriers) | [03](03-library-service-and-api.md) | 07, 12, 14 |
| Asset DTOs | `AssetSummary` (list rows, carries `Origin`) vs full `Asset` (inspector) | [03](03-library-service-and-api.md) | 07, 11, 12 |
| Crate names | `3dam-core`, `3dam-render`, `3dam-media`, `3dam-sources`, `3dam-store`, `3dam-api`, `3dam-client`, `3dam-server`, `3dam-gui`, `3dam-cli`, `3dam` | [01](01-architecture-and-crates.md) | all |
| Storage tables | `asset`, `audio_attr`/`image_attr`/`model_attr`, `tag`/`asset_tag`, `collection`/`collection_member`, `source`, `job` (library.db); `feature_flag`/`account`/`audit_log` (server.db) | [02](02-data-model-and-storage.md) | 03, 05, 07, 10 |
| IDs & hashing | id = `BLOB(16)` UUIDv7; `content_hash` = `BLOB(32)` BLAKE3 | [02](02-data-model-and-storage.md) | 03, 05, 07 |
| Handler trait | `MediaHandler` (`detect`/`extract_metadata`(cheap)/`decode`/`thumbnail`/`extract_features`(expensive)) | [04](04-media-handlers.md) | 05, 06, 08 |
| Handler output | `MediaAttributes` = `Audio`/`Image`/`ModelAttributes`; `FeatureBundle` (carries `extractor_versions`) | [04](04-media-handlers.md) | 02, 05 |
| Convert types | `ConvertJob`, `ConvertTarget`, `RunMode` (`DryRun`/`Preview`/`Commit`), `OutputPolicy`; `MediaEncoder` trait | [08](08-convert-pipeline.md) | 03, 11, 13, (encoder on 04) |
| Render API | `Renderer`, `SceneDesc`, `render_thumbnail(_png)`, `render_multiview(…, ViewSet)`, `FramingVersion` | [06](06-3d-render.md) | 04, 05, 12 |
| Embedding-space id | `EmbeddingSpace { model_id, model_version, media, dim, metric }` | [05](05-analysis-similarity-dedup.md) | 02 (index scoping), 07 (cross-peer gating) |
| Auth type | `AuthContext { identity, scopes, visibility, via }`; `Scope { Read, Write, Admin, McpUse, Federate }` | [10](10-auth-accounts-and-flags.md) | 03, 07, 09, 11, 13 |
| Feature flags | `FlagKey { RemoteAccess, Authentication, UserAccounts, McpServer, NetworkWrites, InboundFederation, RemoteConnect, AnalysisWatch }`; `McpServer` is tri-state `Off`/`ReadOnly`/`ReadWrite` | [10](10-auth-accounts-and-flags.md) | 09, 11, 13 |
| Execution primitives | `Stage<I,O>` bounded pool, `cpu()` handoff, `CancellationToken`; tokio(I/O)+rayon(CPU) split | [14](14-concurrency-performance-reliability.md) | 04, 05, 07, 08 |
| Client credentials | per-source, `keyring`-backed `AuthConfig`; **never** in the library file or in a DTO | [07](07-sources-and-federation.md) / [10](10-auth-accounts-and-flags.md) | 02 (ref only), 03 |

**Surface-vs-engine naming.** CLI verbs ([13](13-cli.md)) and MCP tool names ([11](11-mcp-server.md))
are user-facing surfaces and may read differently from the `LibraryService` method they call
(e.g. CLI `similar` and MCP `find_similar` both invoke the same engine method). The *engine*
method name (file 03) is the contract; the surface names are ergonomics. The exact CLI-verb ↔
MCP-tool-name parity is an open question (below).

## Status of this document set

Draft v0.1 — all sixteen files exist and cover their areas. They were drafted area-by-area
and reconciled once against the shared-name registry above; expect the usual first-draft
rough edges at the seams. The tech spec follows the product docs, which are themselves Draft
v0.1 — where this spec and a product-spec or ADR statement disagree, the product spec / ADR
wins and this spec is the bug.

## Open questions (roll-up)

Each area file keeps its own "Open questions" list; the load-bearing ones are collected here
with their owning file. This complements, and does not replace, the product spec's open
questions (PRODUCT_SPEC §10) and its build phasing (PRODUCT_SPEC §9).

> **Most of the tail below is resolved.** The scope/decision questions (licence, flags, accounts,
> auth, federation semantics, MCP/CLI, convert-export, format matrix, web islands, packaging) were
> decided on **2026-07-06 in [ADR 0009](../adr/0009-v1-scope-decisions.md)** — see it for the v1
> line and rationale. What genuinely remains: the **embedding-model spike** (research in
> `spikes/embedding-models/`) and a few **calibration** knobs tuned against the benchmark harness.

**Needs a spike (gate-level):**
- ~~**Headless 3D render on a GPU-less server**~~ — **✅ Resolved 2026-07-06** by
  [`spikes/headless-render/`](../../spikes/headless-render/README.md): wgpu renders headless to
  PNG on real GPU, and the **fallback ladder was exercised** — rung 2 (`force_fallback_adapter`)
  and rung 3 (simulated GPU-less via the lavapipe ICD) both render correctly through llvmpipe.
  The gate on [ADR 0001](../adr/0001-3d-render-backend.md); owned by [06](06-3d-render.md).
- ~~**GUI toolkit final choice**~~ — **Decided: egui/eframe** ([ADR 0005](../adr/0005-gui-toolkit-egui.md),
  2026-07-06). The rendering/perf spike is now a validation follow-up, not a gate. Owned by [12](12-desktop-gui.md).
- ~~**Cross-peer similarity**~~ — **✅ Decided 2026-07-06** by
  [`spikes/cross-peer-similarity/`](../../spikes/cross-peer-similarity/README.md): **advertise an
  `EmbeddingSpace` `space_id` and gate cross-peer ranking on exact match**, with per-peer-grouped
  fallback when spaces differ; negotiation deferred (it's a re-embedding job, not a handshake).
  Same-space rank corr **0.817**, mismatched spaces **~0** (gate: 0 false ±). Owned by
  [05](05-analysis-similarity-dedup.md)/[07](07-sources-and-federation.md). Remaining: content-address
  the id on the model-artefact sha256 (05 §2.3), and validate the corr threshold on real checkpoints.

**Model & analysis:**
- ~~**Concrete embedding models per media type**~~ — **Researched 2026-07-06**
  ([`spikes/embedding-models/`](../../spikes/embedding-models/README.md)): **SigLIP 768-d** (image,
  candle-native; + DINOv2 384-d dedup), **LAION-CLAP 512-d** (audio, via `ort`), **multi-view→SigLIP
  768-d** (3D, reuses headless render). Runtime `candle` ([ADR 0006](../adr/0006-inference-runtime-candle.md));
  **audio forces the `ort` path** (no candle CLAP). Remaining: a follow-up *code* spike for on-domain
  quality + real latency before dims freeze. [05](05-analysis-similarity-dedup.md).
- Vector-index storage: embedded extension (`sqlite-vec`) vs sidecar HNSW; on-disk vs in-memory
  at 1M+. [02](02-data-model-and-storage.md)/[05](05-analysis-similarity-dedup.md).
  **✅ Decided 2026-07-06** by [`spikes/vector-index/`](../../spikes/vector-index/README.md):
  **sidecar HNSW (usearch) is the primary index** (sub-ms/query at 1M, recall tunable to ~100%);
  `sqlite-vec` is exact-but-O(N) (726 ms/query at 1M) → kept for small libraries / exact re-rank.
  Remaining: quantization (f16/i8) for memory, and validation on real embeddings.

**Data & licence:** — all **decided in [ADR 0009 §1](../adr/0009-v1-scope-decisions.md)**.
- ~~Licence taxonomy & detection~~ — **No defaults; unknown-is-unknown** (never inferred). Hybrid
  representation (SPDX id | Proprietary | Custom | NULL), tri-state rights flags, per-asset
  overrides via an `'inherited'` provenance. [02](02-data-model-and-storage.md).

**Server, auth & flags:** — all **decided in [ADR 0009 §2–4](../adr/0009-v1-scope-decisions.md)**.
- ~~Feature-flag store & lifecycle~~ — versioned `server.db` table, per-flag `config_authority`
  (default `seed-only`, `reconcile` opt-in, no auto-revert), live-by-default + frozen restart-only
  set. [10](10-auth-accounts-and-flags.md).
- ~~User-accounts scope~~ — fixed `admin`/`editor`/`viewer`, source/collection visibility, config
  bootstrap recovery, session 14d/90d. Custom roles + per-asset scoping post-v1. [10](10-auth-accounts-and-flags.md).
- ~~Auth hardening & TLS~~ — rate-limit + lockout + CSRF; static `rustls` cert/key (ACME post-v1);
  **bind beyond localhost without TLS refused** unless `--insecure`. [10](10-auth-accounts-and-flags.md).

**Federation:** — all **decided in [ADR 0009 §5](../adr/0009-v1-scope-decisions.md)**.
- ~~Query semantics~~ — 2.5 s fixed deadline, partial results flagged, `total = None`, accept cursor
  drift, LRU peer-cache min(2 GB, 10% disk) / 7-day TTL. [07](07-sources-and-federation.md).
- ~~Protocol & versioning~~ — versioned subset of 03's read surface + `advertise()` carrying
  `protocol_version` + `space_id`; newer peers degrade; bearer-token auth first. [07](07-sources-and-federation.md).

**MCP & CLI:** — all **decided in [ADR 0009 §6–7](../adr/0009-v1-scope-decisions.md)**.
- ~~MCP surface & safety~~ — small purpose-tool set + resources, read-only default, per-tool
  opt-in writes gated on auth beyond localhost, no transitive peer MCP. [11](11-mcp-server.md).
- ~~CLI-verb ↔ MCP-tool-name parity~~ — keep each surface idiomatic, back both with **one verb
  registry** + an explicit name map (no forced 1:1). [11](11-mcp-server.md)/[13](13-cli.md).
- ~~Export job DTO~~ — `ExportRequest{selection, profile, destination, manifest}` → `JobHandle`;
  unified selection grammar; manifest persisted first-class. [03](03-library-service-and-api.md)/[08](08-convert-pipeline.md).

**Web, GUI & build:**
- ~~Exact React stack~~ — **Decided: React + TypeScript + Tailwind** on Vite/pnpm
  ([ADR 0008](../adr/0008-web-client-stack.md), 2026-07-06). *WASM-island packaging & data
  handoff* remains open. [09](09-server-and-web-client.md) / PRODUCT_SPEC §9.
- ~~WASM-island packaging & data handoff; view-logic sharing; windowed-rows reconciliation~~ —
  **Decided in [ADR 0009 §9](../adr/0009-v1-scope-decisions.md)**: `wasm-pack` islands (WebGPU +
  WebGL2 fallback), each frontend owns its own presentation (no shared view crate), periodic
  coalesced window refresh, NDJSON streamed queries, native GUI (no webview).
  [09](09-server-and-web-client.md)/[12](12-desktop-gui.md)/[03](03-library-service-and-api.md).
- ~~Where the frontend-shared backend helper lives~~ — **Decided: a small `3dam-frontend` crate**
  (2026-07-06) holds `open_backend`/`Backend` + `classify`/`Role` so `3dam-gui` need not link the
  clap tree. [01](01-architecture-and-crates.md)/[12](12-desktop-gui.md)/[13](13-cli.md).
- ~~Code signing & notarization~~ — **Decided: unsigned for v1** (accept Gatekeeper/SmartScreen
  warnings; revisit post-v1). ~~Distribution channels~~ — **GitHub Releases only for v1**; Homebrew
  + `cargo-binstall` fast-follow ([ADR 0009 §10](../adr/0009-v1-scope-decisions.md)).
  [15](15-observability-config-testing-packaging.md) / PRODUCT_SPEC §9.

**Format coverage:** — **frozen in [ADR 0009 §8](../adr/0009-v1-scope-decisions.md)**.
- ~~v1-vs-later format/codec matrix~~ — v1 decodes PNG/JPEG/WebP/TIFF/GIF/BMP/**DDS/KTX2**,
  WAV/FLAC/OGG/MP3/**AAC-MP4**, glTF/OBJ/**FBX (decode-only)**/**PLY/STL**; encode targets =
  glTF family + OBJ. **USD decode and FBX/USD encode are post-v1.**
  [04](04-media-handlers.md)/[08](08-convert-pipeline.md).

---

See also: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) · [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) ·
[ADRs](../adr/)
