# ADR 0014 — First-run claim: the first account becomes admin, gated to loopback

Status: **Accepted** · Date: 2026-07-30 · Deciders: 3DAM core
Supersedes: the bootstrap-token-first flow of [tech-spec 10 §4.4 v0.1](../tech-spec/10-auth-accounts-and-flags.md) · Related: [ADR 0009 §3–§4](0009-v1-scope-decisions.md) (frozen accounts scope), [ADR 0004](0004-feature-flags-admin.md) (off ⇒ absent), issue [#42](https://github.com/krazyjakee/3DAM/issues/42) (full user accounts epic), issue [#41](https://github.com/krazyjakee/3DAM/issues/41) (OIDC)

## Context

Phase 6 brings full user accounts (`UserAccounts` flag, off by default). Something has to mint the
**first admin**. Tech-spec 10 v0.1 specified a single-use **bootstrap token** printed to the server
log, redeemed to create the initial admin account. That flow is secure but hostile to the primary
persona — a solo dev flipping accounts on from the desktop app or a localhost `serve` — who now has
to go fish a secret out of a log file to sign up on their own machine.

The friendlier pattern (Jellyfin, Grafana, Portainer, …) is the **first-run claim**: while zero
accounts exist the instance is *unclaimed*, and the first signup becomes admin. The equally
well-known failure mode is the same products' CVE class: an unclaimed instance reachable over the
network is a **land-grab** — whoever hits it first owns it.

## Decision

Adopt the first-run claim, with the land-grab closed off by three mitigations that ship together:

1. **Localhost-only claim by default.** `POST /api/v1/auth/claim` accepts only when the *peer
   address* is loopback (or the bind itself is loopback-only). A remote request to an unclaimed
   instance is refused (`403`), not served a signup form. The claim window closes permanently on
   the first successful claim; the race between two simultaneous claims is settled under the
   store's lock.
2. **Off-box claim degrades to token redemption.** Enabling `UserAccounts` (like enabling
   authentication) mints the **bootstrap owner token** if no admin credential exists. Presenting
   that token as an Admin-scoped bearer authorises a claim from anywhere — the v0.1 bootstrap-token
   design survives as exactly this path, so a headless/remote deployment keeps a first-class flow.
3. **The unclaimed state is loud.** `serve` logs a recurring warning while unclaimed and
   `/admin/api/status` reports `unclaimed: true` (+ `account_count`), so an exposed unclaimed
   instance is visible to its operator, never silent.

Supporting decisions:

- **Recovery** for a lost sole admin stays the config-file escape hatch (ADR 0009 §3):
  `[accounts] reopen_claim = true` re-opens the window for one boot (audited, loudly warned).
  No email/reset flow in v1.
- **Accounts imply a gate.** `UserAccounts = on` raises the *effective* auth mode to at least
  `Token` (`Off` and accounts cannot coexist); `Anonymous` is preserved as public-read +
  login-to-elevate.
- **Groups are share targets, not roles.** The same issue introduces groups + source/collection
  shares (tech-spec 10 §4.3). Recorded here explicitly: the ADR 0009 "no custom roles in v1"
  freeze is about *permission levels* and does not block groups, which govern *reach*.

## Consequences

- The solo-dev happy path is a signup form on first launch — no log-spelunking; the secure path
  for remote deployments is unchanged in spirit (a secret minted at gate-up time).
- The loopback gate needs the peer address, so the serve stack registers axum connect-info; the
  in-process test seam (no socket) treats "unknown peer" as *not* loopback and relies on the
  bind posture.
- A claimed instance can never be re-claimed except through the audited config escape hatch —
  the window is one-way by design.
- The web client renders the claim screen off the public `GET /api/v1/auth/status`
  (`unclaimed: true`), which reveals only what the claim gate itself would.
