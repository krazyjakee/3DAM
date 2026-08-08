//! Resource sharing and account visibility resolution for the server store.

use super::*;
use dam_api::accounts::*;
use dam_api::id::{CollectionId, SourceId};
use dam_api::service::{Visibility, VisibilityScope};
use std::sync::atomic::Ordering;

impl ServerStore {
    pub fn create_share(&self, req: &NewShare, actor: &str) -> Result<ShareInfo, LibError> {
        // Exactly one target — mirrors the schema CHECK, but with a friendlier error.
        match (&req.account_id, &req.group_id) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(LibError::BadRequest(
                    "a share targets exactly one of account_id or group_id".into(),
                ))
            }
        }
        // The resource id must at least be a well-formed uuid — the resource itself lives across
        // the database boundary in library.db (soft reference; the route layer validates liveness).
        //
        // **Canonicalised on the way in.** `Uuid::parse_str` is permissive (hyphenated, simple,
        // braced, urn:…), but every *other* participant compares the stored string literally: the
        // orphan GC binds `SourceId::to_string()`, and the web share list filters on string
        // equality. A share stored in a non-canonical spelling would be live (visibility resolution
        // re-parses) yet invisible in the UI and immune to GC — an unrevocable grant. Storing the
        // canonical hyphenated form makes all three agree by construction.
        let resource_id = uuid::Uuid::parse_str(&req.resource_id)
            .map_err(|_| LibError::BadRequest("invalid resource id".into()))?
            .to_string();
        let share_id = uuid::Uuid::now_v7().simple().to_string();
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            if let Some(a) = &req.account_id {
                let known: bool = conn
                    .query_row(
                        "SELECT 1 FROM account WHERE account_id = ?1",
                        params![a],
                        |_| Ok(true),
                    )
                    .optional()
                    .map_err(internal)?
                    .unwrap_or(false);
                if !known {
                    return Err(LibError::BadRequest(format!("unknown account {a}")));
                }
            }
            if let Some(g) = &req.group_id {
                let known: bool = conn
                    .query_row(
                        "SELECT 1 FROM group_ WHERE group_id = ?1",
                        params![g],
                        |_| Ok(true),
                    )
                    .optional()
                    .map_err(internal)?
                    .unwrap_or(false);
                if !known {
                    return Err(LibError::BadRequest(format!("unknown group {g}")));
                }
            }
            conn.execute(
                "INSERT INTO share
                 (share_id, resource, resource_id, account_id, group_id, access, granted_by, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    share_id,
                    req.resource.as_str(),
                    resource_id,
                    req.account_id,
                    req.group_id,
                    req.access.as_str(),
                    actor,
                    now
                ],
            )
            .map_err(internal)?;
            Self::audit_row(
                &conn,
                actor,
                "share.create",
                Some(&share_id),
                Some(serde_json::json!({
                    "resource": req.resource,
                    "resource_id": resource_id,
                    "account_id": req.account_id,
                    "group_id": req.group_id,
                    "access": req.access,
                })),
            )?;
        }
        self.bump_visibility_gen();
        Ok(ShareInfo {
            share_id,
            resource: req.resource,
            resource_id,
            account_id: req.account_id.clone(),
            group_id: req.group_id.clone(),
            access: req.access,
            granted_by: actor.to_string(),
            created: now,
        })
    }

    pub fn list_shares(&self) -> Result<Vec<ShareInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT share_id, resource, resource_id, account_id, group_id, access,
                        granted_by, created
                 FROM share ORDER BY created DESC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                let resource_s: String = r.get(1)?;
                let access_s: String = r.get(5)?;
                Ok(ShareInfo {
                    share_id: r.get(0)?,
                    resource: ShareResource::parse(&resource_s).unwrap_or(ShareResource::Source),
                    resource_id: r.get(2)?,
                    account_id: r.get(3)?,
                    group_id: r.get(4)?,
                    access: ShareAccess::parse(&access_s).unwrap_or(ShareAccess::Read),
                    granted_by: r.get(6)?,
                    created: r.get(7)?,
                })
            })
            .map_err(internal)?;
        rows.collect::<Result<_, _>>().map_err(internal)
    }

    pub fn delete_share(&self, share_id: &str, actor: &str) -> Result<(), LibError> {
        {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute("DELETE FROM share WHERE share_id = ?1", params![share_id])
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::NotFound(format!("share {share_id}")));
            }
            Self::audit_row(&conn, actor, "share.delete", Some(share_id), None)?;
        }
        self.bump_visibility_gen();
        Ok(())
    }

    /// Orphan GC for the cross-database soft reference (issue #42 §3): when a source/collection is
    /// deleted in `library.db`, its share rows here are dead weight — drop them. Ids are uuids and
    /// never recycled, so a missed GC is only clutter, never a grant to a future resource.
    pub fn remove_shares_for_resource(
        &self,
        resource: ShareResource,
        resource_id: &str,
        actor: &str,
    ) -> Result<u64, LibError> {
        let n = {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute(
                    "DELETE FROM share WHERE resource = ?1 AND resource_id = ?2",
                    params![resource.as_str(), resource_id],
                )
                .map_err(internal)?;
            if n > 0 {
                Self::audit_row(
                    &conn,
                    actor,
                    "share.gc",
                    Some(resource_id),
                    Some(serde_json::json!({ "resource": resource, "removed": n })),
                )?;
            }
            n
        };
        if n > 0 {
            self.bump_visibility_gen();
        }
        Ok(n as u64)
    }

    // ── visibility resolution (issue #42 §3 resolution rules) ────────────────

    /// Resolve an account's visibility ceiling — **at auth time, in the server** — so the engine
    /// only ever sees a finished value and never learns what an account or group is:
    /// 1. admins bypass sharing entirely (`Full`);
    /// 2. direct shares ∪ shares to any group the account belongs to;
    /// 3. the most permissive access level wins per resource (`write` ⊃ `read`);
    /// 4. `write` grants land in the `write_*` sets — whether the identity may write *at all*
    ///    stays a scope question (rule 4: two independent gates).
    pub fn resolve_visibility(&self, account: &AccountIdentity) -> Result<Visibility, LibError> {
        if account.role == Role::Admin {
            return Ok(Visibility::Full);
        }
        let conn = self.conn.lock().unwrap();
        let mut scope = VisibilityScope::default();
        let mut stmt = conn
            .prepare(
                "SELECT resource, resource_id, access FROM share
                 WHERE account_id = ?1
                    OR group_id IN (SELECT group_id FROM group_member WHERE account_id = ?1)",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![account.account_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(internal)?;
        for row in rows {
            let (resource_s, rid, access_s) = row.map_err(internal)?;
            let write = access_s == "write";
            match ShareResource::parse(&resource_s) {
                Some(ShareResource::Source) => {
                    if let Ok(id) = rid.parse::<SourceId>() {
                        scope.sources.insert(id);
                        if write {
                            scope.write_sources.insert(id);
                        }
                    }
                }
                Some(ShareResource::Collection) => {
                    if let Ok(id) = rid.parse::<CollectionId>() {
                        scope.collections.insert(id);
                        if write {
                            scope.write_collections.insert(id);
                        }
                    }
                }
                None => {}
            }
        }
        Ok(Visibility::Restricted(scope))
    }

    /// The generation counter behind live share/membership edits — a subscriber caches this with
    /// its resolved ceiling and re-resolves when it moves (issue #42: a share change must not keep
    /// serving a stale WS stream).
    pub fn visibility_generation(&self) -> u64 {
        self.visibility_gen.load(Ordering::Relaxed)
    }

    /// `pub(super)` so the sibling `store::oidc` can call it too: provisioning an account from a
    /// verified subject changes who can see what, exactly as `create_account` does.
    pub(super) fn bump_visibility_gen(&self) {
        self.visibility_gen.fetch_add(1, Ordering::Relaxed);
    }
}
