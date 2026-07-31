//! User-account / group / share wire types (issue #42, tech-spec 10 §4).
//!
//! Shared by the server (which owns `server.db` and the semantics) and every client of the
//! `/api/v1/auth/*` + `/admin/api/accounts|groups|shares` surfaces (the CLI over `--connect`, the
//! web Settings surface). The whole surface is behind the `UserAccounts` flag — off means absent
//! (ADR 0004), so none of these types imply a served endpoint by existing.

use crate::service::{Scope, Scopes};
use serde::{Deserialize, Serialize};

/// The frozen v1 role set (ADR 0009 §3): a **permission level**, not a team — groups (below) are
/// orthogonal and answer "what can this account *reach*", roles answer "what may it *do*".
/// Custom roles are post-v1.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Editor,
    Viewer,
}

impl Role {
    /// The role → scope mapping (tech-spec 10 §4.2). Guards keep checking *scopes*, never roles —
    /// this is the single place a role gains meaning.
    pub fn scopes(self) -> Scopes {
        match self {
            Role::Viewer => Scopes::none().with(Scope::Read).with(Scope::McpUse),
            Role::Editor => Scopes::none()
                .with(Scope::Read)
                .with(Scope::Write)
                .with(Scope::McpUse)
                .with(Scope::Federate),
            Role::Admin => Scopes::owner(),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Editor => "editor",
            Role::Viewer => "viewer",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "admin" => Some(Role::Admin),
            "editor" => Some(Role::Editor),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The resolved account behind a request — carried on `AuthContext` and echoed in `whoami` so a
/// client can show who is signed in without a second lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    pub account_id: String,
    pub username: String,
    pub role: Role,
}

/// One account as listed by `GET /admin/api/accounts` — never the password hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountInfo {
    pub account_id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub role: Role,
    pub disabled: bool,
    pub created: i64,
    pub last_login: Option<i64>,
}

/// Body of `POST /admin/api/accounts` — create an account (admin CRUD; self-signup only exists as
/// the first-run claim).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewAccount {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub role: Role,
}

/// Body of `PUT /admin/api/accounts/{id}` — partial update; absent fields are left unchanged.
/// Setting `password` replaces the credential and revokes the account's other sessions.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateAccount {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub password: Option<String>,
}

// ── groups (issue #42 §3) ────────────────────────────────────────────────────

/// A flat, named set of accounts — a share *target* ("Audio Team"), never a permission level.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroupInfo {
    pub group_id: String,
    pub name: String,
    pub created: i64,
    /// Member account ids (flat — groups do not nest).
    pub members: Vec<String>,
}

/// Body of `POST /admin/api/groups`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewGroup {
    pub name: String,
}

/// Body of `PUT /admin/api/groups/{id}/members` — replaces the full membership set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroupMembers {
    pub account_ids: Vec<String>,
}

// ── shares (issue #42 §3) ────────────────────────────────────────────────────

/// What kind of resource a share grants access to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareResource {
    Source,
    Collection,
}

impl ShareResource {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareResource::Source => "source",
            ShareResource::Collection => "collection",
        }
    }
    pub fn parse(s: &str) -> Option<ShareResource> {
        match s {
            "source" => Some(ShareResource::Source),
            "collection" => Some(ShareResource::Collection),
            _ => None,
        }
    }
}

/// The access level a share grants. `Write` still requires `Scope::Write` on the identity — the
/// share controls *which* resources, the role controls *whether* writes are allowed at all
/// (two independent gates; issue #42 resolution rule 4).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareAccess {
    Read,
    Write,
}

impl ShareAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareAccess::Read => "read",
            ShareAccess::Write => "write",
        }
    }
    pub fn parse(s: &str) -> Option<ShareAccess> {
        match s {
            "read" => Some(ShareAccess::Read),
            "write" => Some(ShareAccess::Write),
            _ => None,
        }
    }
}

/// One grant: a source or collection shared to exactly one account **or** one group. The
/// `resource_id` is a soft reference into `library.db` (a uuid, never recycled); orphans left by a
/// deleted resource are garbage-collected, not resolved.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShareInfo {
    pub share_id: String,
    pub resource: ShareResource,
    pub resource_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub access: ShareAccess,
    pub granted_by: String,
    pub created: i64,
}

/// Body of `POST /admin/api/shares` — exactly one of `account_id` / `group_id` must be set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewShare {
    pub resource: ShareResource,
    pub resource_id: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub group_id: Option<String>,
    pub access: ShareAccess,
}

// ── sessions + the login/claim surface (`/api/v1/auth/*`) ────────────────────

/// The public auth posture (`GET /api/v1/auth/status`, no credential required): what a client
/// needs to render the right gate — login, first-run claim, or nothing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountsStatus {
    /// The `UserAccounts` flag. When `false` the rest of the surface 404s.
    pub enabled: bool,
    /// Zero accounts exist (or the config-file escape hatch re-opened the window): the next
    /// loopback signup claims the instance as admin.
    pub unclaimed: bool,
}

/// Body of `POST /api/v1/auth/claim` (first-run) and the claim half of the signup story: only
/// honoured while unclaimed, from loopback (or a caller already holding `Admin` via bearer —
/// the off-box bootstrap-token path).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Body of `POST /api/v1/auth/login`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Reply to a successful login/claim. The session itself travels as an `HttpOnly` cookie; the
/// CSRF token is returned here (and as a readable cookie) for the double-submit check — the client
/// echoes it in `x-dam-csrf` on every cookie-authenticated mutating request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginReply {
    pub account: AccountIdentity,
    pub csrf: String,
}

/// One of the caller's own sessions (`GET /api/v1/auth/sessions`) — enough to recognise and
/// revoke a stray login. Never the secret.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub created: i64,
    pub last_seen: i64,
    pub user_agent: Option<String>,
    /// True for the session making this request.
    pub current: bool,
}
