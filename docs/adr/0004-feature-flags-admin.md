# ADR 0004 — Runtime feature flags & server administration (config + web UI)

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §2, §5, §6.7, §6.8, §6.10, §6.11,
§7, [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §2, §3.6, [0003 — MCP server](0003-mcp-server.md)

## Context

`3dam serve` is meant to run anywhere from a personal laptop to an exposed multi-user host on a
NAS. Those are very different security postures, but it is **one binary** with one server
surface (HTTP/WS API + web client + MCP endpoint, all on one port — §6.8, [ADR 0003](0003-mcp-server.md)).
Two requirements pull on that surface:

1. **The operator must be able to switch capabilities on and off** — explicitly, the MCP agent
   server must be turn-off-able, and authentication and **user accounts** must be configurable —
   from a **beautifully presented web UI**, not just a hand-edited config file.
2. **Safe by default.** A server started with no thought should expose nothing sensitive:
   localhost-only, no auth, no accounts, no agent access, no inbound federation, read-only to the
   network. Capability is opt-in, per the local-first / no-account principles (§2).

There is a tension to resolve, not dodge: the spec's Privacy and Non-goals say **"no account"**.
That means *no mandatory account with a 3DAM cloud service* — it must not come to mean "you may
never run accounts on your own server." Self-hosted, opt-in, off-by-default accounts are
compatible with the principle; a mandatory or cloud account is not.

## Decision

**1. Capabilities are runtime feature flags on one shared surface — off by default.** Remote
access, authentication, user accounts, the MCP server, network writes, inbound federation, and
remote client connect are named flags, not separate builds or daemons. A fresh server is locked
down; each flag is switched on deliberately.

**2. Two coequal control planes over one persisted state.** The config file **seeds** the flag
state declaratively (for headless / reproducible deploys); a **Settings / Administration** area
in the web client **edits the same state** at runtime; and the API/CLI can read and set it too.
There is one source of truth (a versioned record in the server's own config store, §5), so no
control plane is authoritative-by-accident and a headless operator never needs the browser.

**3. Turning a flag off removes its surface.** "Off" unmounts the route / tools / endpoint (e.g.
the whole `POST /mcp` route, §6.10) rather than leaving it dormant behind auth — so disabling a
capability genuinely reduces attack surface, and does not rely on auth being configured correctly
to be safe.

**4. User accounts are opt-in, server-side, and layered on the existing auth seam.** Off by
default. When on, accounts are identity records (name, hashed credentials, role, optional
visibility scope) in the **server's own store — never in the portable library file or exports**.
Roles are `admin` / `editor` / `viewer`; scope caps what an account sees in *every* client,
because everything already sits on one auth surface (§6.7, [ADR 0003](0003-mcp-server.md)). This
reconciles the "no account" principle: no cloud account, accounts are a thing you enable on your
own host.

**5. The admin UI is a first-class surface, not a raw config dump.** Grouped toggle cards,
progressive disclosure of sub-options, a warning + confirm before any exposure-increasing toggle,
and explicit live-vs-restart labelling (DESIGN_GUIDELINES §3.6). Visuals follow the dark-first,
low-chrome language (§4).

## Consequences

**Positive**

- One binary spans "zero-config personal library" and "hardened multi-user host" with no rebuild;
  the posture is data, not code.
- Because every flag sits on the one `LibraryService` + auth surface, enabling auth/accounts gates
  the web client, API, MCP, and inbound federation *together* — no per-subsystem policy to keep in
  sync.
- Config-file / CLI / web-UI parity means CI and headless operators are first-class, and the web
  UI is a convenience over one source of truth.
- "Off removes the surface" makes the safe default robust to misconfiguration, not dependent on it.

**Negative / risks**

- **Two writers, one state.** Config-file edits and admin-UI edits can race. Needs a defined
  reconciliation (which wins on conflict, whether the file is watched and re-applied) and clear
  live-vs-restart semantics — tracked in §10.
- **Accounts are new scope.** Even minimal role/scope adds first-admin bootstrap, credential
  storage/hashing, session/token lifetime, and recovery paths. Mitigated by shipping accounts
  opt-in and *after* the flag framework + token auth (spec §9 step 6), starting from fixed roles.
- An admin UI that can expose the server is itself sensitive: it must be admin-scoped and, once
  auth is on, never reachable anonymously.

**Follow-ups**

- Decide the flag persistence + reconciliation model and per-flag live/restart classification (§10).
- Scope the v1 accounts model: fixed vs custom roles, where visibility scoping bottoms out
  (source/collection vs per-asset), bootstrap and recovery (§10).
- Confirm the admin surface's warning/confirm flows for exposure-increasing toggles during the
  web-client build (web-first phasing, PRODUCT_SPEC §9).
