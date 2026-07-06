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
| 15 | [15-observability-config-testing-packaging.md](15-observability-config-testing-packaging.md) | Logging/tracing, the error taxonomy, config precedence, the testing strategy and scale fixtures, and packaging/release mechanics (expanding the roadmap's release section). |

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
questions (PRODUCT_SPEC §10) and the roadmap's ([ROADMAP.md](../ROADMAP.md)).

**Needs a spike (gate-level):**
- **Headless 3D render on a GPU-less server** — software-raster fallback (lavapipe/llvmpipe)
  vs CPU thumbnail path vs render-on-demand by a GPU client. The gate on [ADR 0001](../adr/0001-3d-render-backend.md);
  owned by [06](06-3d-render.md). Until it resolves, the server's on-render degradation path
  is provisional.
- **GUI toolkit final choice** (egui/eframe vs Iced) — needs the rendering/perf spike, then an
  ADR. Owned by [12](12-desktop-gui.md).
- **Cross-peer similarity** — ranking hits across peers needs compatible embedding spaces
  (`EmbeddingSpace` match); advertise + gate, negotiate, or per-peer-grouped fallback. Owned by
  [05](05-analysis-similarity-dedup.md)/[07](07-sources-and-federation.md).

**Model & analysis:**
- Concrete embedding models per media type (quality/size/speed on-device); `candle` vs `ort`.
  [05](05-analysis-similarity-dedup.md).
- Vector-index storage: embedded extension (`sqlite-vec`) vs sidecar HNSW; on-disk vs in-memory
  at 1M+. [02](02-data-model-and-storage.md)/[05](05-analysis-similarity-dedup.md).

**Data & licence:**
- Licence taxonomy: SPDX vs a 3DAM rights model for non-code assets; auto-detection (pack
  manifests/sidecars/LICENSE) vs user-set; per-asset overrides within a blanket-licensed pack.
  [02](02-data-model-and-storage.md).

**Server, auth & flags:**
- Feature-flag store & lifecycle: where state persists, how config-file and admin-UI edits
  reconcile, which flags flip live vs need a restart. [10](10-auth-accounts-and-flags.md).
- User-accounts scope for v1: fixed vs custom roles, where visibility scoping bottoms out
  (source/collection vs per-asset), first-admin bootstrap, session/token lifetime.
  [10](10-auth-accounts-and-flags.md).
- Server auth/security hardening for exposure beyond localhost; TLS extent. [10](10-auth-accounts-and-flags.md).

**Federation:**
- Federated-query semantics: timeouts, partial results, pagination/result caps across N peers,
  how much of a peer's catalog to cache. [07](07-sources-and-federation.md).
- Federation protocol & versioning (the inter-instance API contract as the seed of a future
  mesh). [07](07-sources-and-federation.md).

**MCP & CLI:**
- MCP surface & safety: tool granularity, how far to lean on resources/prompts vs tools, exact
  write-tool gating beyond localhost. [11](11-mcp-server.md).
- **CLI-verb ↔ MCP-tool-name parity** (`similar`/`find_similar`, `source add`/`add_source`) —
  the product docs are themselves in tension; pick one spelling convention.
  [11](11-mcp-server.md)/[13](13-cli.md).
- **Export job DTO** — reconciliation added `submit_export` to `LibraryService`
  ([03](03-library-service-and-api.md)) to back the `export` CLI verb + MCP tool, but its
  `ExportRequest`/return shape must be pinned against the manifest model in
  [08](08-convert-pipeline.md)/[03](03-library-service-and-api.md).

**Web, GUI & build:**
- Exact React stack (bundler/router/state) and how WASM viewer islands are packaged and fed
  data from the DOM. [09](09-server-and-web-client.md) / [ROADMAP](../ROADMAP.md).
- How much view logic (if any) is shared between the web client and the egui desktop GUI.
  [12](12-desktop-gui.md).
- Windowed-rows virtualisation vs the live-update stream — reconciliation against 03's
  pagination contract. [12](12-desktop-gui.md)/[03](03-library-service-and-api.md).
- Code signing & notarization (macOS/Windows) and distribution channels beyond GitHub
  Releases. [15](15-observability-config-testing-packaging.md) / [ROADMAP](../ROADMAP.md).

**Format coverage:**
- v1-vs-later format/codec matrix per media; in particular **FBX is decode-only in v1** (encode
  targets are the glTF family + OBJ), which the product spec frames as "FBX↔glTF" — to confirm.
  [04](04-media-handlers.md)/[08](08-convert-pipeline.md).

---

See also: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) · [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) ·
[ROADMAP.md](../ROADMAP.md) · [ADRs](../adr/)
