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

1. **Localhost-only claim by default — on positive evidence of a local peer.** `POST
   /api/v1/auth/claim` accepts the open (credential-less) path only when *all* of the following
   hold; a remote request to an unclaimed instance is refused (`403`), not served a signup form:

   | Condition | Why |
   |---|---|
   | The **peer socket** is loopback | The only fact about the caller we observe directly. |
   | The request carries **no `X-Forwarded-For` / `X-Real-IP` / `Forwarded`** header | Such a header is proof the request was *proxied*; its peer being 127.0.0.1 then says nothing about the client. |
   | `[accounts] require_claim_token` is not set | The operator's explicit "I am behind a proxy" switch. |

   The **bind** posture is deliberately *not* part of this test, though an earlier draft of this ADR
   allowed it ("or the bind itself is loopback-only"). That disjunct made the gate vacuous in the
   exact deployment [DEPLOYMENT.md](../DEPLOYMENT.md) recommends — server on `127.0.0.1` behind
   nginx/Caddy — where the bind *is* loopback-only and every internet visitor's peer *is* 127.0.0.1.
   Any visitor could have claimed the instance and become admin: the Jellyfin/Grafana CVE this ADR
   exists to prevent, reintroduced by the mitigation itself.

   The forwarded-header rule is a *negative signal only*: we never read the header's value to
   recover a client IP (that needs a trusted-proxy list this project does not have and does not
   want). A client that forges one merely locks itself out of the open path — failing safe. A proxy
   that strips them leaves no signal at all, which is what `require_claim_token` is for.

   The claim window closes permanently on the first successful claim. Two simultaneous claims are
   settled inside a single `BEGIN IMMEDIATE` transaction — the check, the insert, and the closing of
   a re-opened window are one atomic step, with the argon2 hash computed *outside* it.
2. **Off-box claim degrades to token redemption.** Enabling `UserAccounts` (like enabling
   authentication) mints the **bootstrap owner token** if no admin credential exists. Presenting
   that token as an Admin-scoped bearer authorises a claim from anywhere — the v0.1 bootstrap-token
   design survives as exactly this path, so a headless/remote deployment keeps a first-class flow.
   This path is **unconditional**: it is unaffected by the peer address, by forwarded headers, and
   by `require_claim_token`. Every tightening of gate 1 is therefore safe by construction — there is
   always a documented way in for an operator who can read the server's data directory.
3. **The unclaimed state is loud.** `serve` logs a recurring warning while unclaimed and
   `/admin/api/status` reports `unclaimed: true` (+ `account_count`), so an exposed unclaimed
   instance is visible to its operator, never silent.

Supporting decisions:

- **Recovery** for a lost sole admin stays the config-file escape hatch (ADR 0009 §3):
  `[accounts] reopen_claim = true` re-opens the window for one boot (audited, loudly warned).
  No email/reset flow in v1. Re-opening the window does **not** lock existing users out of the
  sign-in form: while accounts exist the web client shows the normal login screen with the claim
  form as a secondary path, and only an instance with genuinely zero accounts gets the claim screen
  exclusively.
- **Cookie `Secure` is an operator declaration, not a header sniff.** The session cookie's `Secure`
  attribute follows `tls || [server] secure_cookies`. The same proxy deployment that motivates
  `require_claim_token` also hides the browser's real scheme from us, and `X-Forwarded-Proto` is
  attacker-controlled wherever forwarded headers are, so the safe form is an explicit opt-in.
- **Accounts-off is exposure-increasing.** Because accounts raise the effective auth mode, turning
  `UserAccounts` **off** while `authentication = off` removes the only gate on the instance. That
  flip therefore requires `confirm=true`, exactly like removing authentication itself (ADR 0004).
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
  in-process test seam (no socket) injects the same `ConnectInfo` extension, and an *absent* peer
  is treated as not-loopback.
- **A reverse-proxy deployment must redeem the bootstrap token or set `require_claim_token`.**
  Claiming through nginx/Caddy no longer "just works", which is the point: the friendly path is for
  the machine the server runs on, and everything else presents a secret. Operators who proxy see the
  `403` with the token's file path named in the server log.
- A claimed instance can never be re-claimed except through the audited config escape hatch —
  the window is one-way by design.
- The web client renders the claim screen off the public `GET /api/v1/auth/status`
  (`unclaimed: true`), which reveals only what the claim gate itself would.
