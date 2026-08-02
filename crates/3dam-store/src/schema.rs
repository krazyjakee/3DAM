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
    // ── V4: the rescan blocklist (issue #21) ────────────────────────────────────────────────────
    // A content hash listed here is never re-imported by a scan/watch/auto-rescan — the "remove +
    // block" outcome persists across re-scans (§2.2). Keyed by the BLAKE3 content hash so it matches
    // regardless of path or source; `label` keeps the last-known filename so the management surface
    // stays recognisable after the asset row it came from is deleted.
    r#"
    CREATE TABLE blocklist (
        content_hash BLOB PRIMARY KEY,       -- BLAKE3 32 bytes, matches asset(content_hash)
        label        TEXT,                    -- last-known filename at block time, for display
        blocked_at   INTEGER NOT NULL
    ) STRICT;
    "#,
    // ── V5: a model's external companion-file footprint (tech-spec 04 §4.1) ──────────────────────
    // `asset.size_bytes` stays the mesh container's own size (it is the delta-scan change token, and
    // must match the filesystem entry). This holds the summed on-disk bytes of the *referenced*
    // files a model pulls in — external textures, glTF `.bin` buffers, an OBJ's `.mtl` + maps — so
    // the reported asset size (`size_bytes + dependency_bytes`) reflects the whole asset. NULL for
    // self-contained models (GLB/STL/PLY) and non-models.
    r#"
    ALTER TABLE model_attr ADD COLUMN dependency_bytes INTEGER;
    "#,
    // ── V6: FTS5 full-text index over the searchable text of an asset (semantic-search M1) ────────
    // v1 text search was `filename LIKE '%term%'` — an unindexed full scan with no word matching or
    // ranking (§13). This adds an FTS5 index so text search becomes an inverted-index lookup with
    // bm25 relevance, real token matching, and prefix queries. Two columns: `filename` (the raw
    // name) and `tokens` (filename-derived terms the analyse pass writes — M2 — so "ak47_lowpoly.fbx"
    // is findable as `ak47`). A standalone (not contentless) FTS table keyed by `asset.rowid`, kept
    // in sync by triggers; the initial backfill seeds rows for an already-populated catalog. V19
    // later gives partial in-word filename substrings their own trigram posting index.
    r#"
    CREATE VIRTUAL TABLE asset_fts USING fts5(
        filename,
        tokens,
        tokenize = "unicode61 remove_diacritics 2"
    );

    -- Seed existing rows (triggers only fire on future writes).
    INSERT INTO asset_fts(rowid, filename, tokens)
        SELECT rowid, filename, '' FROM asset;

    -- Keep the index in lockstep with the asset table. rowid is the join key back to asset.
    CREATE TRIGGER asset_fts_ai AFTER INSERT ON asset BEGIN
        INSERT INTO asset_fts(rowid, filename, tokens) VALUES (new.rowid, new.filename, '');
    END;
    CREATE TRIGGER asset_fts_ad AFTER DELETE ON asset BEGIN
        DELETE FROM asset_fts WHERE rowid = old.rowid;
    END;
    CREATE TRIGGER asset_fts_au AFTER UPDATE OF filename ON asset BEGIN
        UPDATE asset_fts SET filename = new.filename WHERE rowid = new.rowid;
    END;
    "#,
    // ── V7: fold tag names into the FTS index so tags power text search ───────────────────────────
    // Auto-tags no longer live in the sidebar; they earn their keep by making assets findable ("kick"
    // surfaces a `kick`-tagged one-shot even when the filename never says so). FTS5 has no ADD COLUMN,
    // so the two-column index is rebuilt with a third `tags` column. The filename-derived `tokens`
    // (M2) live *only* in the index, so they're stashed and restored rather than lost; `tags` is
    // seeded from the current non-rejected tag names. The MATCH expression is column-agnostic, so the
    // new column is searched automatically — no query change. The write path (analysis.rs) keeps the
    // column current on every tag mutation; a rejected tag drops straight back out of search.
    r#"
    -- tokens (filename sub-tokens) exist only in the FTS index — stash before the rebuild.
    CREATE TEMP TABLE _fts_tokens AS SELECT rowid AS rid, tokens FROM asset_fts;

    DROP TRIGGER asset_fts_ai;
    DROP TRIGGER asset_fts_ad;
    DROP TRIGGER asset_fts_au;
    DROP TABLE asset_fts;

    CREATE VIRTUAL TABLE asset_fts USING fts5(
        filename,
        tokens,
        tags,
        tokenize = "unicode61 remove_diacritics 2"
    );

    -- Re-seed: filename from the base table, tokens from the stash, tags from non-rejected tag names.
    INSERT INTO asset_fts(rowid, filename, tokens, tags)
        SELECT a.rowid,
               a.filename,
               COALESCE(t.tokens, ''),
               COALESCE((SELECT group_concat(tg.name, ' ')
                         FROM asset_tag at JOIN tag tg ON tg.id = at.tag_id
                         WHERE at.asset_id = a.id AND at.state <> 'rejected'), '')
        FROM asset a LEFT JOIN _fts_tokens t ON t.rid = a.rowid;

    DROP TABLE _fts_tokens;

    -- Recreate the sync triggers (insert seeds empty tokens/tags; the write paths fill them).
    CREATE TRIGGER asset_fts_ai AFTER INSERT ON asset BEGIN
        INSERT INTO asset_fts(rowid, filename, tokens, tags) VALUES (new.rowid, new.filename, '', '');
    END;
    CREATE TRIGGER asset_fts_ad AFTER DELETE ON asset BEGIN
        DELETE FROM asset_fts WHERE rowid = old.rowid;
    END;
    CREATE TRIGGER asset_fts_au AFTER UPDATE OF filename ON asset BEGIN
        UPDATE asset_fts SET filename = new.filename WHERE rowid = new.rowid;
    END;
    "#,
    // ── V8: server-side waveform peaks (hosted mode, issue #73) ───────────────────────────────────
    // The analysis pass now computes a fixed-bucket, normalised (0–1) peak array for each audio asset
    // and stores it here as a JSON `[f32,…]` string, so every client draws the inspector waveform
    // from server-provided data instead of re-downloading + re-decoding the audio just to size bars.
    // NULL until the asset is analysed; full-content fetch remains only for actual playback.
    r#"
    ALTER TABLE audio_attr ADD COLUMN waveform_peaks TEXT;
    "#,
    // ── V9: which sources a job touches (issue #42) ───────────────────────────────────────────────
    // `JobStatus` now carries attribution so a visibility-restricted identity can be shown its own
    // jobs — and be sent `JobProgress` events — without seeing paths from sources it cannot reach.
    // Stored as a JSON array of canonical uuid strings; NULL/absent on pre-V9 rows, which read back
    // as an empty set and are therefore visible only at `Visibility::Full` (fail-safe).
    r#"
    ALTER TABLE job ADD COLUMN sources TEXT;
    "#,
    // ── V10: video + document attribute tables (PRODUCT_SPEC §9 phase 2b, issue #79) ──────────────
    // `MediaType` grows a fourth and fifth variant so a source is catalogued *completely*. Each new
    // type gets its own `*_attr` table, mirroring audio/image/model exactly — one row per asset,
    // cascade-deleted with it, every column nullable because both cheap tiers are best-effort.
    //
    // Note `asset.media_type` carries no CHECK constraint (it never has), so existing rows and the
    // enum widening need no data migration: the column's comment in V1 is simply now out of date,
    // and the authoritative list lives in `MediaType::parse`.
    //
    // Video columns come from a discovered `ffprobe` (ADR 0015) and are *all* NULL when no prober is
    // installed — that is the designed floor, not a failure. `has_audio` is 0/1 (STRICT has no bool).
    r#"
    CREATE TABLE video_attr (
        asset_id    BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
        duration_ms INTEGER,
        width       INTEGER,
        height      INTEGER,
        fps         REAL,
        codec       TEXT,
        container   TEXT,
        bitrate     INTEGER,
        has_audio   INTEGER,
        class       TEXT
    ) STRICT;

    CREATE TABLE document_attr (
        asset_id   BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
        page_count INTEGER,
        word_count INTEGER,
        title      TEXT,
        author     TEXT,
        encoding   TEXT,
        excerpt    TEXT,
        class      TEXT
    ) STRICT;
    "#,
    // ── V11: extracted document text as a fourth FTS column (PRODUCT_SPEC §9 phase 2b) ────────────
    // Documents are the first media type whose *content is language*, so their text belongs in the
    // index that already answers text search — not a second index that would need its own ranking,
    // its own triggers, and a union at query time. FTS5 still has no ADD COLUMN, so this repeats the
    // V7 rebuild dance exactly: stash the columns that live *only* in the index (`tokens` and now
    // `tags`, which V7 seeds from the tag tables but the write path has since updated), rebuild with
    // the new column, re-seed, restore triggers.
    //
    // The new column is seeded empty rather than back-filled: the text isn't in the database yet,
    // it's in the files. The analyse pass fills it (a document scanned before this migration is
    // picked up on its next analyse, like any other new derived field).
    //
    // Ranking is the real design question here, not plumbing. `bm25()` weights columns left-to-right
    // and defaults to 1.0 for each; with a 40-page design doc in `text`, an unweighted index buries
    // an exact filename match under every document that merely mentions the word. The query layer
    // therefore passes explicit weights (see `query.rs`) — this migration only has to put the column
    // last so the existing weight order stays stable.
    r#"
    CREATE TEMP TABLE _fts_stash AS
        SELECT rowid AS rid, tokens, tags FROM asset_fts;

    DROP TRIGGER asset_fts_ai;
    DROP TRIGGER asset_fts_ad;
    DROP TRIGGER asset_fts_au;
    DROP TABLE asset_fts;

    CREATE VIRTUAL TABLE asset_fts USING fts5(
        filename,
        tokens,
        tags,
        text,
        tokenize = "unicode61 remove_diacritics 2"
    );

    INSERT INTO asset_fts(rowid, filename, tokens, tags, text)
        SELECT a.rowid, a.filename, COALESCE(s.tokens, ''), COALESCE(s.tags, ''), ''
        FROM asset a LEFT JOIN _fts_stash s ON s.rid = a.rowid;

    DROP TABLE _fts_stash;

    CREATE TRIGGER asset_fts_ai AFTER INSERT ON asset BEGIN
        INSERT INTO asset_fts(rowid, filename, tokens, tags, text)
            VALUES (new.rowid, new.filename, '', '', '');
    END;
    CREATE TRIGGER asset_fts_ad AFTER DELETE ON asset BEGIN
        DELETE FROM asset_fts WHERE rowid = old.rowid;
    END;
    CREATE TRIGGER asset_fts_au AFTER UPDATE OF filename ON asset BEGIN
        UPDATE asset_fts SET filename = new.filename WHERE rowid = new.rowid;
    END;
    "#,
    // ── V12: per-asset user notes (issue #81) ────────────────────────────────────────────────────
    // A free-text annotation — the "why" no extractor can infer ("client rejected this variant").
    // It lives in the catalog, never as a sidecar next to the file: originals are never mutated and
    // all derived data stays in the managed store (tech-spec 02 §3.5), and a `.txt` 3DAM wrote would
    // come straight back in on the next scan as an asset of its own.
    //
    // Its own table rather than a column on `asset`, for three reasons: the body is unbounded and
    // `asset` is the hot row every grid query reads; a note is authored state that must survive the
    // scan upsert path untouched; and `updated_at`/`updated_by` are note facts, not asset facts.
    // `updated_by` is a loose identity string on purpose — when accounts land (#42) it becomes a
    // real account reference without rewriting the migration.
    //
    // The V1 `asset.notes` column is dropped in the same step. It shipped in the first schema and
    // was never read or written by any code path; leaving a dead `notes` next to a live `asset_note`
    // is the kind of ambiguity that gets written to by mistake exactly once.
    //
    // FTS still has no ADD COLUMN, so this is the V7/V11 stash-and-restore rebuild a third time, now
    // with three index-only columns to carry across. `note` is seeded empty because `asset_note` is
    // created empty here; the write path fills it from then on.
    //
    // Column placement is deliberate: `note` goes *before* `text` so the tier/weight story stays
    // legible — see `search.rs`, where notes join the authored tier (a note is the user speaking,
    // not incidental body prose) but rank below filename, tokens, and tags within it.
    r#"
    CREATE TABLE asset_note (
        asset_id   BLOB PRIMARY KEY REFERENCES asset(id) ON DELETE CASCADE,
        body       TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        updated_by TEXT
    ) STRICT;

    ALTER TABLE asset DROP COLUMN notes;

    CREATE TEMP TABLE _fts_stash AS
        SELECT rowid AS rid, tokens, tags, text FROM asset_fts;

    DROP TRIGGER asset_fts_ai;
    DROP TRIGGER asset_fts_ad;
    DROP TRIGGER asset_fts_au;
    DROP TABLE asset_fts;

    CREATE VIRTUAL TABLE asset_fts USING fts5(
        filename,
        tokens,
        tags,
        note,
        text,
        tokenize = "unicode61 remove_diacritics 2"
    );

    INSERT INTO asset_fts(rowid, filename, tokens, tags, note, text)
        SELECT a.rowid, a.filename, COALESCE(s.tokens, ''), COALESCE(s.tags, ''), '',
               COALESCE(s.text, '')
        FROM asset a LEFT JOIN _fts_stash s ON s.rid = a.rowid;

    DROP TABLE _fts_stash;

    CREATE TRIGGER asset_fts_ai AFTER INSERT ON asset BEGIN
        INSERT INTO asset_fts(rowid, filename, tokens, tags, note, text)
            VALUES (new.rowid, new.filename, '', '', '', '');
    END;
    CREATE TRIGGER asset_fts_ad AFTER DELETE ON asset BEGIN
        DELETE FROM asset_fts WHERE rowid = old.rowid;
    END;
    CREATE TRIGGER asset_fts_au AFTER UPDATE OF filename ON asset BEGIN
        UPDATE asset_fts SET filename = new.filename WHERE rowid = new.rowid;
    END;
    "#,
    // ── V13: folder names as a searchable column (issue #66) ─────────────────────────────────────
    // Artists organise assets into hierarchies that carry real meaning — `Environment/Rock/Cliffs/`,
    // an asset pack's own layout — and until now that structure was navigable but not *findable*.
    // The folder tree could take you to `Cliffs/` only if you already knew where to look; typing
    // "cliffs" found nothing, because nothing indexed the directory a file sits in.
    //
    // The segments are tokenised with the same splitter filenames use (`search::tokenize_name`), so
    // `Weapons_AK47/` yields `weapons`, `ak47`, `ak`, `47`. The **filename is excluded** — it is
    // already its own column, and duplicating it here would double-count a name match and quietly
    // distort bm25.
    //
    // Unlike `tokens`/`tags`/`text`, this column is *derivable*: it is a pure function of
    // `asset.path`, which is a base-table column. So it is back-filled in place here rather than
    // seeded empty, and a future rebuild could recompute it instead of stashing it — though the
    // tokeniser lives in Rust, so the back-fill below is a coarser SQL approximation (whole segments
    // only, lowercased) that the next scan refines. That asymmetry is deliberate: a fresh install
    // and a scan-since-upgrade get the good tokens, and an un-rescanned upgrade still gets whole
    // folder names, which is the case the issue actually cares about.
    r#"
    CREATE TEMP TABLE _fts_stash AS
        SELECT rowid AS rid, tokens, tags, note, text FROM asset_fts;

    DROP TRIGGER asset_fts_ai;
    DROP TRIGGER asset_fts_ad;
    DROP TRIGGER asset_fts_au;
    DROP TABLE asset_fts;

    CREATE VIRTUAL TABLE asset_fts USING fts5(
        filename,
        tokens,
        tags,
        note,
        folder,
        text,
        tokenize = "unicode61 remove_diacritics 2"
    );

    -- Back-fill `folder` from the stored path: everything up to the last '/', with separators
    -- turned into spaces so FTS sees one term per segment. Files at the source root have no '/'
    -- and get ''.
    INSERT INTO asset_fts(rowid, filename, tokens, tags, note, folder, text)
        SELECT a.rowid, a.filename, COALESCE(s.tokens, ''), COALESCE(s.tags, ''),
               COALESCE(s.note, ''),
               CASE WHEN instr(a.path, '/') > 0
                    THEN replace(lower(rtrim(substr(a.path, 1, length(a.path) - length(a.filename)), '/')), '/', ' ')
                    ELSE '' END,
               COALESCE(s.text, '')
        FROM asset a LEFT JOIN _fts_stash s ON s.rid = a.rowid;

    DROP TABLE _fts_stash;

    CREATE TRIGGER asset_fts_ai AFTER INSERT ON asset BEGIN
        INSERT INTO asset_fts(rowid, filename, tokens, tags, note, folder, text)
            VALUES (new.rowid, new.filename, '', '', '', '', '');
    END;
    CREATE TRIGGER asset_fts_ad AFTER DELETE ON asset BEGIN
        DELETE FROM asset_fts WHERE rowid = old.rowid;
    END;
    CREATE TRIGGER asset_fts_au AFTER UPDATE OF filename ON asset BEGIN
        UPDATE asset_fts SET filename = new.filename WHERE rowid = new.rowid;
    END;
    "#,
    // ── V14: per-asset discussion threads (issue #82) ────────────────────────────────────────────
    // The append-only counterpart to `asset_note` (V12): a note is one durable editable annotation,
    // a thread is authored history. Both live in `library.db` because both are per-asset.
    //
    // `author` is an **account id, held as a soft reference across the database boundary** — the
    // accounts themselves live in `server.db`, which this database cannot join against. That is
    // deliberate, not an oversight: the alternative (a foreign key, or a cascade) would delete a
    // person's messages when their account is removed, silently rewriting a project's history. The
    // id is kept forever and resolved to a display name at read time; when it no longer resolves,
    // the message renders as an unresolved author rather than disappearing.
    //
    // `deleted_at` is a soft delete for the same reason: hard-deleting a message somebody replied
    // to would orphan the reply. The row survives as a tombstone with its body blanked.
    //
    // `comment_id` is a UUIDv7, so `ORDER BY comment_id` *is* chronological order — the index below
    // exists for the per-asset filter, not to make time sortable.
    //
    // Deliberately **not** joined to `asset_fts`: indexing conversation makes search noisy, and
    // notes are the curated text that should be findable. Revisit on evidence.
    r#"
    CREATE TABLE asset_comment (
        comment_id BLOB PRIMARY KEY,
        asset_id   BLOB NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
        author     TEXT NOT NULL,
        body       TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        edited_at  INTEGER,
        deleted_at INTEGER,
        reply_to   BLOB REFERENCES asset_comment(comment_id) ON DELETE SET NULL
    ) STRICT;
    CREATE INDEX idx_comment_asset ON asset_comment(asset_id, comment_id);
    "#,
    // ── V15: GPU texture container attributes (issue #49) ────────────────────────────────────────
    // DDS and KTX2 were catalogued as images with dimensions and nothing else, which makes a BC5
    // normal map and a BC7 albedo the same row. These two columns are what distinguish them, and
    // they come from the container header at the cheap tier — no pixel decode.
    //
    // Additive `ALTER TABLE`, so existing rows keep working and simply read NULL until their next
    // scan fills them in. Ordinary rasters leave both NULL for good: PNG has no mip chain and its
    // "format" is the file extension the row already carries.
    r#"
    ALTER TABLE image_attr ADD COLUMN texture_format TEXT;
    ALTER TABLE image_attr ADD COLUMN mip_levels INTEGER;
    "#,
    // ── V16: durable, differentiated terminal job outcomes (issue #117) ────────────────────────
    // `error` remains reserved for a hard job failure. A successful run's description and its
    // recoverable degradation are separate so clients cannot accidentally call a partial result a
    // success. Timestamps already lived on the row; initiator is additive and nullable because old
    // jobs and embedded/automatic callers do not always have an authenticated identity.
    r#"
    ALTER TABLE job ADD COLUMN summary TEXT;
    ALTER TABLE job ADD COLUMN warnings TEXT;
    ALTER TABLE job ADD COLUMN initiator TEXT;
    ALTER TABLE job ADD COLUMN result_artifacts TEXT;
    "#,
    // ── V17: host-side migration completion markers (issue #103) ────────────────────────────────
    // Credential extraction cannot be performed by SQLite alone: it must commit each legacy secret
    // to the OS/host secret backend before redacting the row. This marker is deliberately separate
    // from `user_version`, which is advanced before dam-core can do that host operation. It carries
    // no host identity or credential material and is therefore safe in the portable catalog.
    r#"
    CREATE TABLE host_migration (
        key          TEXT PRIMARY KEY,
        completed_at INTEGER NOT NULL
    ) STRICT;

    -- Two-store deletion is not atomic. Queue opaque refs in the catalog transaction before source
    -- rows disappear; dam-core removes each queue row only after the host backend confirms deletion.
    CREATE TABLE host_secret_cleanup (
        auth_ref TEXT PRIMARY KEY
    ) STRICT;
    "#,
    // ── V18: durable structured long-operation reports (issue #114) ─────────────────────────────
    r#"
    ALTER TABLE job ADD COLUMN result TEXT;
    ALTER TABLE job ADD COLUMN collections TEXT;
    "#,
    // ── V19: indexed filename substring fallback (issue #134) ────────────────────────────────
    // Unicode FTS remains the primary lexical index. This second, filename-only FTS5 table uses
    // the trigram tokenizer for the legacy in-word fallback (`k47` in `ak47_lowpoly.fbx`) without
    // putting `filename LIKE '%…%'` beside every otherwise-indexed MATCH. The rowid is the same
    // asset rowid used by `asset_fts`, so query-time UNIONs are index-to-index and join straight
    // back to the catalog. Standalone storage keeps ordinary DELETE/UPDATE triggers simple.
    r#"
    CREATE VIRTUAL TABLE asset_filename_trigram USING fts5(
        filename,
        tokenize = "trigram remove_diacritics 1"
    );

    INSERT INTO asset_filename_trigram(rowid, filename)
        SELECT rowid, filename FROM asset;

    CREATE TRIGGER asset_filename_trigram_ai AFTER INSERT ON asset BEGIN
        INSERT INTO asset_filename_trigram(rowid, filename) VALUES (new.rowid, new.filename);
    END;
    CREATE TRIGGER asset_filename_trigram_ad AFTER DELETE ON asset BEGIN
        DELETE FROM asset_filename_trigram WHERE rowid = old.rowid;
    END;
    CREATE TRIGGER asset_filename_trigram_au AFTER UPDATE OF filename ON asset BEGIN
        UPDATE asset_filename_trigram SET filename = new.filename WHERE rowid = new.rowid;
    END;
    "#,
    // ── V20: materialized source folder hierarchy (issue #136) ───────────────────────────────
    // Folder expansion used to split and group every descendant `asset.path` on every click. Keep
    // one row per source-relative directory instead: `parent_path` is the direct-child lookup key,
    // while the two counters distinguish files immediately in the directory from its whole subtree.
    // Paths are the same normalized `/`-separated strings stored on assets; the empty path is the
    // source root. Asset triggers make every ingest/removal/path move update the hierarchy in the
    // caller's transaction, including deletes reached through source/blocklist cascades.
    r#"
    CREATE TABLE folder (
        source_id              BLOB NOT NULL REFERENCES source(id) ON DELETE CASCADE,
        path                   TEXT NOT NULL,
        parent_path            TEXT NOT NULL,
        name                   TEXT NOT NULL,
        direct_asset_count     INTEGER NOT NULL DEFAULT 0 CHECK (direct_asset_count >= 0),
        descendant_asset_count INTEGER NOT NULL DEFAULT 0 CHECK (descendant_asset_count >= 0),
        PRIMARY KEY (source_id, path)
    ) STRICT;
    CREATE INDEX idx_folder_parent
        ON folder(source_id, parent_path, name COLLATE NOCASE);

    -- Empty sources still own a root node. The recursive backfill emits root + each directory
    -- prefix once per asset; `rest` has no slash exactly at that asset's immediate parent.
    INSERT INTO folder(source_id, path, parent_path, name)
        SELECT id, '', '', '' FROM source;
    WITH RECURSIVE hierarchy(source_id, path, parent_path, name, rest) AS (
        SELECT source_id, '', '', '', path FROM asset
        UNION ALL
        SELECT source_id,
               path || substr(rest, 1, instr(rest, '/')),
               path,
               substr(rest, 1, instr(rest, '/') - 1),
               substr(rest, instr(rest, '/') + 1)
          FROM hierarchy WHERE instr(rest, '/') > 0
    )
    INSERT INTO folder(source_id, path, parent_path, name,
                       direct_asset_count, descendant_asset_count)
        SELECT source_id, path, min(parent_path), min(name),
               sum(CASE WHEN instr(rest, '/') = 0 THEN 1 ELSE 0 END), count(*)
          FROM hierarchy
         GROUP BY source_id, path
        ON CONFLICT(source_id, path) DO UPDATE SET
            parent_path = excluded.parent_path,
            name = excluded.name,
            direct_asset_count = excluded.direct_asset_count,
            descendant_asset_count = excluded.descendant_asset_count;

    CREATE TRIGGER folder_source_ai AFTER INSERT ON source BEGIN
        INSERT INTO folder(source_id, path, parent_path, name)
        VALUES (new.id, '', '', '');
    END;

    CREATE TRIGGER folder_asset_ai AFTER INSERT ON asset BEGIN
        INSERT OR IGNORE INTO folder(source_id, path, parent_path, name)
        SELECT new.source_id, path, parent_path, name FROM (
            WITH RECURSIVE hierarchy(path, parent_path, name, rest) AS (
                SELECT '', '', '', new.path
                UNION ALL
                SELECT path || substr(rest, 1, instr(rest, '/')),
                       path,
                       substr(rest, 1, instr(rest, '/') - 1),
                       substr(rest, instr(rest, '/') + 1)
                  FROM hierarchy WHERE instr(rest, '/') > 0
            )
            SELECT path, parent_path, name FROM hierarchy
        );
        UPDATE folder
           SET descendant_asset_count = descendant_asset_count + 1,
               direct_asset_count = direct_asset_count +
                   CASE WHEN instr(substr(new.path, length(path) + 1), '/') = 0 THEN 1 ELSE 0 END
         WHERE source_id = new.source_id AND path IN (
            SELECT path FROM (
                WITH RECURSIVE hierarchy(path, rest) AS (
                    SELECT '', new.path
                    UNION ALL
                    SELECT path || substr(rest, 1, instr(rest, '/')),
                           substr(rest, instr(rest, '/') + 1)
                      FROM hierarchy WHERE instr(rest, '/') > 0
                )
                SELECT path FROM hierarchy
            )
         );
    END;

    CREATE TRIGGER folder_asset_ad AFTER DELETE ON asset BEGIN
        UPDATE folder
           SET descendant_asset_count = descendant_asset_count - 1,
               direct_asset_count = direct_asset_count -
                   CASE WHEN instr(substr(old.path, length(path) + 1), '/') = 0 THEN 1 ELSE 0 END
         WHERE source_id = old.source_id AND path IN (
            SELECT path FROM (
                WITH RECURSIVE hierarchy(path, rest) AS (
                    SELECT '', old.path
                    UNION ALL
                    SELECT path || substr(rest, 1, instr(rest, '/')),
                           substr(rest, instr(rest, '/') + 1)
                      FROM hierarchy WHERE instr(rest, '/') > 0
                )
                SELECT path FROM hierarchy
            )
         );
        DELETE FROM folder
         WHERE source_id = old.source_id AND path <> '' AND descendant_asset_count = 0
           AND path IN (
            SELECT path FROM (
                WITH RECURSIVE hierarchy(path, rest) AS (
                    SELECT '', old.path
                    UNION ALL
                    SELECT path || substr(rest, 1, instr(rest, '/')),
                           substr(rest, instr(rest, '/') + 1)
                      FROM hierarchy WHERE instr(rest, '/') > 0
                )
                SELECT path FROM hierarchy
            )
         );
    END;

    -- A path/source move is logically one removal plus one insertion, but UPDATE does not fire the
    -- INSERT/DELETE triggers. Repeat those two bounded ancestor walks here so direct SQL importers
    -- and future rename support cannot leave stale counts.
    CREATE TRIGGER folder_asset_au AFTER UPDATE OF source_id, path ON asset
    WHEN old.source_id <> new.source_id OR old.path <> new.path BEGIN
        UPDATE folder
           SET descendant_asset_count = descendant_asset_count - 1,
               direct_asset_count = direct_asset_count -
                   CASE WHEN instr(substr(old.path, length(path) + 1), '/') = 0 THEN 1 ELSE 0 END
         WHERE source_id = old.source_id AND path IN (
            SELECT path FROM (
                WITH RECURSIVE hierarchy(path, rest) AS (
                    SELECT '', old.path
                    UNION ALL
                    SELECT path || substr(rest, 1, instr(rest, '/')),
                           substr(rest, instr(rest, '/') + 1)
                      FROM hierarchy WHERE instr(rest, '/') > 0
                )
                SELECT path FROM hierarchy
            )
         );
        INSERT OR IGNORE INTO folder(source_id, path, parent_path, name)
        SELECT new.source_id, path, parent_path, name FROM (
            WITH RECURSIVE hierarchy(path, parent_path, name, rest) AS (
                SELECT '', '', '', new.path
                UNION ALL
                SELECT path || substr(rest, 1, instr(rest, '/')),
                       path,
                       substr(rest, 1, instr(rest, '/') - 1),
                       substr(rest, instr(rest, '/') + 1)
                  FROM hierarchy WHERE instr(rest, '/') > 0
            )
            SELECT path, parent_path, name FROM hierarchy
        );
        UPDATE folder
           SET descendant_asset_count = descendant_asset_count + 1,
               direct_asset_count = direct_asset_count +
                   CASE WHEN instr(substr(new.path, length(path) + 1), '/') = 0 THEN 1 ELSE 0 END
         WHERE source_id = new.source_id AND path IN (
            SELECT path FROM (
                WITH RECURSIVE hierarchy(path, rest) AS (
                    SELECT '', new.path
                    UNION ALL
                    SELECT path || substr(rest, 1, instr(rest, '/')),
                           substr(rest, instr(rest, '/') + 1)
                      FROM hierarchy WHERE instr(rest, '/') > 0
                )
                SELECT path FROM hierarchy
            )
         );
        DELETE FROM folder
         WHERE source_id = old.source_id AND path <> '' AND descendant_asset_count = 0
           AND path IN (
            SELECT path FROM (
                WITH RECURSIVE hierarchy(path, rest) AS (
                    SELECT '', old.path
                    UNION ALL
                    SELECT path || substr(rest, 1, instr(rest, '/')),
                           substr(rest, instr(rest, '/') + 1)
                      FROM hierarchy WHERE instr(rest, '/') > 0
                )
                SELECT path FROM hierarchy
            )
         );
    END;
    "#,
    // ── V21: streamed scan generations (issue #139) ─────────────────────────────────────────
    // A scan stamps each observed asset as it streams from the source. Only an exhaustively walked
    // source advances missing status, in one indexed database-side update. This replaces the two
    // catalog-sized Rust path collections and makes cancellation/failure safe: an unfinished walk
    // simply never runs the generation-finalisation statement.
    r#"
    ALTER TABLE source ADD COLUMN scan_generation INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE asset ADD COLUMN seen_generation INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX idx_asset_source_seen_generation
        ON asset(source_id, seen_generation);
    CREATE TRIGGER asset_seen_generation_ai AFTER INSERT ON asset BEGIN
        UPDATE asset
           SET seen_generation = (SELECT scan_generation FROM source WHERE id = new.source_id)
         WHERE rowid = new.rowid;
    END;
    "#,
    // ── V22: keyset browse keys and indexes (issue #133) ─────────────────────────────────────
    // Size shown in the grid includes model companions, which cannot be indexed as a cross-table
    // expression. Materialize that one browse key and keep it synchronized at both write seams.
    // SQLite can traverse each `(key,id)` B-tree forward or backward, serving both directions with
    // the id as the deterministic keyset tie-breaker.
    r#"
    ALTER TABLE asset ADD COLUMN browse_size_bytes INTEGER;
    UPDATE asset
       SET browse_size_bytes = size_bytes + COALESCE(
           (SELECT dependency_bytes FROM model_attr WHERE asset_id = asset.id), 0
       );

    CREATE INDEX idx_asset_browse_name ON asset(filename, id);
    CREATE INDEX idx_asset_browse_scanned ON asset(scanned_at, id);
    CREATE INDEX idx_asset_browse_size ON asset(browse_size_bytes, id);

    CREATE TRIGGER asset_browse_size_ai AFTER INSERT ON asset BEGIN
        UPDATE asset SET browse_size_bytes = new.size_bytes WHERE rowid = new.rowid;
    END;
    CREATE TRIGGER asset_browse_size_au AFTER UPDATE OF size_bytes ON asset BEGIN
        UPDATE asset
           SET browse_size_bytes = new.size_bytes + COALESCE(
               (SELECT dependency_bytes FROM model_attr WHERE asset_id = new.id), 0
           )
         WHERE rowid = new.rowid;
    END;
    CREATE TRIGGER model_browse_size_ai AFTER INSERT ON model_attr BEGIN
        UPDATE asset
           SET browse_size_bytes = size_bytes + COALESCE(new.dependency_bytes, 0)
         WHERE id = new.asset_id;
    END;
    CREATE TRIGGER model_browse_size_au AFTER UPDATE OF dependency_bytes ON model_attr BEGIN
        UPDATE asset
           SET browse_size_bytes = size_bytes + COALESCE(new.dependency_bytes, 0)
         WHERE id = new.asset_id;
    END;
    CREATE TRIGGER model_browse_size_ad AFTER DELETE ON model_attr BEGIN
        UPDATE asset SET browse_size_bytes = size_bytes WHERE id = old.asset_id;
    END;
    "#,
    // ── V23: incremental derivative backlog (issue #140) ────────────────────────────────────
    // Zero means the content-keyed thumbnail/model-preview slice still needs warming. A successful
    // background render advances the marker; content changes and explicit cache clears reset it.
    // Keeping this beside the asset makes restart recovery durable and turns every wake from a
    // catalog sweep into an indexed walk over only pending rows.
    r#"
    ALTER TABLE asset ADD COLUMN derivative_version INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX idx_asset_analysis_planner
        ON asset(analysis_version, source_id, id);
    CREATE INDEX idx_asset_force_planner
        ON asset(source_id, id);
    CREATE INDEX idx_asset_derivative_pending
        ON asset(derivative_version, source_id, id)
        WHERE media_type IN ('image','video','model');
    "#,
    // ── V24: durable duplicate-review decisions (issue #111) ───────────────────────────────
    // Review metadata is library state, not deletion state. The chosen keep is advisory; catalog
    // removal remains an explicit operation and the source file is never touched.
    r#"
    CREATE TABLE duplicate_review (
        review_key   TEXT PRIMARY KEY,
        state        TEXT NOT NULL CHECK(state IN ('pending','resolved','dismissed')),
        chosen_keep  BLOB,
        updated_at   INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX idx_duplicate_review_state ON duplicate_review(state, updated_at);
    "#,
];

#[cfg(test)]
mod tests {
    use super::MIGRATIONS;
    use rusqlite::{Connection, OptionalExtension};

    /// Apply the first `n` migrations to a fresh in-memory database.
    fn db_at(n: usize) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for step in &MIGRATIONS[..n] {
            conn.execute_batch(step).unwrap();
        }
        conn
    }

    /// An FTS5 rebuild has to carry the index-only columns across, and the only way to know it does
    /// is to upgrade a *populated* database — a fresh one is empty when the migration runs, so every
    /// other test in the workspace exercises the SQL without exercising the data movement.
    ///
    /// `tokens`, `tags`, `note`, and `text` exist nowhere but inside `asset_fts`: losing them
    /// silently un-indexes filename sub-tokens, every tag, every user note, and every document body,
    /// with no error and no way to notice short of a user's search going quiet. V7, V11, V12, and
    /// V13 are the rebuilds so far; this walks a *populated* database through the last two of them
    /// in sequence, so a value dropped by either shows up here.
    #[test]
    fn the_fts_rebuilds_preserve_the_index_only_columns() {
        // Populate at V11, before `note` and `folder` exist.
        let conn = db_at(11);
        conn.execute_batch(
            "INSERT INTO source (id, name, kind, connection, created_at, updated_at)
                VALUES (x'01', 's', 'local_fs', '/tmp', 0, 0);
             INSERT INTO asset (id, source_id, path, filename, scanned_at, media_type, format,
                                created_at, updated_at)
                VALUES (x'02', x'01', 'Weapons/Rifles/ak47_lowpoly.fbx', 'ak47_lowpoly.fbx', 0,
                        'model', 'fbx', 0, 0);
             UPDATE asset_fts SET tokens = 'ak47 ak 47 low poly fbx', tags = 'rifle weapon',
                                  text = 'the quick brown fox';",
        )
        .unwrap();

        // V12 adds `note` (and drops the dead V1 `asset.notes` column) …
        conn.execute_batch(MIGRATIONS[11]).unwrap();
        let has_notes_column: bool = conn
            .prepare("SELECT * FROM asset")
            .unwrap()
            .column_names()
            .contains(&"notes");
        assert!(!has_notes_column, "dead asset.notes column survived V12");
        conn.execute_batch("UPDATE asset_fts SET note = 'client rejected this variant';")
            .unwrap();

        // … V13 adds `folder`, which must carry the note across as well as the older three.
        conn.execute_batch(MIGRATIONS[12]).unwrap();

        let (tokens, tags, note, folder, text): (String, String, String, String, String) = conn
            .query_row(
                "SELECT tokens, tags, note, folder, text FROM asset_fts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            tokens, "ak47 ak 47 low poly fbx",
            "tokens lost in a rebuild"
        );
        assert_eq!(tags, "rifle weapon", "tags lost in a rebuild");
        assert_eq!(
            note, "client rejected this variant",
            "note lost in a rebuild"
        );
        assert_eq!(
            text, "the quick brown fox",
            "document text lost in a rebuild"
        );
        // `folder` is the one column a rebuild can *derive* rather than carry: it back-fills from
        // `asset.path`, minus the filename. The SQL back-fill is whole-segment only (the real
        // tokeniser is in Rust and refines this on the next scan), which is what this asserts.
        assert_eq!(
            folder, "weapons rifles",
            "folder was not back-filled from the stored path"
        );
    }

    /// A file at the source root has no folder to index, and must not pick up its own filename —
    /// the back-fill's `substr` arithmetic is easy to get one character wrong.
    #[test]
    fn the_folder_backfill_leaves_root_level_files_empty() {
        let conn = db_at(11);
        conn.execute_batch(
            "INSERT INTO source (id, name, kind, connection, created_at, updated_at)
                VALUES (x'01', 's', 'local_fs', '/tmp', 0, 0);
             INSERT INTO asset (id, source_id, path, filename, scanned_at, media_type, format,
                                created_at, updated_at)
                VALUES (x'02', x'01', 'loose.png', 'loose.png', 0, 'image', 'png', 0, 0);",
        )
        .unwrap();
        conn.execute_batch(MIGRATIONS[11]).unwrap();
        conn.execute_batch(MIGRATIONS[12]).unwrap();

        let folder: String = conn
            .query_row("SELECT folder FROM asset_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(folder, "", "a root-level file has no folder terms");
    }

    #[test]
    fn filename_trigram_migration_backfills_and_tracks_asset_lifecycle() {
        let conn = db_at(18);
        conn.execute_batch(
            "INSERT INTO source (id, name, kind, connection, created_at, updated_at)
                VALUES (x'01', 's', 'local_fs', '/tmp', 0, 0);
             INSERT INTO asset (id, source_id, path, filename, scanned_at, media_type, format,
                                created_at, updated_at)
                VALUES (x'02', x'01', 'AK47_LowPoly.fbx', 'AK47_LowPoly.fbx', 0,
                        'model', 'fbx', 0, 0);",
        )
        .unwrap();

        conn.execute_batch(MIGRATIONS[18]).unwrap();
        let matches = |term: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM asset_filename_trigram
                 WHERE asset_filename_trigram MATCH ?1",
                [format!("\"{term}\"")],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(matches("k47"), 1, "existing asset was not backfilled");

        conn.execute_batch(
            "INSERT INTO asset (id, source_id, path, filename, scanned_at, media_type, format,
                                created_at, updated_at)
                VALUES (x'03', x'01', 'impact...final.wav', 'impact...final.wav', 0,
                        'audio', 'wav', 0, 0);",
        )
        .unwrap();
        assert_eq!(matches("..."), 1, "insert trigger missed the filename");

        conn.execute(
            "UPDATE asset SET filename = 'renamed.mesh' WHERE id = x'02'",
            [],
        )
        .unwrap();
        assert_eq!(
            matches("k47"),
            0,
            "update trigger retained the old filename"
        );
        assert_eq!(matches("name"), 1, "update trigger missed the new filename");

        conn.execute("DELETE FROM asset WHERE id = x'03'", [])
            .unwrap();
        assert_eq!(matches("..."), 0, "delete trigger left a stale trigram row");
    }

    #[test]
    fn folder_hierarchy_migration_backfills_and_tracks_moves_and_deletes() {
        let conn = db_at(19);
        conn.execute_batch(
            "INSERT INTO source (id, name, kind, connection, created_at, updated_at)
                VALUES (x'01', 's', 'local_fs', '/tmp', 0, 0);
             INSERT INTO asset (id, source_id, path, filename, scanned_at, media_type, format,
                                created_at, updated_at) VALUES
                (x'02', x'01', 'loose.png', 'loose.png', 0, 'image', 'png', 0, 0),
                (x'03', x'01', 'Art/one.png', 'one.png', 0, 'image', 'png', 0, 0),
                (x'04', x'01', 'Art/Deep/two.png', 'two.png', 0, 'image', 'png', 0, 0),
                (x'05', x'01', 'Artist/three.png', 'three.png', 0, 'image', 'png', 0, 0);",
        )
        .unwrap();

        conn.execute_batch(MIGRATIONS[19]).unwrap();
        let counts = |source: &[u8], path: &str| -> Option<(i64, i64)> {
            conn.query_row(
                "SELECT direct_asset_count, descendant_asset_count
                   FROM folder WHERE source_id = ?1 AND path = ?2",
                rusqlite::params![source, path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .unwrap()
        };
        assert_eq!(counts(&[1], ""), Some((1, 4)));
        assert_eq!(counts(&[1], "Art/"), Some((1, 2)));
        assert_eq!(counts(&[1], "Art/Deep/"), Some((1, 1)));
        assert_eq!(counts(&[1], "Artist/"), Some((1, 1)));

        conn.execute_batch(
            "INSERT INTO source (id, name, kind, connection, created_at, updated_at)
                VALUES (x'06', 'new', 'sftp', '{}', 0, 0);",
        )
        .unwrap();
        assert_eq!(
            counts(&[6], ""),
            Some((0, 0)),
            "a source inserted after V20 did not get an empty root"
        );
        conn.execute_batch(
            "INSERT INTO asset (id, source_id, path, filename, scanned_at, media_type, format,
                                created_at, updated_at)
                VALUES (x'07', x'06', 'Incoming/new.png', 'new.png', 0,
                        'image', 'png', 0, 0);",
        )
        .unwrap();
        assert_eq!(
            counts(&[6], ""),
            Some((0, 1)),
            "an asset inserted after V20 did not maintain the root"
        );
        assert_eq!(counts(&[6], "Incoming/"), Some((1, 1)));

        conn.execute(
            "UPDATE asset SET path = 'Art/New/three.png' WHERE id = x'05'",
            [],
        )
        .unwrap();
        assert_eq!(
            counts(&[1], ""),
            Some((1, 4)),
            "a move changed the root total"
        );
        assert_eq!(counts(&[1], "Art/"), Some((1, 3)));
        assert_eq!(counts(&[1], "Art/New/"), Some((1, 1)));
        assert_eq!(
            counts(&[1], "Artist/"),
            None,
            "empty old branch was retained"
        );

        conn.execute(
            "UPDATE asset SET source_id = x'06', path = 'Moved/three.png' WHERE id = x'05'",
            [],
        )
        .unwrap();
        assert_eq!(counts(&[1], ""), Some((1, 3)));
        assert_eq!(counts(&[1], "Art/New/"), None);
        assert_eq!(counts(&[6], ""), Some((0, 2)));
        assert_eq!(counts(&[6], "Moved/"), Some((1, 1)));

        conn.execute("DELETE FROM asset WHERE id = x'04'", [])
            .unwrap();
        assert_eq!(counts(&[1], ""), Some((1, 2)));
        assert_eq!(counts(&[1], "Art/"), Some((1, 1)));
        assert_eq!(counts(&[1], "Art/Deep/"), None);

        conn.execute_batch("DELETE FROM source WHERE id IN (x'01', x'06');")
            .unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM folder", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0, "source cascade left hierarchy rows behind");
    }

    #[test]
    fn derivative_backlog_migration_marks_existing_and_new_assets_pending() {
        let conn = db_at(22);
        conn.execute_batch(
            "INSERT INTO source (id, name, kind, connection, created_at, updated_at)
                VALUES (x'01', 's', 'local_fs', '{\"kind\":\"local_fs\",\"root\":\"/tmp\"}', 0, 0);
             INSERT INTO asset (id, source_id, path, filename, scanned_at, media_type, format,
                                created_at, updated_at)
                VALUES (x'02', x'01', 'old.png', 'old.png', 0, 'image', 'png', 0, 0);",
        )
        .unwrap();
        conn.execute_batch(MIGRATIONS[22]).unwrap();
        conn.execute_batch(
            "INSERT INTO asset (id, source_id, path, filename, scanned_at, media_type, format,
                                created_at, updated_at)
                VALUES (x'03', x'01', 'new.png', 'new.png', 0, 'image', 'png', 0, 0);",
        )
        .unwrap();
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset WHERE derivative_version=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 2);
    }

    #[test]
    fn browse_key_migration_backfills_and_tracks_model_dependency_size() {
        let conn = db_at(21);
        conn.execute_batch(
            "INSERT INTO source (id, name, kind, connection, created_at, updated_at)
                VALUES (x'01', 's', 'local_fs', '{}', 0, 0);
             INSERT INTO asset (id, source_id, path, filename, size_bytes, scanned_at, media_type,
                                format, created_at, updated_at) VALUES
                (x'02', x'01', 'old.glb', 'old.glb', 10, 1, 'model', 'glb', 0, 0),
                (x'03', x'01', 'unknown.glb', 'unknown.glb', NULL, 2, 'model', 'glb', 0, 0);
             INSERT INTO model_attr (asset_id, dependency_bytes) VALUES (x'02', 5), (x'03', 7);",
        )
        .unwrap();

        conn.execute_batch(MIGRATIONS[21]).unwrap();
        let browse_size = |id: u8| -> Option<i64> {
            conn.query_row(
                "SELECT browse_size_bytes FROM asset WHERE id = ?1",
                [vec![id]],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            browse_size(2),
            Some(15),
            "existing model was not backfilled"
        );
        assert_eq!(
            browse_size(3),
            None,
            "unknown base size must remain unknown"
        );

        conn.execute_batch(
            "INSERT INTO asset (id, source_id, path, filename, size_bytes, scanned_at, media_type,
                                format, created_at, updated_at)
                VALUES (x'04', x'01', 'new.glb', 'new.glb', 20, 3, 'model', 'glb', 0, 0);
             INSERT INTO model_attr (asset_id, dependency_bytes) VALUES (x'04', 7);",
        )
        .unwrap();
        assert_eq!(browse_size(4), Some(27));
        conn.execute(
            "UPDATE model_attr SET dependency_bytes = 8 WHERE asset_id = x'04'",
            [],
        )
        .unwrap();
        assert_eq!(browse_size(4), Some(28));
        conn.execute("UPDATE asset SET size_bytes = 30 WHERE id = x'04'", [])
            .unwrap();
        assert_eq!(browse_size(4), Some(38));
        conn.execute("DELETE FROM model_attr WHERE asset_id = x'04'", [])
            .unwrap();
        assert_eq!(browse_size(4), Some(30));

        let indexes = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_asset_browse_%'")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(indexes.len(), 3);
    }
}
