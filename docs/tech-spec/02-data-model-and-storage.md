# 02 — Data model & storage

Status: **Draft v0.1** · Scope: the persistence layer — SQLite schema, IDs & content hashing, migrations, vector-index storage, the on-disk derivative/blob cache, and the separate server store.

This file owns *how the catalog is stored*. It gives concrete `CREATE TABLE` sketches for the unified asset model (PRODUCT_SPEC §5), the per-media attribute tables, tags/collections/sources, and the first-class license/rights columns; it fixes the content-hash and id conventions that drive dedup and change detection; it pins the migration mechanism; it frames the vector-index storage decision; and it lays out the derivative cache directory structure. It does **not** own how vectors are *produced or queried* (that is [05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md)), the `LibraryService` DTOs that read/write these rows ([03-library-service-and-api.md](03-library-service-and-api.md)), the `MediaHandler` extraction split that fills the attribute tables ([04-media-handlers.md](04-media-handlers.md)), the `Source` trait semantics ([07-sources-and-federation.md](07-sources-and-federation.md)), or the feature-flag/account **semantics** ([10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md)) — this file only defines *where those live and their table shape*. Where this spec and PRODUCT_SPEC / an ADR disagree, the product doc wins.

---

## 1. Storage overview

Two physically separate stores, deliberately (PRODUCT_SPEC §5 end):

```
  <data-dir>/                         platform data dir (see §9)
  ├── library.db                      ── THE PORTABLE LIBRARY (catalog only)
  │      assets, media attrs, tags,
  │      collections, sources (no secrets),
  │      license/rights, jobs, schema_version
  │
  ├── vectors/                        ── vector index (sidecar HNSW, §7 — ADR 0016)
  │      image.hnsw / audio.hnsw / shape.hnsw   (derived; vectors themselves live in library.db)
  │
  ├── cache/                          ── derivative/blob cache (§8) — regenerable
  │      thumbnails, waveforms, embeddings, renders, keyed by content-hash + extractor version
  │
  └── server.db                       ── SEPARATE SERVER STORE (§10)
         feature flags, user accounts, audit log — NEVER in library.db, never exported
```

Design invariants that shape everything below:

- **`library.db` is the portable artifact.** It is the "documented, stable, exportable" store of DESIGN_GUIDELINES §1.5 / PRODUCT_SPEC §6.6. It contains *only catalog data* — no secrets, no server identity, no flag state. You can copy it to another machine and it is a complete library; the `cache/` and `vectors/` dirs rebuild from it.
- **`cache/` and `vectors/` are derived and disposable.** Deleting them costs re-analysis time, never data. Every derived artifact is keyed so it can be invalidated and regenerated (§8.2).
- **`server.db` is host configuration, not catalog.** It lives beside the library but is a distinct file so it never travels in a library copy or an export ([10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md) owns its contents' meaning).
- **Local-first & non-destructive** (DESIGN_GUIDELINES §1.3): originals are referenced by path, never mutated; all derived data lives in 3DAM's managed store, never in the source tree.

---

## 2. IDs & content hashing

### 2.1 Identifiers

Every row that other tables reference uses a **stable, application-generated id**, not the source path (paths move; ids must not). Ids are **UUIDv7** stored as a 16-byte `BLOB`:

- UUIDv7 is time-ordered, so `PRIMARY KEY` inserts stay roughly sequential → good B-tree locality at 1M+ assets, unlike random UUIDv4.
- 16-byte `BLOB` beats a 36-char text UUID on index size and comparison cost.
- Generated in-process, so id assignment needs no DB round-trip and works offline / pre-insert.

Convention used across the spec: `asset.id`, `source.id`, `tag.id`, `collection.id` are all `BLOB(16)` UUIDv7. Federated (remote-owned) assets keep a **local** `asset.id` plus a `remote_ref` (§4) so they are addressable locally while remaining attributable to their peer.

### 2.2 Content hash — the dedup & change-detection key

Each *file-source* asset carries a **content hash** over the raw file bytes:

- **Algorithm: BLAKE3.** Fast (SIMD, multithreaded, faster than SHA-256 on the large binaries we hash), 256-bit, no length-extension worry. Stored as the raw 32-byte digest in `asset.content_hash BLOB(32)` (not hex — half the bytes, direct index compare).
- **What it keys:**
  - **Exact dedup.** Two assets with equal `content_hash` are byte-identical duplicates → the duplicate-review view (PRODUCT_SPEC §6.3) groups on this. (Near-duplicate detection is perceptual and lives in [05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md); it does *not* use this hash.)
  - **Change detection.** On re-scan, a cheap `(size, mtime)` stat gate decides *whether to rehash*; a changed hash means the file's content changed and its derivatives + analysis are stale (§8.2). This two-tier gate (stat first, hash only on stat-mismatch) keeps re-scan cheap at scale.
  - **Derivative cache key.** Thumbnails/waveforms/embeddings are stored under the content hash (§8), so identical bytes in two locations share one cached derivative and one embedding.
- **Federated assets have no local content hash** (we never read their bytes — PRODUCT_SPEC §4.4). Their `content_hash` is `NULL`; dedup/identity for them rides on `remote_ref` instead.

> Perceptual/similarity hashes (pHash for images, etc.) are a *separate* concept produced by the analysis pipeline and stored in the media-attribute tables (§3.3), not here. This §2.2 hash is exact-bytes only.

---

## 3. The library schema (`library.db`)

SQLite in **WAL** journal mode (concurrent readers during a write scan), `foreign_keys = ON`, `synchronous = NORMAL`. DDL below is indicative, not frozen.

**Connection handling** (shipped, issue #137; the full contract is [14](14-concurrency-performance-reliability.md) §4.3 and the module doc on `dam-store/src/db.rs`). WAL's concurrent readers only exist if there is more than one connection, so `Store` owns **one read-write connection** behind a mutex plus a **bounded pool of read-only connections** (CPU count clamped to 2–4, overridable with **`3DAM_DB_READERS`**), gated by a `maint` `RwLock` that `VACUUM` / `wal_checkpoint(TRUNCATE)` / migrations take exclusively to drain everyone. Reads run inside `BEGIN DEFERRED` so one call observes one snapshot; writes are always one `BEGIN IMMEDIATE` transaction, so a concurrent reader can never catch a half-applied edit and a second process on the same data dir waits on the busy handler instead of failing with `SQLITE_BUSY_SNAPSHOT`. Only the writer connection sets `journal_mode`/`synchronous` (they are persisted header properties); pooled readers add `query_only = ON`. An in-memory store (tests) is single-connection by construction and cannot be pooled.

### 3.1 `asset` — shared fields

The spine. One row per catalogued asset (local or federated), media-agnostic where possible.

```sql
CREATE TABLE asset (
    id              BLOB    PRIMARY KEY,          -- UUIDv7, 16 bytes
    content_hash    BLOB,                         -- BLAKE3 32 bytes; NULL for federated
    -- location / source ref
    source_id       BLOB    NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    path            TEXT    NOT NULL,             -- path within the source (or remote id path)
    filename        TEXT    NOT NULL,             -- original basename, for display/search
    size_bytes      INTEGER,                      -- NULL for federated (unknown/remote)
    -- times (unix epoch, ms; source_* from the file, scanned/analysed ours)
    source_created_at   INTEGER,
    source_modified_at  INTEGER,                  -- feeds the (size,mtime) change gate
    scanned_at          INTEGER NOT NULL,
    analysed_at         INTEGER,                  -- NULL until features extracted
    -- type
    media_type      TEXT    NOT NULL,             -- 'audio' | 'image' | 'model'
    format          TEXT    NOT NULL,             -- concrete: 'wav','png','gltf',...
    -- organisation (free-form, cheap)
    rating          INTEGER,                      -- user 0..5, NULL = unrated
    flags           INTEGER NOT NULL DEFAULT 0,   -- bitfield: favourite, hidden, ...
    notes           TEXT,
    -- license / rights (first-class — see §5; kept ON the asset row, prioritised in inspector)
    license_id          TEXT,                     -- SPDX id | 'Proprietary'|'Custom'|'Unknown'
    license_status      TEXT NOT NULL DEFAULT 'unknown',  -- 'permissive'|'attribution'|'restricted'|'unknown'
    rights_commercial   INTEGER,                  -- tri-state: 1 yes / 0 no / NULL unknown
    rights_modify       INTEGER,
    rights_redistribute INTEGER,
    rights_attribution  INTEGER,                  -- 1 = attribution REQUIRED
    attribution_holder  TEXT,                     -- author/holder
    attribution_credit  TEXT,                     -- exact credit string to reproduce
    license_url         TEXT,                     -- EULA / license source
    license_provenance  TEXT NOT NULL DEFAULT 'unknown',  -- 'declared'|'sidecar'|'manifest'|'user'|'unknown'
    -- derived refs (keys into the cache; content-hash-derived, so nullable pointers)
    thumbnail_key   TEXT,                          -- see §8; cache path derivable from content_hash
    preview_key     TEXT,
    -- provenance / bookkeeping
    remote_ref      TEXT,                          -- non-NULL ⇒ federated ref: peer-scoped asset id
    analysis_version INTEGER NOT NULL DEFAULT 0,   -- extractor bundle version last applied (§8.2)
    created_at      INTEGER NOT NULL,              -- row creation (ours)
    updated_at      INTEGER NOT NULL
) STRICT;

-- change-detection & dedup
CREATE INDEX idx_asset_hash        ON asset(content_hash);           -- exact-dup grouping
CREATE INDEX idx_asset_source_path ON asset(source_id, path);        -- re-scan reconciliation
CREATE INDEX idx_asset_media_type  ON asset(media_type, format);     -- type facet
CREATE INDEX idx_asset_license      ON asset(license_status, rights_commercial, rights_attribution);
CREATE INDEX idx_asset_analysis    ON asset(analysis_version);       -- find stale-for-re-analysis
CREATE UNIQUE INDEX uq_asset_remote ON asset(source_id, remote_ref) WHERE remote_ref IS NOT NULL;
```

Notes:
- **License lives as columns on `asset`, not a side table** (PRODUCT_SPEC §5, DESIGN_GUIDELINES §3.1) — it is prioritised, always-present, and a search facet, so it is denormalised onto the spine for a single-row read in the inspector and a single-index scan for the "safe-to-ship" smart folder. `license_status` is the pre-computed badge colour bucket so the facet query never re-derives it. §5 details the taxonomy.
- **Full-text search** over `filename`, `notes`, and confirmed tags is a `fts5` virtual table (`asset_fts`) kept in sync by triggers; the ranking/tokeniser detail is out of scope here (it is a query concern — see [03](03-library-service-and-api.md)), but the column choices above are its inputs.
- `flags` is a bitfield to avoid schema churn for boolean-ish states; documented constants live in `3dam-core`.

### 3.2 Per-media attribute tables

One table per media type, `asset_id` as both PK and FK — a strict 1:1 extension of `asset`, so a row exists only once its cheap metadata tier has run (PRODUCT_SPEC §6.2; the `extract_metadata` vs `extract_features` split is [04](04-media-handlers.md)'s). Nullable columns cover the "features not yet extracted" state.

```sql
CREATE TABLE audio_attr (
    asset_id     BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
    duration_ms  INTEGER,
    sample_rate  INTEGER,
    bit_depth    INTEGER,
    channels     INTEGER,
    bpm          REAL,                 -- detected; NULL if n/a
    musical_key  TEXT,                 -- e.g. 'A#min'
    loudness_lufs REAL,
    brightness   REAL,                 -- spectral descriptors
    harmonicity  REAL,
    class        TEXT                  -- 'one-shot'|'loop'|'sfx'|'music'... (auto)
) STRICT;

CREATE TABLE image_attr (
    asset_id     BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
    width        INTEGER,
    height       INTEGER,
    color_depth  INTEGER,
    has_alpha    INTEGER,              -- bool
    color_space  TEXT,
    dominant_colors TEXT,              -- small JSON array of packed RGB (display only)
    class        TEXT,                 -- 'texture'|'sprite'|'ui'|'concept' (auto)
    phash        BLOB,                 -- perceptual hash (near-dup — consumed by 05)
    -- tileability metric (PRODUCT_SPEC §5): score + classification, not a boolean
    tileability      REAL,             -- 0..1 edge-continuity score; NULL = not computed
    repeat_period    INTEGER,          -- internal repetition period in px; NULL = none/non-repeating
    tile_class       TEXT              -- 'seamless'|'tiled'|'non-tiling'
) STRICT;

CREATE INDEX idx_image_tileclass ON image_attr(tile_class);   -- 'seamless' facet
CREATE INDEX idx_image_phash     ON image_attr(phash);        -- near-dup candidate lookup (05)

CREATE TABLE model_attr (
    asset_id     BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
    vertex_count   INTEGER,
    triangle_count INTEGER,
    mesh_count     INTEGER,
    material_count INTEGER,
    texture_count  INTEGER,
    bbox_min       TEXT,               -- JSON [x,y,z]
    bbox_max       TEXT,
    has_rig        INTEGER,            -- bool
    has_animation  INTEGER,
    has_uv         INTEGER,
    class          TEXT                -- category guess (auto)
) STRICT;
```

The **embedding vectors** referenced by "per-view render embedding" (model), "audio embedding", and "image embedding" are **not** stored inline here — they live in the vector index (§7) and/or the blob cache (§8), keyed by `content_hash` + extractor version, with the *analysis* side ([05](05-analysis-similarity-dedup.md)) owning their production and dimensionality. These attribute tables hold the cheap, human-readable, sortable/facetable stats only.

### 3.3 Tags & the suggested/auto vs confirmed distinction

Tags are shared and free-form across media types (PRODUCT_SPEC §5) with a first-class **suggested/auto vs confirmed** state (DESIGN_GUIDELINES §1.2 — the system proposes, the user disposes).

```sql
CREATE TABLE tag (
    id     BLOB PRIMARY KEY,          -- UUIDv7
    name   TEXT NOT NULL,
    UNIQUE(name COLLATE NOCASE)
) STRICT;

CREATE TABLE asset_tag (
    asset_id  BLOB NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
    tag_id    BLOB NOT NULL REFERENCES tag(id)   ON DELETE CASCADE,
    state     TEXT NOT NULL,          -- 'suggested' | 'confirmed' | 'rejected'
    source    TEXT NOT NULL,          -- 'auto' | 'user'  (who created the association)
    confidence REAL,                  -- 0..1 for auto suggestions; NULL for user tags
    extractor  TEXT,                  -- which analyser proposed it (explainability, §1.2)
    explanation TEXT,                 -- concise visible reason for the proposal
    created_at INTEGER NOT NULL,
    PRIMARY KEY (asset_id, tag_id)
) STRICT;

CREATE INDEX idx_asset_tag_tag   ON asset_tag(tag_id, state);   -- "assets with tag X (confirmed)"
CREATE INDEX idx_asset_tag_state ON asset_tag(state, source);   -- "review all suggestions"
```

- `state = 'suggested'` is presented as **pending** on the wire/UI; `'confirmed'` is accepted (by user, or a user tag from creation); `'rejected'` is a remembered reject. Undo returns either decided state to pending. This tri-state powers one-action accept/reject/undo and keeps pending automation out of confirmed-only discovery.
- `extractor` + `confidence` + `explanation` satisfy the "explainable" requirement without a separate audit table. Re-analysis may refresh those fields only while the row is pending; confirmed/rejected decisions are immutable until explicit undo.
- Auto-category/class values are proposed as these same reviewable rows. Attribute-table `class`
  columns retain raw analyser output for diagnostics, but class/category filters use confirmed tag
  values. A correction is reject + add the correct manual class tag. Measured attributes remain ordinary facts.

### 3.4 Collections & smart folders

Manual sets and saved-query live sets, possibly spanning federated sources (PRODUCT_SPEC §5).

```sql
CREATE TABLE collection (
    id       BLOB PRIMARY KEY,
    name     TEXT NOT NULL,
    kind     TEXT NOT NULL,           -- 'manual' | 'smart'
    query    TEXT,                    -- smart only: serialised query (JSON) → re-run live
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- membership only for kind='manual'; smart folders compute membership from `query` at read time
CREATE TABLE collection_member (
    collection_id BLOB NOT NULL REFERENCES collection(id) ON DELETE CASCADE,
    asset_id      BLOB NOT NULL REFERENCES asset(id)      ON DELETE CASCADE,
    added_at      INTEGER NOT NULL,
    PRIMARY KEY (collection_id, asset_id)
) STRICT;
```

Smart folders persist a **serialised query** (`collection.query`, JSON) rather than a materialised member list — they are "live, self-updating" (DESIGN_GUIDELINES §3.3) and can include federated results, so materialising them in the DB would be stale by design. The query DSL is defined in [03](03-library-service-and-api.md); its fan-out across peers is [07](07-sources-and-federation.md)'s.

### 3.5 Sources — file vs federated, no secrets

Sources are first-class records with kind, connection info, and online/offline state. **The secret is never here** (DESIGN_GUIDELINES §1.5 / §2, PRODUCT_SPEC §6.7): the row holds only a *reference* to a credential in the OS keychain.

```sql
CREATE TABLE source (
    id           BLOB PRIMARY KEY,
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL,       -- 'local_fs' | 'sftp' | 'smb' | 'federated'
    -- connection info (non-secret): root path, host, port, share, base URL...
    connection   TEXT NOT NULL,       -- JSON, kind-specific, NO secrets
    -- auth: a REFERENCE only. Actual token/password lives in the OS secret store.
    auth_mode    TEXT,                -- 'none'|'anonymous'|'token'|'oidc'  (NULL for local_fs)
    auth_ref     TEXT,                -- keychain entry id, e.g. "3dam.source.<id>"; never the secret
    -- state
    online       INTEGER NOT NULL DEFAULT 1,   -- last-known reachability
    last_scanned_at INTEGER,
    watch        INTEGER NOT NULL DEFAULT 0,    -- FS watch enabled?
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
) STRICT;
```

- `kind` splits the two categories of PRODUCT_SPEC §4.4: **file sources** (`local_fs`/`sftp`/`smb`) whose bytes we process, and **federated** (`federated`) whose catalog we query. The `Source` trait behaviour is [07](07-sources-and-federation.md)'s; this row is just its persisted config.
- `online` + `last_scanned_at` support fail-soft (DESIGN_GUIDELINES §2 / PRODUCT_SPEC §6.1): an offline source is flagged, its cached assets stay usable.
- `auth_ref` is the load-bearing indirection: the library file stays safe to copy/export because it carries *no* credential material, only a pointer the local keychain resolves. Credential storage mechanics are [10](10-auth-accounts-and-flags.md)'s.

### 3.6 Jobs (convert / scan / analysis)

A lightweight persisted job table so long operations (scan, convert batches, re-analysis) are resumable and their progress survives a restart — supporting the incremental/cancellable/resumable requirement (DESIGN_GUIDELINES §1.1, PRODUCT_SPEC §6.1). The *pipeline* that drives these is [08](08-convert-pipeline.md) (convert) and [05](05-analysis-similarity-dedup.md) (analysis); the row shape:

```sql
CREATE TABLE job (
    id         BLOB PRIMARY KEY,
    kind       TEXT NOT NULL,         -- 'scan'|'analyse'|'convert'|'export'
    state      TEXT NOT NULL,         -- 'queued'|'running'|'paused'|'done'|'failed'|'cancelled'
    params     TEXT NOT NULL,         -- JSON: the request (source id, format, dry_run, ...)
    progress   REAL NOT NULL DEFAULT 0,
    total      INTEGER,
    error      TEXT,                  -- fail-soft: per-job error, never a crash
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_job_state ON job(state, kind);
```

---

## 4. Federated (remote-owned) assets

Federated assets are **read-only references** owned by a peer (PRODUCT_SPEC §4.4/§5): they live as ordinary `asset` rows (so one grid/search/facet path covers them) but are marked by `remote_ref IS NOT NULL` and `content_hash IS NULL`. We store *enough to list, filter, and rank* — identity, key attributes, tags, license (license travels with the asset, PRODUCT_SPEC §6.7), and remote preview refs — but never their bytes or regenerated derivatives. Their `thumbnail_key`/`preview_key` point at **fetched-on-demand, cached** remote previews (§8.3), not locally computed ones. `source_id` ties each to its origin peer so results stay attributable. Cache eviction and staleness for federated previews is §8.3.

---

## 5. License / rights model

License is first-class per-asset (PRODUCT_SPEC §5, DESIGN_GUIDELINES §3.1) — columns on `asset` (§3.1), not a buried note. The model:

| Field | Column | Meaning |
|---|---|---|
| **Identifier** | `license_id` | SPDX id where one exists (`CC-BY-4.0`, `CC0-1.0`, `MIT`, `OFL-1.1`, …) **or** the sentinel `Proprietary` / `Custom` / `Unknown`. Free text so non-SPDX asset licenses fit; validated against an SPDX list where it matches. |
| **Status bucket** | `license_status` | Pre-computed badge class: `permissive` / `attribution` / `restricted` / `unknown`. Denormalised so the inspector badge and the facet are one column read; derived from the id + rights at write time. |
| **Rights summary** | `rights_commercial`, `rights_modify`, `rights_redistribute`, `rights_attribution` | The four game-relevant permissions, **tri-state** (`1`/`0`/`NULL`) so "unknown" is distinct from "no". `rights_attribution = 1` means attribution is *required*. |
| **Attribution** | `attribution_holder`, `attribution_credit` | Author/holder and the exact credit string to reproduce in a credits export. |
| **Provenance** | `license_url`, `license_provenance` | License source URL/EULA, and where the value came from: `declared` (by the source/peer), `sidecar`, `manifest`, `user`, or `unknown`. |

Rules:
- **Unknown is never silently permissive** (PRODUCT_SPEC §5, DESIGN_GUIDELINES §3.1). Absence defaults to `license_status = 'unknown'` and `NULL` rights, which the inspector must render as explicitly unverified, never as safe.
- The **facet** "commercial use, no attribution required" is `rights_commercial = 1 AND rights_attribution = 0` — indexed by `idx_asset_license` — and drives the *safe-to-ship* smart folder.
- Auto-detection (from pack manifests, sidecars, `LICENSE`/readme files) writes `license_provenance` accordingly; how far to auto-detect vs require the user is an open question (§11, carried from PRODUCT_SPEC §10).

### 5.1 Maintained library and facet counts

`library_stats`, the source sidebar, and the confirmed-tag vocabulary read compact maintained
tables rather than counting `asset`/`asset_tag` at refresh time. `library_stat` owns total,
unanalyzed, and source counts; `media_stat` owns the global media mix; `source_stat` and
`source_media_stat` own source-scoped totals; `tag_stat` and `source_tag_stat` own confirmed and
manual tag counts. Asset/source/tag triggers update those rows in the same transaction as scan,
removal, analysis, retag, reclassification, and cascade deletion.

A visibility ceiling made only of whole-source grants is an exact sum of source aggregates. A
manual-collection grant is an arbitrary asset subset and may overlap a source grant, so that case
continues through the visibility join to preserve union-without-double-counting semantics. V26
backfills every aggregate while migrating an existing catalog; `Store::repair_aggregates` exposes
the same rebuild as one atomic, idempotent integrity-repair operation.

---

## 6. Migrations

- **Crate: `rusqlite`** (with the `bundled` SQLite feature for a pinned, portable engine and to guarantee the `fts5` / `json1` extensions we rely on). Chosen over `sqlx` because: (a) 3DAM is single-file embedded SQLite, not a networked pool — `sqlx`'s async pool and compile-time query checking against a live DB add ceremony we don't need for an embedded store; (b) `bundled` gives a reproducible engine across all three OSes (PRODUCT_SPEC §8 portability) without depending on the system SQLite; (c) synchronous calls fit our model where DB access already runs on a bounded blocking pool off the async runtime ([14](14-concurrency-performance-reliability.md) owns that split). This is a mechanics choice; if a later ADR revisits it, that ADR wins.
- **Versioned, forward-only.** A single integer schema version lives in SQLite's built-in `PRAGMA user_version`. Each store owns an append-only ordered list of embedded SQL steps `V1, V2, …`; on open it applies every step with number `> user_version` transactionally, then bumps `user_version`. No down-migrations — forward-only, matching the "documented, stable" promise (DESIGN_GUIDELINES §1.5): older binaries refuse a newer DB with a clear message rather than corrupting it. `library.db` and `server.db` have independent version sequences because neither may attach or modify the other.
- **Additive-first.** Prefer additive changes (new nullable column, new table, new index) so an in-progress library upgrades without a rewrite. Destructive column changes go through SQLite's 12-step table rebuild only when unavoidable.
- **Cache/vectors are not migrated** — they carry their own version tags (§8.2) and are regenerated on mismatch, so schema migrations never need to touch derived data.

---

## 7. Vector-index storage (this file owns the *storage* framing)

The similarity vectors (image/audio/shape embeddings) need an approximate-nearest-neighbour (ANN) index. **This file owns where and how vectors are stored on disk; [05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md) owns how they are produced, their dimensionality/model, and how queries are issued and re-ranked** (incl. cross-peer, PRODUCT_SPEC §4.4).

The two storage strategies that were on the table (PRODUCT_SPEC §7/§10), as the spike framed them:

| | `sqlite-vec` (extension inside `library.db`) | an HNSW crate (sidecar files under `vectors/`) |
|---|---|---|
| **On-disk** | one file — vectors ride in the library DB, one backup/copy unit | separate `*.hnsw` files beside the DB |
| **Portability** | vectors travel with the portable library automatically | must copy the sidecar too (or regenerate from cache) |
| **Query model** | SQL `WHERE embedding MATCH ?` — joins naturally with facet filters in one query | in-process ANN call, then join ids back to SQLite for facets |
| **Scale / memory** | **exact linear scan** — measured **726 ms/query at 1M×512** (spike) | HNSW graph; old spike raw K=10 was sub-ms, while final high-recall product K=10 over-fetch measured **8.817 ms** and 99.6% candidate recall at 1M |
| **Ops** | no extra process, no extra file | `usearch` base is deserialized into process memory; lifecycle and graph RSS are measured explicitly |

**Storage decision — [ADR 0016](../adr/0016-vector-index-backend.md), on the evidence of the
[vector-index spike](../../spikes/vector-index/README.md) (2026-07-06):**
The spike benchmarked both at 1M×512-d. `sqlite-vec 0.1.x` `vec0` is an **exact linear scan** (100%
recall but **726 ms/query** at 1M — ~1500× slower than the ANN, far past "instant", and ~10× slower
than our own in-process rayon exact scan at **68 ms/query**). The old spike's raw K=10 `usearch`
lookup was sub-millisecond; the final #141 configuration instead spends **8.817 ms** at 1M to fetch
80 candidates with **99.6% candidate recall**, at the cost of a ~1.21 GiB graph RSS delta and a
background build. **Decision: the primary similarity index is a sidecar HNSW under
`vectors/`, built on pinned `usearch` 2.25.3** (f16/M16/ef-construction 256/search 2048; the native
toolchain cost is accepted after the pure-Rust backend failed the 1M gate — ADR 0016 §Decision);
**`sqlite-vec` is dropped entirely**, including the exact-re-rank role, because
the exact cosine scan we already ship covers it faster and with no new dependency. Details below
still hold:
- **Vectors are derived data** and can always be rebuilt from the blob-cached embeddings (§8). Their canonical home is the `embedding` table inside the portable DB (convenience, one-file backup); the HNSW graph over them is a regenerable sidecar under `vectors/`. Either way, losing the index is a re-index, never data loss.
- **The sidecar HNSW under `vectors/`** is the primary similarity index — decoupled from the DB write path so re-indexing doesn't bloat the WAL, and regenerable from the embedding table. Schema V27 stores a generation, explicit lifecycle (`pending`/`building`/`ready`/`recovering`), and a latest-change/tombstone overlay per space. Sidecars use a hash of the space id plus generation as the filename and carry a magic header, format/backend/space/dimension metadata, and BLAKE3 checksum. The immutable base plus overlay makes individual writes immediately queryable without global invalidation; after a bounded overlay threshold, a background worker copies one space's vectors and its generation in one SQLite snapshot, releases SQLite, builds and fsyncs the graph, then atomically renames and publishes that snapshot while retaining every newer journal entry in the overlay. Continuous analysis writes therefore do not starve compaction. Corrupt or missing files rebuild from canonical rows. Facet filtering stays in SQLite; bounded ANN ids are fetched from `embedding` for exact cosine rerank before metadata hydration.
- **Federated similarity** does *not* use the local index for remote hits — the query embedding is sent to each peer's endpoint and merged locally ([05](05-analysis-similarity-dedup.md) / [07](07-sources-and-federation.md)); only local assets populate the local vector store.

Dimensionality per media type and the one-index-per-`EmbeddingSpace` rule are carried in [05](05-analysis-similarity-dedup.md).

---

## 8. Derivative / blob cache

### 8.1 Layout

All regenerable derivatives live under `cache/`, **outside the source tree** (DESIGN_GUIDELINES §1.3) and separate from the DB. Files are keyed by **content hash** so identical bytes share one derivative, and sharded by the first hash byte to avoid huge flat directories:

```
  cache/
  ├── thumb/<hh>/<content_hash>.<ext>          image/3d render thumbnails (webp/png)
  ├── wave/<hh>/<content_hash>.json            audio waveform peak data
  ├── preview/<hh>/<content_hash>.<ext>        larger previews (zoomable image, turntable)
  ├── embed/<hh>/<content_hash>.<space>.f32    raw embedding vectors (image|audio|shape)
  ├── render/<hh>/<content_hash>.<view>.png    multi-view 3D renders feeding shape embeds
  └── remote/<source_id>/<remote_ref_hash>.<ext>   fetched federated previews (§8.3)

  <hh> = first byte of content_hash, hex → 256-way shard
```

The `asset.thumbnail_key`/`preview_key` columns are the **cache keys** (typically just the content hash, or `content_hash + variant`), so the on-disk path is *derivable* — the DB stores a small key, not a full path, keeping it portable (a moved `cache/` dir still resolves).

### 8.2 Cache key = content hash + extractor/analysis version

Every derivative is invalidated by **either** a content change **or** an extractor upgrade — this is the re-analysis keying that makes "versioned, re-analysable" (DESIGN_GUIDELINES §1.2/§6, PRODUCT_SPEC §6.2) mechanical:

- **Change detection** (content changed): the `(size, mtime)` stat gate → rehash → new `content_hash` ⇒ a new cache path; the old derivative is simply orphaned (GC'd, §8.4). `asset.analysed_at` is cleared so the pipeline re-runs.
- **Extractor version bump** (model improved): each extractor has a monotonically increasing version; the *bundle* version is written to `asset.analysis_version` after a successful pass. A cached embedding filename encodes the space/extractor version it was produced with (e.g. `<hash>.imgclip-v3.f32`); a mismatch between the current extractor version and what produced the cached file means "stale → regenerate". Assets with `analysis_version < current` are exactly the "needs re-analysis" set — that's what `idx_asset_analysis` finds.

So the invalidation predicate for any derivative is: **`derivative is valid ⟺ produced from this content_hash AND by ≥ the required extractor version`.** Nothing else needs to be tracked to decide re-analysis.

### 8.3 Federated previews

Remote previews (§4) are fetched on demand and cached under `cache/remote/<source_id>/…` — separated from local derivatives because they are keyed by `remote_ref` (no local content hash) and belong to a peer. They carry a TTL / are evicted when their source goes offline or the asset drops out of the peer's catalog; they are never treated as authoritative and never re-analysed (PRODUCT_SPEC §4.4).

### 8.4 Cache management

- **Regenerable, disposable** — the whole `cache/` dir can be deleted; it rebuilds lazily. This is the recovery path for corruption and the reason it never holds primary data.
- **GC** removes derivatives whose `content_hash` no longer appears in any `asset` row (orphaned by content change or asset deletion), run opportunistically / on demand.
- **Size cap** (config) with LRU eviction for `preview/`, `render/`, and `remote/` (the large, easily-regenerated tiers); `thumb/`, `wave/`, and `embed/` are cheap-to-keep and evicted last. Eviction policy detail interacts with [14](14-concurrency-performance-reliability.md).

---

## 9. Where the files live

Paths follow OS conventions (via a `directories`-style resolver), overridable by config ([15](15-observability-config-testing-packaging.md) owns config precedence). Indicative:

| OS | Data dir (`library.db`, `server.db`) | Cache dir (`cache/`, `vectors/`) |
|---|---|---|
| Linux | `$XDG_DATA_HOME/3dam` (`~/.local/share/3dam`) | `$XDG_CACHE_HOME/3dam` |
| macOS | `~/Library/Application Support/3dam` | `~/Library/Caches/3dam` |
| Windows | `%APPDATA%\3dam` | `%LOCALAPPDATA%\3dam\cache` |

`cache/`/`vectors/` sit under the *cache* dir (regenerable, excludable from backup); `library.db`/`server.db` under the *data* dir (precious). The desktop app and CLI share one library by default (DESIGN_GUIDELINES §5); `3dam serve` may point at its own via config.

---

## 10. The separate server store (`server.db`)

Feature flags, user accounts, and the audit log **must live outside `library.db`** (PRODUCT_SPEC §5 end, §6.11) — they are host configuration and identity, not catalog data, and must never travel in a library copy or an export. This file defines *where and the shape*; **[10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md) owns the semantics** (flag lifecycle live-vs-restart, role/scope meaning, auth flow, [ADR 0004](../adr/0004-feature-flags-admin.md)).

Location & shape: a **second SQLite file, `server.db`**, beside the library but physically separate (own file → cannot leak into a library export). Only present/used in `serve` mode; a purely embedded client never creates it.

`server.db` uses its own forward-only migration list. V1 is the original flags/tokens/audit shape,
V2 adds accounts/sessions/groups/shares, and V3 adds OIDC state; these first three steps use
`IF NOT EXISTS` solely to adopt already-shipped, unversioned databases without replacing their
tables. Future changes append exactly one new migration and must not edit an earlier step. Opening
takes an immediate SQLite transaction, refuses a `user_version` newer than the binary, snapshots a
non-empty on-disk database through SQLite's online-backup API, and commits the whole ordered upgrade
atomically. A failed statement therefore leaves the original schema/version usable on restart; the
snapshot is the operator's recovery copy if the migration itself is later found to be semantically
wrong.

```sql
-- feature flags: versioned, persisted, seeded by the config file, edited by the admin API (§6.11)
CREATE TABLE feature_flag (
    key        TEXT PRIMARY KEY,      -- 'remote_access','auth_mode','accounts',
                                      -- 'mcp','network_writes','inbound_federation',
                                      -- 'remote_connect','analysis_watch'
    value      TEXT NOT NULL,         -- JSON: bool | enum | sub-config
    updated_at INTEGER NOT NULL,
    updated_by TEXT                   -- account id / 'config' / 'cli'  (also in audit)
) STRICT;

-- user accounts: opt-in, off by default; NO plaintext secrets, argon2 hashes only
CREATE TABLE account (
    id         BLOB PRIMARY KEY,      -- UUIDv7
    name       TEXT NOT NULL UNIQUE,
    pw_hash    TEXT NOT NULL,         -- argon2id
    role       TEXT NOT NULL,         -- 'admin'|'editor'|'viewer'
    scope      TEXT,                  -- JSON: visible source/collection ids (NULL = all)
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- audit log: every flag/account change, who + when (PRODUCT_SPEC §6.11 "auditable & reversible")
CREATE TABLE audit_log (
    id         BLOB PRIMARY KEY,
    at         INTEGER NOT NULL,
    actor      TEXT NOT NULL,         -- account id or 'config'/'cli'/'system'
    action     TEXT NOT NULL,         -- 'flag.set','account.create',...
    detail     TEXT NOT NULL          -- JSON before/after
) STRICT;

CREATE INDEX idx_audit_at ON audit_log(at);
```

Config-file values *seed* `feature_flag` and the admin API/CLI edit the same rows — one source of truth, coequal control planes (PRODUCT_SPEC §6.11). Whether flag state ultimately lives here vs a watched config file, and how the two reconcile, is an open question owned by [10](10-auth-accounts-and-flags.md) and carried in §11.

---

## 11. Open questions

> **Resolved 2026-07-06.** Licence taxonomy → [ADR 0009 §1](../adr/0009-v1-scope-decisions.md)
> (**no defaults; unknown-is-unknown**; hybrid SPDX/Proprietary/Custom/NULL; per-asset override via
> `'inherited'` provenance). Feature-flag store, change-detection `strict` mode, and cache-size
> policy → [ADR 0009 §2/§11](../adr/0009-v1-scope-decisions.md). Vector-index storage → the
> [vector-index spike](../../spikes/vector-index/README.md) (sidecar HNSW) and
> [ADR 0016](../adr/0016-vector-index-backend.md) (`usearch` 2.25.3; `sqlite-vec` dropped).
> Kept below as rationale.

Carried forward from PRODUCT_SPEC §10 where they touch storage; the analysis/auth files own the non-storage halves.

- ~~**Vector index — embedded vs sidecar, backend crate and lifecycle**~~ (PRODUCT_SPEC §10) — **Decided** ([ADR 0016](../adr/0016-vector-index-backend.md)) and implemented by [#141](https://github.com/krazyjakee/3DAM/issues/141): a persisted sidecar HNSW under `vectors/` on pinned `usearch` 2.25.3, one immutable base plus durable incremental overlay per `EmbeddingSpace`, background atomic compaction, corruption recovery, bounded candidates and exact Rust rerank; `sqlite-vec` is dropped. Storage framing is §7; production/query is [05](05-analysis-similarity-dedup.md).
- **License taxonomy & storage shape** (PRODUCT_SPEC §10): how far to lean on SPDX ids vs a 3DAM rights model in `license_id`/`license_status`; how to represent **per-asset overrides within a pack** that has one blanket license (inherit-from-source column? explicit override flag?); and the enum set for `license_status`/`license_provenance` as detection matures. This file fixes the columns; the taxonomy is not final.
- **Feature-flag store & lifecycle** (PRODUCT_SPEC §10, §6.11): whether flag state persists in `server.db.feature_flag` (§10) vs a watched config file, and how config-file and admin-UI edits reconcile — the *store shape* is here, the *reconciliation/live-vs-restart lifecycle* is [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md)'s.
- **Change-detection gate robustness:** the `(size, mtime)` fast gate can miss same-size/same-mtime edits and mis-fire on touch-only changes; whether to offer a "trust content hash only" strict mode for correctness-critical libraries, at re-scan cost.
- **Cache size policy:** default cap, per-tier eviction weights, and whether `embed/` should ever be evicted given it also backs the (regenerable) vector index — interacts with [14](14-concurrency-performance-reliability.md).
