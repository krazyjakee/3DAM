# 10 — Auth, accounts & feature flags

Status: **Draft v0.2** · Scope: the server-side auth layer (off / anonymous / token / OIDC-OAuth2) that turns a request into an **auth context**, credential storage (client keychain vs server secret store), the runtime **feature-flag** store and its config-file ↔ admin-UI reconciliation and live-vs-restart lifecycle, opt-in **user accounts** (roles → scopes, visibility scope, groups & sharing, first-run claim, audit), and the **admin API** that the web UI and CLI both drive.

> **Amended 2026-07-30 (issue #42, phase 6).** Two deliberate changes from v0.1, marked inline:
> §4.4's single-use bootstrap token is **replaced by the first-run claim** (localhost-gated open
> signup; the token flow survives as the off-box redemption path), and §4.3 gains **groups and
> shares** — positive per-resource grants that compose with the per-identity visibility ceiling.
> Groups are share *targets*, not roles; the ADR 0009 custom-roles freeze is not affected.

This file is the low-level design for the security and administration layer that wraps every `serve`-mode surface. It sits in the **`3dam-server`** crate ([01](01-architecture-and-crates.md) §1, key deps `rustls`/`oauth2`/`argon2`/`keyring`) and implements the mechanics decided in [ADR 0004](../adr/0004-feature-flags-admin.md) and specified at the product level in [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.7 (federation & authentication), §6.11 (server administration), and §5 (the server-side settings/flags/accounts records that live *outside* the library file). It does not restate that rationale or re-decide the ADR — it fills in the types, schemas, and routes.

**Borders with siblings** (do not write outside this scope):

- [03](03-library-service-and-api.md) owns the `LibraryService` trait, its DTOs, and the HTTP/WS API surface. This file defines the auth *middleware* that wraps that surface and the `AuthContext` it injects; it does not define the library methods themselves.
- [09](09-server-and-web-client.md) hosts the `serve` config file, the web admin UI, and the routing/mount plumbing. This file defines the **flag/account semantics** and the **admin API** the config loader and admin UI drive; file 09 wires the router and renders the UI.
- [07](07-sources-and-federation.md) consumes the `AuthContext` for inbound federation and carries the **client** `AuthConfig` outbound to peers.
- [11](11-mcp-server.md) sits behind this auth layer; its `POST /mcp` route is mounted or unmounted by the MCP flag, and its tools read the same `AuthContext` and scopes.
- [13](13-cli.md) drives the same admin API as the web UI (one source of truth) for headless administration.

Compile-time note: the runtime flags here presuppose the capability was **compiled in** — the `server`/`mcp` features gate those surfaces ([01](01-architecture-and-crates.md) §6). A runtime flag can only enable what the build carries.

> **The `auth-oidc` Cargo feature was not adopted** *(decided 2026-08-01, issue #41)*. This file previously gated `oauth2`/`openidconnect` behind a build feature. In implementation that lost to CLAUDE.md golden rule 4, which names *auth* among the capabilities that are runtime flags in `server.db` **and not Cargo features** (ADR 0004), and to the shipping reality that 3DAM is one binary: a build-time gate means the released artifact either carries OIDC or cannot be given it, which is the situation [ADR 0015](../adr/0015-video-decode-backend.md) deliberately avoided for video decode. `openidconnect` is therefore an unconditional dependency of `3dam-server`, and [`FlagKey::Oidc`](#) is the only switch. The cost is a larger dependency tree for deployments that never enable it; the benefit is that enabling it is a runtime decision an operator can actually make.

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
}
```

> **No `Oidc` variant** *(amended 2026-08-01, issue #41)*. This enum originally carried
> `Oidc(OidcConfig)`. Implementing §1.5 showed that to be the wrong shape: an OIDC login mints an
> **ordinary server session** (§4.4), so it composes with password login and bearer tokens rather
> than displacing them — and a mode is by definition the one answer to "what does this instance
> demand of an unauthenticated caller?". As a variant, turning OIDC on would have switched password
> login *off*. It is therefore `FlagKey::Oidc`, a capability alongside the mode, gated exactly like
> uploads and accounts: off ⇒ the `/api/v1/auth/oidc` surface 404s.

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
   - **An issuer-minted bearer JWT** (the stateless API/agent leg of §1.5) → validate against the configured issuer's bounded JWKS cache (signature, exact `iss`/`aud`, `exp`/`nbf`, and an RS256 algorithm allowlist), then map `(issuer, subject)` to an account (§4). A native `dam_…` key is classified first and never causes issuer traffic; JWT-shaped non-native credentials take this arm. Unknown `kid` refreshes are singleflight and cooldown-bounded so rotation is prompt without turning attacker-controlled key ids into an outbound request flood.
   - **Session** (web client, post-login) → validate the session token against the session store (§4.4) and load the account.
3. **No credential, mode `Anonymous`** → the anonymous identity with `anon_scopes()`.
4. **No credential, mode requires one** (accounts on, or a scope beyond anon requested) → `401`.

The context is computed once per request; scope checks downstream are pure set-membership tests against `ScopeSet`, never a re-derivation. A handler that needs a capability calls a small guard (`ctx.require(Scope::Write)?` → `403` on miss).

**`GET /api/v1/whoami`** reflects the resolved context back to the caller — `{ identity, scopes, anonymous }` — so a front-end can shape its UI to the granted scopes (disable a write control rather than let the action 403). It requires no scope of its own: it reports whatever the presented credential resolves to, and under `Token` mode with no credential it still `401`s (the client's cue to render a login gate). This is the "one gate, then permissions decide" model made legible to the client instead of discovered through failures. `anonymous` distinguishes the two credential-less contexts by the `Admin` scope: the auth-off local **owner** (full trust) is *not* anonymous, the `Anonymous`-mode fallback caller *is*.

**401 responses** carry `WWW-Authenticate: Bearer` (RFC 7235 §3.1). Long-lived bearers are header-only: query bearer authentication is rejected. Browser thumbnails, images, and decoded meshes use authenticated fetches and local blob URLs. Range-streamed audio/video use a five-minute, exact-path-and-query media ticket; each Range request re-verifies the hidden parent credential, so API-key revocation, session revocation, and JWT expiry/identity changes take effect without trusting the context captured when the ticket was minted. WebSockets use a separate 30-second, one-use, WS-only ticket and periodically re-verify the hidden parent credential. Neither ticket can call JSON/admin APIs or name another asset. Request tracing records `uri.path()` only, and every response retains `Referrer-Policy: no-referrer` as defense in depth. `CorsLayer::permissive()` is intentional and load-bearing: hosted-mode (issue #74) has the web client on a *different origin* from the server it drives, so the API must accept cross-origin requests. Bearer tokens are not ambient credentials, so permissive CORS does not expose them to a drive-by page.

### 1.3 Credential storage — client vs server

Two stores, never mixed, per DESIGN_GUIDELINES §2 ("credentials live in the OS secret store, never in the library file") and PRODUCT_SPEC §5:

- **Client credentials (outbound, in the OS keychain).** When a native client adds a federated source or connects with `--connect`, the credential it presents to *that* peer (a token, or OIDC refresh/client secret) is stored **per source/origin+mount** in the OS keychain via `keyring`. SFTP/SMB passwords and SFTP key paths/passphrases use the same per-source indirection. It is **never** written into the portable library SQLite file, web storage, a persisted query cache, diagnostics, or any export. `library.db` holds only an opaque `auth_ref`; source operations hydrate a short-lived connection immediately before use. Headless Unix deployments without a secret-service session may explicitly set `3DAM_SOURCE_SECRET_DIR` to a non-portable, owner-only host directory (0700 directory, 0600 files, outside the library data path); this plaintext-at-rest fallback trusts the service-account/root boundary and is documented operationally in `docs/DEPLOYMENT.md`. A locked/missing backend fails explicitly and never falls back silently. The Tauri page cannot retrieve the secret: native initialization captures it with pristine prototype accessors and attaches it only to matching fetches; `worker-src 'none'` plus an incognito renderer partition prevent service-worker interception. Browser-only token entry has the explicitly weaker posture of `sessionStorage` (one tab/session, cleared by Sign out or tab close); durable token fields inside `localStorage["3dam.server"]` from older releases are erased and users receive a one-time recovery prompt while the non-secret base URL is retained.

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

OIDC is the open-standard extension over the token default, gated by the **`oidc` runtime flag**
(which additionally requires `user_accounts` — a verified subject resolves to an account or to
nothing). Implemented in `3dam-server` via `openidconnect`; both the web-client login leg and the
stateless bearer-JWT leg are live. Flow:

- **Config (`OidcConfig`):** issuer URL (discovery document fetched for endpoints + JWKS), client id, client secret (server store), redirect URL, requested scopes, and a claim→account mapping rule (which claim is the stable subject, optional group→role mapping).
- **Web-client login:** Authorization Code + PKCE. The browser is redirected to the issuer; on callback the server exchanges the code, validates the ID token, resolves/creates the account link, and issues a **server session** (§4.4) — the browser thereafter carries the session cookie, not the raw JWT.
- **API/agent callers:** present an issuer-minted **bearer JWT** directly; the middleware validates signature, issuer, audience, expiry/not-before, and RS256 against a single-provider cached JWKS. A cache miss for an unknown `kid` coalesces concurrent callers into one refresh and imposes a retry cooldown, while a provider config change replaces the sole cache entry. No session is created for stateless callers. The verified account then uses the identical role→scope and share→visibility resolution as a session.
- **Account mapping:** a verified subject maps to an `Account` (§4). Because `Linked` is the default, the **link routes are load-bearing, not a convenience** — without them a default-configured provider is one nobody can ever sign in through. Whether an unknown-but-valid subject is auto-provisioned (with a default role) or rejected is a configured policy on `OidcConfig`; v1 default is reject-unless-linked. Provisioning **refuses** rather than merges when the derived username collides with an existing local account — otherwise anyone who can get a chosen `preferred_username` out of the issuer could take over a local one.
- **The identity key is `(issuer, subject)`, not `subject`.** `sub` is only unique *within* an issuer, so a bare-subject key would let a second configured provider mint a subject that collides with a linked one and inherit that account. Stored in its own `oidc_identity` table, introduced by the ordered `server.db` V3 migration; all later table, index, constraint, or column changes append another transactional migration rather than changing an already-shipped step.
- **In-flight state is server-side.** `state`, `nonce` and the PKCE verifier live in an `oidc_login` row, deleted as it is read (`DELETE … RETURNING`), so a captured callback URL cannot be replayed; rows expire after ten minutes. The verifier in particular must never reach the browser — PKCE's whole value is that a stolen code is useless without a secret the browser never saw. The row count is capped, because `/start` is necessarily unauthenticated and therefore an unauthenticated write.
- **`state` is bound to the browser that started the login.** Single-use is *not* sufficient on its own: it does not stop **login CSRF**, where an attacker completes an honest login as themselves and hands the victim the resulting callback URL — valid in every respect, never redeemed — so the victim is silently signed in *as the attacker* and works inside the attacker's library. `/start` therefore sets a short-lived `HttpOnly` `dam_oidc` cookie and stores only its blake3 hash; the callback is refused (and audited) unless the browser presents a cookie matching it, compared in constant time. An attacker cannot set that cookie on this origin.
- **`return_to` is a rooted local path, never a URL.** A login link a stranger sends must not choose where you land; `//host` is refused along with schemes, backslashes and control characters.

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

### 4.3 Visibility scope, groups & sharing *(amended 2026-07-30, issue #42)*

Two mechanisms compose into what an identity can reach, and the composition rule is the crux:

- the **visibility ceiling** — a per-identity narrowing filter (this section's original concept):
  it can only *shrink* the reachable set, never widen it;
- **shares** — positive grants hanging off a *resource* (a source or a collection), targeted at an
  individual **account** or at a named **group**.

```rust
pub enum Visibility {
    Full,                                   // no restriction (admins, tokens, the local owner)
    Restricted(VisibilityScope),
}
pub struct VisibilityScope {                // the reachable set of a restricted identity
    sources: BTreeSet<SourceId>,            // read-reachable
    collections: BTreeSet<CollectionId>,
    write_sources: BTreeSet<SourceId>,      // additionally write-granted
    write_collections: BTreeSet<CollectionId>,
}
```

**Groups** are flat (no nesting — resolution stays a set union, not graph traversal) and
**orthogonal to role**: an account has exactly one role (what it may *do*) and any number of group
memberships (what it may *reach*). Groups are not custom roles; the ADR 0009 custom-roles freeze
does not apply to them.

**Resolution rules** (effective reachable set for an identity, in order):

1. **Admins bypass sharing entirely** (`Full`) — otherwise an admin could lose access to the
   library they administer, and share management would need its own recursive share.
2. **Union the grants**: direct account shares ∪ shares to every group the account belongs to.
   The most permissive access wins per resource (`write` beats `read`).
3. **Intersect with any account/token ceiling** — grants never widen a ceiling.
4. **`write` access still requires `Scope::Write`.** The share controls *which* resources; the
   role controls *whether* the identity may write at all. Two independent gates, both must pass —
   guards keep checking scopes, never roles.
5. **Collection ⊅ source.** A shared collection grants its member assets only, not their whole
   source. A shared source grants its assets and any collection view over them. A shared *smart*
   folder grants the folder itself, but its results stay intersected with the identity's
   source/manual-collection grants — a saved query is never a widening instrument.

For v1 scoping **bottoms out at source and collection level** (not per-asset — an Open question, PRODUCT_SPEC §10). The library service ([03](03-library-service-and-api.md)) receives the `AuthContext` and applies the visibility filter as a query predicate; federated fan-out ([07](07-sources-and-federation.md)) restricts which peers/sources are consulted accordingly. Enforcement is in the engine's query path, not bolted onto each handler, so it cannot be forgotten per endpoint — and it covers the whole leak surface: search, similarity (filtered at the candidate set, not post-top-k), dedup groups (re-formed after filtering; a group of one is not a duplicate), stats/tag aggregates, folder trees, thumbnails/content by id, exports, jobs/events, and MCP (same engine, same predicate).

**Smart and federated grants (implemented by issue #127).** Sharing a smart folder makes its record
reachable but does not add its live matches to the ceiling; search, counts and manifest export run
the saved query only after intersecting it with independently shared sources and manual
collections. A read share on a federated source selects only that registered peer for fan-out and
for cross-peer similarity, preserves the peer credential/deadline/partial-warning/embedding-space
gates, and re-attributes returned rows to the local federated source id. Detail and preview reads by
id must carry that source id (as returned by search); restricted callers never use the
unrestricted, hintless legacy-bookmark recovery round, and revocation therefore also gates an
already-cached preview. Federated sources are remote-owned and always read-only: the admin API
rejects a `write` share rather than recording authority the engine cannot exercise.

Two federation presentation limits remain explicit. A registered peer represents its whole remote
catalog as one local source, so the local folder-tree endpoint cannot enumerate the peer's internal
source roots (folder facets in a federated query still work). The peer's own background jobs and
event stream are not relayed; local jobs such as manifest export are attributed to the federated
source and obey the ordinary job/event ceiling.

**The cross-database seam.** Identity (accounts, groups, shares) lives in `server.db`; sources and collections live in `library.db`. A share's `resource_id` is therefore a **soft reference** — no FK can enforce it. The layering resolution: shares are resolved **at auth time, in the server**, into a finished `Visibility` value carried on `AuthContext`; the engine never learns what an account or group is. Deleting a source/collection garbage-collects its share rows; ids are UUIDs and never recycled, so a missed GC is clutter, never a grant to a future resource. A share change takes effect on the next request; long-lived subscriptions (the WS stream) watch a generation counter and re-resolve.

**Restricted identities and library-wide operations.** Operations that inherently span the catalog — source add/remove, scans, analysis, blocklist edits, collection creation — require `Full` visibility; the per-resource grant model has no meaningful subset semantics for them in v1.

### 4.4 First-run claim, sessions & token lifetime *(amended 2026-07-30, issue #42)*

- **First account becomes admin (the claim).** *(Replaces v0.1's single-use bootstrap token as the
  primary flow.)* When `UserAccounts` is on and **zero accounts exist**, the instance is
  **unclaimed**: the first account created through `POST /api/v1/auth/claim` is made `admin` and
  closes the window permanently. The first-run race (the Jellyfin/Grafana land-grab CVE class) is
  mitigated three ways, together:
  - **Localhost-only claim by default** — acceptance is bound to a loopback **peer address** (never
    the bind address, which is loopback for every visitor behind a same-host reverse proxy), and is
    refused outright for any request carrying `X-Forwarded-For` / `X-Real-IP` / `Forwarded`, or when
    `[accounts] require_claim_token = true`. A remote request to an unclaimed instance is refused,
    not served a signup form. See [ADR 0014](../adr/0014-first-run-claim.md).
  - **Off-box claim = token redemption** — the bootstrap owner token (minted whenever a
    credentialed gate goes up with no admin credential) presented as an Admin bearer authorises a
    claim from anywhere; the v0.1 token flow survives as exactly this path.
  - **Loud unclaimed state** — the server logs a recurring warning while unclaimed and
    `/admin/api/status` reports `unclaimed: true`; an exposed unclaimed instance is never silent.

  The claim is auditable (`action = 'account.claim'`) and **atomic**: the unclaimed check and the
  account insert share one `BEGIN IMMEDIATE` transaction, so two racing claims with different
  usernames cannot both win. Recovery for a lost sole admin stays the **config-file escape hatch**
  (ADR 0009 §3): `[accounts] reopen_claim = true` re-opens the window for one boot — existing
  accounts keep working, so the client keeps the login screen primary and offers the claim form as a
  secondary path. Turning `UserAccounts` on raises the **effective** auth mode to at least `Token`
  (accounts and unauthenticated owner trust never coexist); `Anonymous` is preserved as
  public-read + login-to-elevate.
- **Sessions** (web-client login) are server-side records (§2.2 `sessions`) with an absolute expiry and a sliding `last_seen`; the browser holds an opaque session cookie (`HttpOnly`, **`SameSite=Lax`** — amended 2026-08-01 for issue #41: the OIDC callback is a cross-site navigation and `Strict` withholds the cookie on exactly that, so a correct login would land looking signed out; `Lax` still withholds it from cross-site POST/PUT/DELETE, and writes are gated on the CSRF token regardless), not credentials, plus a double-submit CSRF token echoed in `x-dam-csrf` on cookie-authenticated writes (ADR 0009 §4). Logout and admin-initiated revocation delete the row. Account lockout: 10 failed logins / 15-minute window per username.
- **Token/session lifetime** (resolved, ADR 0009 §3): sessions expire after **14 days of inactivity** with a **90-day absolute ceiling**; API tokens are long-lived with an optional `expires`. Tokens remain independent credentials at or below their issuing authority's ceiling — never above.

### 4.5 Audit log

Every flag change is logged with **who and when** (PRODUCT_SPEC §6.11: "auditable & reversible"), via the `audit_log` table (§2.2). The `set` path appends one entry per change with a credential-safe `actor` prefix (`token:<label>`, `account:<username>`, or `oidc-bearer:<username>`; never a secret, JWT, issuer subject, or session id), `action`, `target`, and a `detail` JSON carrying before/after for flag changes. Non-request writes use actors such as `config-file`/`bootstrap`. Account and token lifecycle events are logged the same way. The log is append-only from the API's perspective (no update/delete route) and readable by admins through the admin API (§5).

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

# Accounts (present only when UserAccounts is on; off ⇒ 404)
GET    /admin/accounts              → list (no hashes)
POST   /admin/accounts              → create (name, initial credential, role)
PUT    /admin/accounts/{id}         → update role / display name / disabled / password
                                       (password change revokes the account's sessions;
                                        the last enabled admin cannot be demoted/disabled)
DELETE /admin/accounts/{id}         → remove (cannot remove the last admin)
DELETE /admin/accounts/{id}/sessions→ revoke all of an account's sessions

# Groups & shares (issue #42; admin-only writes in v1 — editor self-sharing needs an
# ownership concept the frozen scope doesn't have; revisit post-v1)
GET    /admin/groups                → list, with member account ids
POST   /admin/groups                → create (name)
DELETE /admin/groups/{id}           → remove (memberships + its shares cascade)
PUT    /admin/groups/{id}/members   → replace the membership set
GET    /admin/shares                → list grants
POST   /admin/shares                → grant (source|collection, account xor group, read|write)
DELETE /admin/shares/{id}           → revoke (takes effect on the next request)

# First-run claim (public route, not admin — gated by a direct loopback peer / bootstrap-token bearer, §4.4)
POST   /api/v1/auth/claim           → create the first admin account while unclaimed

# Tokens / API keys
GET    /admin/tokens                → list (labels, scopes, expiry; never secrets)
POST   /admin/tokens                → issue (label, scopes⊆ceiling, visibility, expiry);
                                       returns the plaintext secret ONCE
DELETE /admin/tokens/{id}           → revoke

# Auth mode config (subset of flags, surfaced for the mode picker)
GET/PUT /admin/api/oidc             → read/set the OIDC provider config (secret write-only)
GET/POST /admin/api/oidc/identities → list / link a provider subject to a local account
DELETE  /admin/api/oidc/identities/{subject} → unlink (the account itself is untouched)

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
