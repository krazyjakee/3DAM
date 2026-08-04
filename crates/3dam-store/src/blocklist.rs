//! Asset removal + the rescan blocklist (issue #21) — part of the `Store` impl, split out of `lib.rs`.
//!
//! Removing an asset deletes its catalog row (cascading to attrs, tags, collection membership, and
//! embeddings via `ON DELETE CASCADE`). Optionally the asset's content hash is recorded on the
//! blocklist so a subsequent scan/watch/auto-rescan skips the same bytes rather than re-importing
//! them (see [`crate::scan`]). Blocking is content-addressed, not per-row: it also purges every
//! byte-identical copy already in the catalog (the whole exact-duplicate group), so "remove + block"
//! is a one-shot dedup disposal rather than a per-copy chore. Non-destructive: source files are
//! never touched.
use super::*;

impl Store {
    /// Remove an asset. Without `block`, deletes just this one row. With `block`, treats the bytes as
    /// unwanted: purges **every** asset sharing the same content hash (the entire byte-identical
    /// group) and records that hash on the blocklist — capturing the filename as a display label — so
    /// future scans skip it. Returns `NotFound` if the id is unknown.
    pub fn remove_asset(&self, id: &AssetId, block: bool) -> Result<(), LibError> {
        let conn = self.write();
        // Read the hash + filename before the row is gone — needed for a meaningful blocklist entry.
        let row: Option<(Option<Vec<u8>>, String)> = conn
            .query_row(
                "SELECT content_hash, filename FROM asset WHERE id = ?1",
                params![id.as_bytes().to_vec()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        let (hash_blob, filename) = row.ok_or_else(|| LibError::NotFound(format!("asset {id}")))?;

        // Blocking a content-addressable asset means the user never wants these bytes: delete every
        // byte-identical copy at once so the whole exact-duplicate group clears, not just the clicked
        // one, then record the hash. (A federated row carries no bytes to block — fall through to the
        // single-row delete below.) Deletes cascade to audio/image/model attrs, asset_tag,
        // collection_member, embedding — all keyed ON DELETE CASCADE to asset.id.
        if block {
            if let Some(hash) = hash_blob {
                conn.execute("DELETE FROM asset WHERE content_hash = ?1", params![hash])
                    .map_err(internal)?;
                conn.execute(
                    "INSERT INTO blocklist (content_hash, label, blocked_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(content_hash) DO UPDATE SET label = excluded.label",
                    params![hash, filename, now_ms()],
                )
                .map_err(internal)?;
                return Ok(());
            }
        }

        conn.execute(
            "DELETE FROM asset WHERE id = ?1",
            params![id.as_bytes().to_vec()],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Every blocked content hash, newest first — the "removed + blocked" management surface.
    pub fn list_blocklist(&self) -> Result<Vec<BlockEntry>, LibError> {
        let conn = self.write();
        let mut stmt = conn
            .prepare(
                "SELECT content_hash, label, blocked_at FROM blocklist ORDER BY blocked_at DESC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                let blob: Vec<u8> = r.get(0)?;
                let label: Option<String> = r.get(1)?;
                let blocked_at: i64 = r.get(2)?;
                Ok((blob, label, blocked_at))
            })
            .map_err(internal)?;
        let mut out = Vec::new();
        for r in rows {
            let (blob, label, blocked_at) = r.map_err(internal)?;
            if let Ok(bytes) = <[u8; 32]>::try_from(blob.as_slice()) {
                out.push(BlockEntry {
                    hash: ContentHash(bytes),
                    label,
                    blocked_at,
                });
            }
        }
        Ok(out)
    }

    /// Lift a block so the content can be re-imported by a later scan. `NotFound` if not blocked.
    pub fn unblock(&self, hash: &ContentHash) -> Result<(), LibError> {
        let conn = self.write();
        let n = conn
            .execute(
                "DELETE FROM blocklist WHERE content_hash = ?1",
                params![hash.as_bytes().to_vec()],
            )
            .map_err(internal)?;
        if n == 0 {
            return Err(LibError::NotFound(format!("hash {hash} is not blocked")));
        }
        Ok(())
    }

    /// Whether a content hash is on the blocklist — the scan-time gate (hot path; single indexed
    /// primary-key lookup).
    pub fn is_blocked(&self, hash: &ContentHash) -> Result<bool, LibError> {
        let conn = self.write();
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM blocklist WHERE content_hash = ?1",
                params![hash.as_bytes().to_vec()],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        Ok(found.is_some())
    }
}
