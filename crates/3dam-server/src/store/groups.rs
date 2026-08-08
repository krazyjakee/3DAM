//! Group creation, membership management, listing, and deletion.

use super::*;
use dam_api::accounts::*;

impl ServerStore {
    pub fn create_group(&self, req: &NewGroup, actor: &str) -> Result<GroupInfo, LibError> {
        let name = req.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(LibError::BadRequest("group name must be 1–64 chars".into()));
        }
        let group_id = uuid::Uuid::now_v7().simple().to_string();
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute(
                    "INSERT OR IGNORE INTO group_ (group_id, name, created) VALUES (?1, ?2, ?3)",
                    params![group_id, name, now],
                )
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::Conflict(format!("group '{name}' already exists")));
            }
            Self::audit_row(
                &conn,
                actor,
                "group.create",
                Some(&group_id),
                Some(serde_json::json!({ "name": name })),
            )?;
        }
        self.bump_visibility_gen();
        Ok(GroupInfo {
            group_id,
            name: name.to_string(),
            created: now,
            members: Vec::new(),
        })
    }

    pub fn list_groups(&self) -> Result<Vec<GroupInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT group_id, name, created FROM group_ ORDER BY name COLLATE NOCASE")
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(GroupInfo {
                    group_id: r.get(0)?,
                    name: r.get(1)?,
                    created: r.get(2)?,
                    members: Vec::new(),
                })
            })
            .map_err(internal)?;
        let mut groups: Vec<GroupInfo> = rows.collect::<Result<_, _>>().map_err(internal)?;
        let mut mstmt = conn
            .prepare("SELECT account_id FROM group_member WHERE group_id = ?1")
            .map_err(internal)?;
        for g in &mut groups {
            let rows = mstmt
                .query_map(params![g.group_id], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            g.members = rows.collect::<Result<_, _>>().map_err(internal)?;
        }
        Ok(groups)
    }

    pub fn delete_group(&self, group_id: &str, actor: &str) -> Result<(), LibError> {
        {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute("DELETE FROM group_ WHERE group_id = ?1", params![group_id])
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::NotFound(format!("group {group_id}")));
            }
            Self::audit_row(&conn, actor, "group.delete", Some(group_id), None)?;
        }
        self.bump_visibility_gen();
        Ok(())
    }

    /// Replace a group's membership set (idempotent, whole-set semantics — the admin UI edits the
    /// list and submits it). Unknown account ids are rejected up front.
    pub fn set_group_members(
        &self,
        group_id: &str,
        account_ids: &[String],
        actor: &str,
    ) -> Result<GroupInfo, LibError> {
        {
            let mut conn = self.conn.lock().unwrap();
            let tx = conn.transaction().map_err(internal)?;
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM group_ WHERE group_id = ?1",
                    params![group_id],
                    |_| Ok(true),
                )
                .optional()
                .map_err(internal)?
                .unwrap_or(false);
            if !exists {
                return Err(LibError::NotFound(format!("group {group_id}")));
            }
            for a in account_ids {
                let known: bool = tx
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
            tx.execute(
                "DELETE FROM group_member WHERE group_id = ?1",
                params![group_id],
            )
            .map_err(internal)?;
            for a in account_ids {
                tx.execute(
                    "INSERT OR IGNORE INTO group_member (group_id, account_id) VALUES (?1, ?2)",
                    params![group_id, a],
                )
                .map_err(internal)?;
            }
            Self::audit_row(
                &tx,
                actor,
                "group.members",
                Some(group_id),
                Some(serde_json::json!({ "members": account_ids })),
            )?;
            tx.commit().map_err(internal)?;
        }
        self.bump_visibility_gen();
        let groups = self.list_groups()?;
        groups
            .into_iter()
            .find(|g| g.group_id == group_id)
            .ok_or_else(|| LibError::NotFound(format!("group {group_id}")))
    }
}
