# 10 — Auth, accounts & feature flags

Status: **Draft v0.1** · Scope: the server-side auth layer (off / anonymous / token / OIDC-OAuth2) that turns a request into an **auth context**, credential storage (client keychain vs server secret store), the runtime **feature-flag** store and its config-file ↔ admin-UI reconciliation and live-vs-restart lifecycle, opt-in **user accounts** (roles → scopes, visibility scope, bootstrap, audit), and the **admin API** that the web UI and CLI both drive.

This file is the low-level design for the security and administration layer that wraps every `serve`-mode surface. It sits in the **`3dam-server`** crate ([01](01-architecture-and-crates.md) §1, key deps `rustls`/`oauth2`/`argon2`/`keyring`) and implements the mechanics decided in [ADR 0004](../adr/0004-feature-flags-admin.md) and specified at the product level in [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.7 (federation & authentication), §6.11 (server administration), and §5 (the server-side settings/flags/accounts records that live *outside* the library file). It does not restate that rationale or re-decide the ADR — it fills in the types, schemas, and routes.

**Borders with siblings** (do not write outside this scope):

- [03](03-library-service-and-api.md) owns the `LibraryService` trait, its DTOs, and the HTTP/WS API surface. This file defines the auth *middleware* that wraps that surface and the `AuthContext` it injects; it does not define the library methods themselves.
- [09](09-server-and-web-client.md) hosts the `serve` config file, the web admin UI, and the routing/mount plumbing. This file defines the **flag/account semantics** and the **admin API** the config loader and admin UI drive; file 09 wires the router and renders the UI.
- [07](07-sources-and-federation.md) consumes the `AuthContext` for inbound federation and carries the **client** `AuthConfig` outbound to peers.
- [11](11-mcp-server.md) sits behind this auth layer; its `POST /mcp` route is mounted or unmounted by the MCP flag, and its tools read the same `AuthContext` and scopes.
- [13](13-cli.md) drives the same admin API as the web UI (one source of truth) for headless administration.

Compile-time note: the runtime flags here presuppose the capability was **compiled in** — the `auth-oidc` Cargo feature gates `oauth2`/`openidconnect`, the `server`/`mcp` features gate the surfaces ([01](01-architecture-and-crates.md) §6). A runtime flag can only enable what the build carries.

---

## 1. The auth layer

### 1.1 Auth modes

Authentication is itself a feature (the `authentication` flag, §3). Its value selects one **auth mode** that governs the whole shared surface — web client, HTTP/WS API, MCP, and inbound federation — at once (PRODUCT_SPEC §6.7: "one surface, one policy"):

```rust
// crate: 3dam-server
pub enum AuthMode {
    /// No authentication. Every request is the anonymous identity.
    /// The safe default is Off *and* bound to localhost (§3).
    Off,
    /// Public read: unauthenticated callers get a fixed anonymous scope set;
    /// presenting a valid credential can still elevate (token/account).
    Anonymous,
    /// Bearer token / API key. The simple default for a private instance.
    Token,
    /// OIDC / OAuth2 bearer (JWT) validated against a configured issuer.
    /// Requires the `auth-oidc` build feature. The open-standard extension path.
    Oidc(OidcConfig),
}
```

`Off` and `Anonymous` differ only in intent and labelling of the resulting identity; both admit unauthenticated requests. The distinction is load-bearing for the admin UI's "exposed without auth" warning (§3.4, DESIGN_GUIDELINES §3.6) and for what an anonymous caller is *granted* (§4.2). When accounts are on (§4), `Off` is disallowed — you cannot require login and also accept no credential — so enabling accounts forces the mode to at least `Token`.

### 1.2 Authenticating a request → `AuthContext`

Every inbound request passes through one auth middleware (an axum layer mounted by [09](09-server-and-web-client.md)) that resolves an **`AuthContext`** and attaches it as a request extension. The API handlers ([03](03-library-service-and-api.md)), the MCP tools ([11](11-mcp-server.md)), and the inbound-federation endpoint ([07](07-sources-and-federation.md)) all read this one type — there is no second policy path.

```rust
// crate: 3dam-server — the single value every guarded surface consumes.
pub struct AuthContext {
    pub identity: Identity,
    pub scopes: ScopeSet,           // the granted, effective scopes (§4.2)
    pub visibility: VisibilityScope, // ceiling on which sources/collections are visible (§4.3)
    pub via: AuthVia,               // how this request authenticated (for audit)
}

pub enum Identity {
    Anonymous,
    Token { token_id: TokenId, label: String },     // an API-key record (§1.4)
    Account { account_id: AccountId, name: String }, // a user account (§4)
    Peer { peer_id: PeerId },                        // an inbound federated instance (§07)
}

pub enum AuthVia { None, ApiKey, Oidc, Session }
```

Resolution order in the middleware:

1. **Mode `Off`** → `AuthContext { identity: Anonymous, scopes: anon_scopes(), .. }` with no credential inspected.
2. A credential is presented (`Authorization: Bearer …`, an API-key header, or a session cookie for the web client):
   - **Token mode** → look the token up in the server token store (§1.4); a hit yields its recorded scopes/visibility, a miss is `401`.
   - **OIDC mode** → validate the JWT against the issuer's JWKS (signature, `iss`, `aud`, `exp`); map the verified subject to an account (§4) or to a default scope set for known-but-account-less subjects.
   - **Session** (web client, post-login) → validate the session token against the session store (§4.4) and load the account.
3. **No credential, mode `Anonymous`** → the anonymous identity with `anon_scopes()`.
4. **No credential, mode requires one** (accounts on, or a scope beyond anon requested) → `401`.

The context is computed once per request; scope checks downstream are pure set-membership tests against `ScopeSet`, never a re-derivation. A handler that needs a capability calls a small guard (`ctx.require(Scope::Write)?` → `403` on miss).

**`GET /api/v1/whoami`** reflects the resolved context back to the caller — `{ identity, scopes, anonymous }` — so a front-end can shape its UI to the granted scopes (disable a write control rather than let the action 403). It requires no scope of its own: it reports whatever the presented credential resolves to, and under `Token` mode with no credential it still `401`s (the client's cue to render a login gate). This is the "one gate, then permissions decide" model made legible to the client instead of discovered through failures. `anonymous` distinguishes the two credential-less contexts by the `Admin` scope: the auth-off local **owner** (full trust) is *not* anonymous, the `Anonymous`-mode fallback caller *is*.

**401 responses** carry `WWW-Authenticate: Bearer` (RFC 7235 §3.1); all responses carry `Referrer-Policy: no-referrer`, so a token that rides a read GET's `?token=` query param (the browser `<img>`/`<audio>`/WS path, §1.1) is not leaked onward via the `Referer` header. `CorsLayer::permissive()` is intentional and load-bearing: hosted-mode (issue #74) has the web client on a *different origin* from the server it drives, so the API must accept cross-origin requests. Bearer tokens are not ambient credentials, so permissive CORS does not expose them to a drive-by page.

### 1.3 Credential storage — client vs server

Two stores, never mixed, per DESIGN_GUIDELINES §2 ("credentials live in the OS secret store, never in the library file") and PRODUCT_SPEC §5:

- **Client credentials (outbound, in the OS keychain).** When a client adds a federated source or connects with `--connect`, the credential it presents to *that* peer (a token, or OIDC refresh/client secret) is stored **per source** in the OS keychain via `keyring`, keyed by the source/peer identity. It is **never** written into the portable library SQLite file or any export. This is the `AuthConfig` carried by `ApiClient` and the federated `Source` ([01](01-architecture-and-crates.md) §4, [07](07-sources-and-federation.md)) — its concrete shape is owned by file 07; this file only fixes that it comes from `keyring` and is scoped per source.

- **Server secrets (inbound, in the server's own store).** Everything the *server* must verify against lives in the server's own config/flags/accounts store (§2, §5), which is **separate from the metadata DB** and never portable:
  - **Account credentials** are stored only as an **`argon2id`** hash (+ per-record salt; tuned cost params recorded alongside for future rehashing). Plaintext is never stored and never logged.
  - **API-key/token records** store a hash of the token secret (the plaintext is shown once at creation and never again), plus the token's label, scopes, visibility, and expiry (§1.4).
  - **OIDC** stores no user secret — it holds the issuer/client config and validates against the issuer; the account link is by verified subject (`sub`).

- **TLS.** Terminated by the server via **`rustls`** when the `tls` block is configured (cert + key paths, or ACME later — out of scope for v1, tracked in Open questions). Binding beyond localhost without TLS is an exposure the admin UI warns on (§3.4). TLS is transport, orthogonal to `AuthMode`, but the two combine in the same "am I safe to expose?" check.

### 1.4 Token / API-key records (the simple default)

A token is the simplest non-anonymous credential and the default private-instance path. Each is a server-side record:

```rust
pub struct TokenRecord {
    pub token_id: TokenId,
    pub label: String,          // human name shown in admin UI / CLI
    pub secret_hash: Hash,      // argon2id of the shown-once secret
    pub scopes: ScopeSet,       // what this token may do (subset of the mode's ceiling)
    pub visibility: VisibilityScope,
    pub created: Timestamp,
    pub expires: Option<Timestamp>,
    pub last_used: Option<Timestamp>,
}
```

Tokens are managed through the admin API (§5) and are independent of accounts: a token can exist with no account (a scoped service credential) or, when accounts are on, be issued *by* an account and inherit a subset of its ceiling. On use, the middleware verifies the presented secret against `secret_hash` and stamps `last_used`.

### 1.5 OIDC / OAuth2 (the extension path, design level)

OIDC is the open-standard extension over the token default, gated by the `auth-oidc` build feature and the `authentication = Oidc` runtime mode. Design-level flow (via `openidconnect`/`oauth2`):

- **Config (`OidcConfig`):** issuer URL (discovery document fetched for endpoints + JWKS), client id, client secret (server store), redirect URL, requested scopes, and a claim→account mapping rule (which claim is the stable subject, optional group→role mapping).
- **Web-client login:** Authorization Code + PKCE. The browser is redirected to the issuer; on callback the server exchanges the code, validates the ID token, resolves/creates the account link, and issues a **server session** (§4.4) — the browser thereafter carries the session cookie, not the raw JWT.
- **API/agent callers:** present an issuer-minted **bearer JWT** directly; the middleware validates it against the cached JWKS each request (with key rotation handled by JWKS refresh). No session is created for stateless callers.
- **Account mapping:** a verified `sub` maps to an `Account` (§4). Whether an unknown-but-valid subject is auto-provisioned (with a default role) or rejected is a configured policy on `OidcConfig`; v1 default is reject-unless-linked (see Open questions).

The seam is deliberately the same `AuthContext` regardless of mode, so adding OIDC changes only *how* the context is populated, not who consumes it (DESIGN_GUIDELINES §2: "keep the seam clean so open-standard identity slots in without a rewrite").

---

## 2. The feature-flag store

### 2.1 Placement

Flag state, token records, account records, and the audit log live in the **server's own config store** — a small SQLite database **beside, but separate from, the metadata DB** ([02](02-data-model-and-storage.md) owns that both files exist and where; this file owns the flags/accounts/audit schema within the server store). It is host configuration and identity, deliberately **outside** the portable library file and every export (PRODUCT_SPEC §5, ADR 0004 decision 4). A different library can be opened under the same server without moving its flags or accounts.

### 2.2 Schema sketch

One versioned flag row per capability (versioned so an edit is optimistic-concurrency-checked and auditable), plus the supporting tables:

```sql
-- server config store (NOT the metadata DB)

CREATE TABLE flags (
  key         TEXT PRIMARY KEY,     -- FlagKey, §3 (e.g. 'authentication')
  value       TEXT NOT NULL,        -- JSON: bool | enum | struct (mode config etc.)
  version     INTEGER NOT NULL,     -- bumped on every set; optimistic concurrency
  updated_at  INTEGER NOT NULL,
  updated_by  TEXT                  -- Identity that last set it (audit join key)
);

CREATE TABLE tokens (
  token_id    TEXT PRIMARY KEY, label TEXT NOT NULL, secret_hash TEXT NOT NULL,
  scopes      TEXT NOT NULL,        -- JSON ScopeSet
  visibility  TEXT NOT NULL,        -- JSON VisibilityScope
  created     INTEGER NOT NULL, expires INTEGER, last_used INTEGER
);

CREATE TABLE accounts (
  account_id  TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
  cred_hash   TEXT NOT NULL,        -- argon2id
  role        TEXT NOT NULL,        -- 'admin' | 'editor' | 'viewer'
  visibility  TEXT NOT NULL,        -- JSON VisibilityScope
  created     INTEGER NOT NULL, disabled INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE sessions (
  session_id  TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(account_id),
  issued      INTEGER NOT NULL, expires INTEGER NOT NULL, last_seen INTEGER
);

CREATE TABLE audit_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  at          INTEGER NOT NULL,
  actor       TEXT NOT NULL,        -- Identity (account/token/'config-file'/'bootstrap')
  action      TEXT NOT NULL,        -- 'flag.set' | 'account.create' | 'token.revoke' | ...
  target      TEXT,                 -- flag key / account id / token id
  detail      TEXT                  -- JSON: before/after for flag changes
);
```

The in-memory representation is a `FlagState` map loaded at startup and held behind an `RwLock` (or watch channel) so guards read it cheaply; a `set` writes the row (version-checked) *and* updates the in-memory copy atomically, then appends an audit entry.

### 2.3 Config-file ↔ admin-UI reconciliation

Two coequal control planes over **one** persisted state (ADR 0004 decision 2). Precedence and reconciliation:

- **The store is the source of truth at runtime.** The config file **seeds** it, it does not shadow it. On startup the loader reads the `serve` config ([09](09-server-and-web-client.md) owns the file format) and applies it as follows, per key:
  - **First run / key absent in store** → the config value is written into the store (seed), attributed to `updated_by = 'config-file'`.
  - **Key already in the store** → precedence is governed by one explicit setting, `config_authority`, so the behaviour is never accidental:
    - `config_authority = seed-only` (**default**): the store wins; config values for already-seeded keys are ignored at startup (a headless operator who wants the file to win uses `reconcile`). Admin-UI/CLI edits persist across restarts.
    - `config_authority = reconcile`: on startup (and on config-file change if watching is enabled) the file is re-applied, overwriting store values it names — the file is authoritative, the UI is a live-but-transient convenience. Suits reproducible/GitOps deploys.
- **Watching is opt-in.** Whether the config file is watched and re-applied live is itself a setting; default is read-once-at-startup (no watch), so an admin-UI edit is not silently reverted by a stale file. This is the "two writers, one state" race ADR 0004 flagged; the `config_authority` + watch settings are the defined resolution.
- **The admin API always writes the store** (§5). It never rewrites the config file — the file stays the operator's declarative artifact, the store stays the live truth. (Exporting the current store *as* a config file is a convenience command, not a two-way sync.)

Every write from either plane goes through the same `set` path (version check → store write → in-memory update → audit append), so both planes are genuinely equal and both are audited.

---

## 3. The v1 flag set

The authoritative v1 capabilities (PRODUCT_SPEC §6.11), keyed by a stable enum. Each flag names its **default** and whether a change applies **live** or needs a **restart**. Live means the middleware/router re-reads state or a route is mounted/unmounted in place; restart means the change is persisted immediately but only takes effect on the next `serve` start (labelled as such in the admin UI, DESIGN_GUIDELINES §3.6 — never silently deferred).

```rust
pub enum FlagKey {
    RemoteAccess,       // bind: Localhost | Address(SocketAddr)
    Authentication,     // AuthMode (§1.1)
    UserAccounts,       // bool; when true, forces Authentication >= Token
    McpServer,          // Off | ReadOnly | ReadWrite  (mounts/unmounts POST /mcp)
    NetworkWrites,      // bool; server read-only vs writes-enabled to the network
    InboundFederation,  // Off | On { auth: … }  (mounts/unmounts the federation endpoint)
    RemoteConnect,      // bool; accept GUI/CLI --connect backend sessions
    AnalysisWatch,      // which extractors run + whether sources are watched (§6.2)
}
```

| Flag | Default | Live / restart | Notes |
|------|---------|----------------|-------|
| **Remote access / bind** | `Localhost` | **Restart** | The listening socket is bound at startup; changing the bind address re-binds, which the router cannot do in place. Warned as exposure-increasing (§3.4). |
| **Authentication** | `Off` | **Live** | Middleware re-reads `AuthMode` per request; switching Off→Token/OIDC takes effect immediately. `Off` disallowed while accounts on. |
| **User accounts** | `Off` | **Live** (forces auth) | Turning on requires (and, if needed, flips) auth to ≥ `Token`; the account tables/bootstrap (§4) come into play at once. |
| **MCP agent server** | `Off` | **Live** | On/off **mounts/unmounts** `POST /mcp` and its tools ([11](11-mcp-server.md)); ReadOnly↔ReadWrite re-gates its write tools live. |
| **Network writes** | `Off` (read-only) | **Live** | Guards write scopes at request time; defaults read-only whenever bound beyond localhost, independent of transport. |
| **Inbound federation** | `Off` | **Live** | On/off mounts/unmounts the peer-facing federation endpoint ([07](07-sources-and-federation.md)). |
| **Remote client connect** | `Off` | **Live** | Gates whether `--connect` backend sessions are accepted; a per-request/handshake check. |
| **Analysis & watch** | conservative default | **Live** for watch toggles; **restart** if it changes linked runtimes | Enabling/disabling watching and extractor selection is live; anything needing a not-yet-loaded runtime is restart-scoped. |

**Off removes the surface** (ADR 0004 decision 3). "Off" is not "present but 403": the router **unmounts** the route/tools/endpoint. `McpServer = Off` removes the whole `POST /mcp` route and its tool registry; `InboundFederation = Off` removes the peer endpoint; `RemoteConnect = Off` refuses the connect handshake. This makes the safe default robust to auth misconfiguration — a removed route cannot be reached even if auth is wrong. Live mounts/unmounts are done by rebuilding the axum router from current `FlagState` and swapping it (or by route-level guards that fall through to `404` when the flag is off — [09](09-server-and-web-client.md) owns the mechanism); the semantic this file fixes is that off ⇒ **absent**, not dormant.

---

## 4. User accounts (opt-in)

Off by default (`UserAccounts = Off` ⇒ single-tenant, whatever the auth mode grants). When on, accounts are the server-side identity records of §2.2, layered on the existing auth seam (ADR 0004 decision 4). Identity records live in the server store, **never** in the library file or exports (PRODUCT_SPEC §5).

### 4.1 Roles

Three fixed roles for v1 (custom roles are an Open question, per PRODUCT_SPEC §10):

- **admin** — manage feature flags, sources, tokens, and other accounts (the admin API, §5), plus everything editor can do.
- **editor** — everything viewer can do **plus writes** (tag / set_license / convert / scan / export). The `NetworkWrites` flag is the ceiling on **implicit trust only** (the auth-off owner posture, anonymous callers): a *verified* credential's scopes alone decide — front-door auth, one gate at the connection, permissions after it.
- **viewer** — read-only browse, search, similarity, preview.

### 4.2 Role → scope mapping

Roles are sugar over a `ScopeSet`; guards check scopes, not roles, so the mapping is the single place roles gain meaning:

```rust
pub enum Scope { Read, Write, Admin, McpUse, Federate }

pub type ScopeSet = /* bitflags/set over Scope */;

fn scopes_for(role: Role) -> ScopeSet {
    match role {
        Role::Viewer => Read | McpUse,               // read + use read tools/federation
        Role::Editor => Read | Write | McpUse | Federate,
        Role::Admin  => Read | Write | Admin | McpUse | Federate,
    }
}

fn anon_scopes() -> ScopeSet { Read | McpUse }        // what Anonymous mode grants
```

Effective granted scopes = `scopes_for(role)` intersected with any narrowing on the credential (a token issued below its account's ceiling) and gated by the relevant flag at the point of use:

- `Scope::Write` present ⇒ a write handler proceeds for a **verified credential** (identity resolved from a token); an **implicit-trust** caller (auth off / anonymous, no credential) additionally needs `NetworkWrites = true` on a non-loopback bind. Missing scope ⇒ `403`; implicit trust without the flag ⇒ `403 disabled`.
- `Scope::McpUse` is necessary but not sufficient for MCP — the `McpServer` flag must be on (else the route is absent), and its ReadOnly/ReadWrite setting further gates MCP write tools ([11](11-mcp-server.md)).
- `Scope::Admin` gates the entire admin API (§5); once auth is on, the admin surface is **never** reachable anonymously (ADR 0004 negative note).

### 4.3 Visibility scope

An optional per-account (and per-token) **visibility scope** caps which sources and collections the identity can see — in *every* client, because all clients sit on the one auth surface. It is the **ceiling** on what that identity reaches anywhere; it intersects with, and can only narrow, what a query would otherwise return.

```rust
pub enum VisibilityScope {
    All,                                   // no restriction (default)
    Restricted { sources: Vec<SourceId>, collections: Vec<CollectionId> },
}
```

For v1 scoping **bottoms out at source and collection level** (not per-asset — an Open question, PRODUCT_SPEC §10). The library service ([03](03-library-service-and-api.md)) receives the `AuthContext` and applies the visibility filter as a query predicate; federated fan-out ([07](07-sources-and-federation.md)) restricts which peers/sources are consulted accordingly. Enforcement is in the engine's query path, not bolted onto each handler, so it cannot be forgotten per endpoint.

### 4.4 Bootstrap, sessions & token lifetime

- **First-admin bootstrap.** When `UserAccounts` is turned on with no `admin` account present, the server enters a one-time bootstrap: it emits a single-use bootstrap token (printed to the server log / returned by the enabling admin-API call) that the first admin presents to create the initial admin account. The bootstrap window closes once one admin exists. This is auditable (`action = 'account.bootstrap'`) and recorded with actor `'bootstrap'`. Recovery (a lost sole admin) is an Open question (PRODUCT_SPEC §10); the design keeps the config-file plane able to re-open bootstrap as the escape hatch.
- **Sessions** (web-client login) are server-side records (§2.2 `sessions`) with an absolute expiry and a sliding `last_seen`; the browser holds an opaque session cookie, not credentials. Logout and admin-initiated revocation delete the row.
- **Token/session lifetime** defaults: sessions expire after an inactivity window (configurable) and an absolute max; API tokens are long-lived with an optional `expires`. Exact defaults are an Open question (PRODUCT_SPEC §10); the records carry the fields so the policy is data, not code.

### 4.5 Audit log

Every flag change is logged with **who and when** (PRODUCT_SPEC §6.11: "auditable & reversible"), via the `audit_log` table (§2.2). The `set` path appends one entry per change with `actor` (the `Identity`, or `'config-file'`/`'bootstrap'` for non-request writes), `action`, `target`, and a `detail` JSON carrying before/after for flag changes. Account and token lifecycle events are logged the same way. The log is append-only from the API's perspective (no update/delete route) and readable by admins through the admin API (§5).

---

## 5. The admin API

One admin API is the single source of truth that both the web admin UI ([09](09-server-and-web-client.md)) and the CLI ([13](13-cli.md)) drive — parity is achieved by *both being clients of these routes*, not by duplicated logic (ADR 0004 decision 2; DESIGN_GUIDELINES §3.6 "parity with config/CLI"). It is mounted under an admin path (e.g. `/admin/*`) by file 09, guarded by `Scope::Admin`, and — once auth is on — never anonymous. Every mutating call goes through the audited `set` path (§2.3, §4.5).

```
# Feature flags
GET    /admin/flags                 → full FlagState (values, versions, live/restart, defaults)
GET    /admin/flags/{key}           → one flag
PUT    /admin/flags/{key}           → set a flag (body: value + expected version for
                                       optimistic concurrency); returns applied state +
                                       whether a restart is pending. Exposure-increasing
                                       changes require an explicit `confirm` field (the
                                       machine form of the UI's warn-and-confirm, §3.4).

# Accounts (present only when UserAccounts is on)
GET    /admin/accounts              → list (no hashes)
POST   /admin/accounts              → create (name, initial credential, role, visibility)
PUT    /admin/accounts/{id}         → update role / visibility / disabled
DELETE /admin/accounts/{id}         → remove (cannot remove the last admin)
POST   /admin/accounts/{id}/password→ set/reset credential (argon2id rehash)
POST   /admin/bootstrap             → redeem the first-admin bootstrap token (§4.4)

# Tokens / API keys
GET    /admin/tokens                → list (labels, scopes, expiry; never secrets)
POST   /admin/tokens                → issue (label, scopes⊆ceiling, visibility, expiry);
                                       returns the plaintext secret ONCE
DELETE /admin/tokens/{id}           → revoke

# Auth mode config (subset of flags, surfaced for the mode picker)
GET/PUT /admin/auth                 → read/set AuthMode incl. OidcConfig (secret write-only)

# Audit
GET    /admin/audit?since=…&action=…&target=… → paged audit entries

# Status (drives the admin UI's live-vs-restart + exposure indicators)
GET    /admin/status                → effective bind, mode, accounts on/off, TLS on/off,
                                       pending-restart flags, exposure warnings
```

Design constraints the routes encode:

- **Optimistic concurrency** on flag writes via `version` (§2.2) so two admins racing the same toggle is detected, not silently last-write-wins.
- **Confirm-on-exposure** is enforced server-side (the `confirm` field), so the safety gate is not merely a UI courtesy — the CLI must pass it too.
- **Secrets are write-only / show-once**: token secrets and OIDC client secrets are never returned by a `GET`.
- **The same reconciliation path** (§2.3) backs these routes and the config-file loader, so the CLI/UI and a headless config deploy converge on one persisted state.

---

## Open questions

> **Resolved 2026-07-06 in [ADR 0009 §2–4](../adr/0009-v1-scope-decisions.md).** Flags: `server.db`
> table, per-flag `config_authority` (default `seed-only`, no auto-revert), live-by-default + a
> frozen restart-only set. Accounts: fixed roles, source/collection visibility, config-bootstrap
> recovery, session 14d/90d (custom roles + per-asset post-v1). Auth: rate-limit + lockout + CSRF,
> static `rustls` (ACME post-v1), **non-localhost bind without TLS refused unless `--insecure`.**
> Kept below as rationale.

Carried from PRODUCT_SPEC §10; this file is where their mechanics land as they resolve.

- **Feature-flag store & lifecycle.** Confirmed here as a versioned table in the server's own store seeded by the config file, with `config_authority` (seed-only vs reconcile) + opt-in watch as the two-writers resolution, and a per-flag live/restart table (§3). Still open: whether the config file should ever be watched-and-reverted by default, and the exact set of restart-only flags once the router's live-remount mechanism ([09](09-server-and-web-client.md)) is built and measured.
- **User-accounts scope for v1.** Fixed `admin`/`editor`/`viewer` roles and source/collection-level visibility are specified (§4). Still open: fixed vs custom roles beyond v1, whether visibility should ever bottom out per-asset, account recovery for a lost sole admin (beyond the config-file bootstrap escape hatch), and concrete session/token lifetime defaults.
- **Server auth / security model & TLS.** Token vs account posture and OIDC mapping policy are designed (§1); still open: how far the server hardens for exposure beyond localhost/LAN in v1 (rate limiting, lockout, CSRF for the session cookie), TLS provisioning beyond static cert/key (ACME), and whether binding beyond localhost without TLS should be refused rather than merely warned.

---

See also: [03](03-library-service-and-api.md) · [07](07-sources-and-federation.md) · [09](09-server-and-web-client.md) · [11](11-mcp-server.md) · [13](13-cli.md) · [ADR 0004](../adr/0004-feature-flags-admin.md) · [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.7 / §6.11 / §5 · [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §2 / §3.6
