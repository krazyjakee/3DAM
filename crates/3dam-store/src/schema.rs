//! Forward-only, versioned migrations (tech-spec 02 §6). Each step's number must be `> user_version`;
//! `open` applies all newer steps in one transaction and bumps `PRAGMA user_version`. Additive-first.

/// Ordered migration steps. Append new ones; never edit or reorder a shipped step.
pub const MIGRATIONS: &[&str] = &[
    // ── V1: the phase-1 catalog schema (tech-spec 02 §3) ────────────────────
    r#"
    CREATE TABLE asset (
        id                  BLOB PRIMARY KEY,          -- UUIDv7, 16 bytes
        content_hash        BLOB,                      -- BLAKE3 32 bytes; NULL for federated
        source_id           BLOB NOT NULL REFERENCES source(id) ON DELETE CASCADE,
        path                TEXT NOT NULL,
        filename            TEXT NOT NULL,
        size_bytes          INTEGER,
        source_created_at   INTEGER,
        source_modified_at  INTEGER,
        scanned_at          INTEGER NOT NULL,
        analysed_at         INTEGER,
        media_type          TEXT NOT NULL,             -- 'audio'|'image'|'model'
        format              TEXT NOT NULL,
        rating              INTEGER,
        flags               INTEGER NOT NULL DEFAULT 0,
        notes               TEXT,
        license_id          TEXT,
        license_status      TEXT NOT NULL DEFAULT 'unknown',
        rights_commercial   INTEGER,
        rights_modify       INTEGER,
        rights_redistribute INTEGER,
        rights_attribution  INTEGER,
        attribution_holder  TEXT,
        attribution_credit  TEXT,
        license_url         TEXT,
        license_provenance  TEXT NOT NULL DEFAULT 'unknown',
        thumbnail_key       TEXT,
        preview_key         TEXT,
        remote_ref          TEXT,
        analysis_version    INTEGER NOT NULL DEFAULT 0,
        created_at          INTEGER NOT NULL,
        updated_at          INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX idx_asset_hash        ON asset(content_hash);
    CREATE UNIQUE INDEX uq_asset_source_path ON asset(source_id, path);
    CREATE INDEX idx_asset_media_type  ON asset(media_type, format);
    CREATE INDEX idx_asset_license     ON asset(license_status, rights_commercial, rights_attribution);
    CREATE INDEX idx_asset_analysis    ON asset(analysis_version);

    CREATE TABLE audio_attr (
        asset_id     BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
        duration_ms  INTEGER,
        sample_rate  INTEGER,
        bit_depth    INTEGER,
        channels     INTEGER,
        bpm          REAL,
        musical_key  TEXT,
        loudness_lufs REAL,
        brightness   REAL,
        harmonicity  REAL,
        class        TEXT
    ) STRICT;

    CREATE TABLE image_attr (
        asset_id     BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
        width        INTEGER,
        height       INTEGER,
        color_depth  INTEGER,
        has_alpha    INTEGER,
        color_space  TEXT,
        dominant_colors TEXT,
        class        TEXT,
        phash        BLOB,
        tileability  REAL,
        repeat_period INTEGER,
        tile_class   TEXT
    ) STRICT;

    CREATE TABLE model_attr (
        asset_id       BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
        vertex_count   INTEGER,
        triangle_count INTEGER,
        mesh_count     INTEGER,
        material_count INTEGER,
        texture_count  INTEGER,
        bbox_min       TEXT,
        bbox_max       TEXT,
        has_rig        INTEGER,
        has_animation  INTEGER,
        has_uv         INTEGER,
        class          TEXT
    ) STRICT;

    CREATE TABLE tag (
        id     BLOB PRIMARY KEY,
        name   TEXT NOT NULL,
        UNIQUE(name COLLATE NOCASE)
    ) STRICT;

    CREATE TABLE asset_tag (
        asset_id  BLOB NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
        tag_id    BLOB NOT NULL REFERENCES tag(id)   ON DELETE CASCADE,
        state     TEXT NOT NULL,
        source    TEXT NOT NULL,
        confidence REAL,
        extractor  TEXT,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (asset_id, tag_id)
    ) STRICT;
    CREATE INDEX idx_asset_tag_tag   ON asset_tag(tag_id, state);
    CREATE INDEX idx_asset_tag_state ON asset_tag(state, source);

    CREATE TABLE collection (
        id       BLOB PRIMARY KEY,
        name     TEXT NOT NULL,
        kind     TEXT NOT NULL,
        query    TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE collection_member (
        collection_id BLOB NOT NULL REFERENCES collection(id) ON DELETE CASCADE,
        asset_id      BLOB NOT NULL REFERENCES asset(id)      ON DELETE CASCADE,
        added_at      INTEGER NOT NULL,
        PRIMARY KEY (collection_id, asset_id)
    ) STRICT;

    CREATE TABLE source (
        id           BLOB PRIMARY KEY,
        name         TEXT NOT NULL,
        kind         TEXT NOT NULL,
        connection   TEXT NOT NULL,
        auth_mode    TEXT,
        auth_ref     TEXT,
        online       INTEGER NOT NULL DEFAULT 1,
        last_scanned_at INTEGER,
        last_error   TEXT,
        watch        INTEGER NOT NULL DEFAULT 0,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE job (
        id         BLOB PRIMARY KEY,
        kind       TEXT NOT NULL,
        state      TEXT NOT NULL,
        params     TEXT NOT NULL,
        progress   REAL NOT NULL DEFAULT 0,
        done       INTEGER NOT NULL DEFAULT 0,
        total      INTEGER,
        current    TEXT,
        error      TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX idx_job_state ON job(state, kind);
    "#,
    // ── V2: persist the audio codec/container the cheap tier now reads (tech-spec 04 §5) ──────
    r#"
    ALTER TABLE audio_attr ADD COLUMN codec     TEXT;
    ALTER TABLE audio_attr ADD COLUMN container TEXT;
    "#,
    // ── V3: the analysis/automation slice (phase 3, tech-spec 05) ──────────────────────────────
    // The per-type attr tables already carry the derived columns (phash/tileability/class from V1),
    // so V3 only adds the vector index. One row per (asset, embedding-space): the normalised f32
    // embedding is stored as little-endian bytes, tagged with the space id + extractor version so a
    // model bump can invalidate just its slice (§2.1, §3.1, §7.2). Similarity is a brute-force cosine
    // scan over this table in v1 — correct and exact; an HNSW/`usearch` sidecar is the scale
    // follow-up (§3.1, open question). Storage layout owned here per tech-spec 02.
    r#"
    CREATE TABLE embedding (
        asset_id   BLOB NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
        space_id   TEXT NOT NULL,             -- content-addressed EmbeddingSpace id (model+ver+media+dim+metric)
        media_type TEXT NOT NULL,             -- one logical index per media space; never mixed
        dim        INTEGER NOT NULL,
        vec        BLOB NOT NULL,             -- dim × f32 little-endian, L2-normalised
        extractor  TEXT NOT NULL,             -- extractor_id@version that produced it (§7.1)
        created_at INTEGER NOT NULL,
        PRIMARY KEY (asset_id, space_id)
    ) STRICT;
    CREATE INDEX idx_embedding_space ON embedding(space_id, media_type);
    "#,
];
