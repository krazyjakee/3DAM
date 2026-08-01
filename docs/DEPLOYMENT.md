# Deploying a hosted 3DAM server

Hosted mode (milestone 7, epic [#69](https://github.com/krazyjakee/3DAM/issues/69)) runs **one
authoritative `3dam serve`** that many thin clients — web and native GUI — connect to over the API.
This guide covers running that server headless and exposing it safely (issue
[#75](https://github.com/krazyjakee/3DAM/issues/75)).

The `3dam` binary is one binary, four roles; `serve` is the long-running server role. It owns the
catalog (`library.db`), server config (`server.db`), and the derivative caches under one `--data`
directory, watches its sources for changes, and — in hosted mode — proactively renders thumbnails
and runs analysis in the background so clients hit ready data (issue #71).

## Safe-by-default posture

Out of the box `serve` binds `127.0.0.1:7878` with **no auth** (the local owner is trusted) and is
**read-only to the network**. Nothing is exposed until you opt in. To expose the server you must
either give it TLS or acknowledge an insecure bind — it refuses to bind beyond localhost in
plaintext otherwise (ADR 0009 §4).

## Outbound source credentials

SFTP/SMB passwords, SFTP private-key paths and passphrases, and federated-peer bearer tokens never
enter `library.db`, manifests, diagnostics, or source-list responses. The catalog stores only an
opaque `source.auth_ref`; interactive desktop/CLI installations resolve it through the native OS
credential store (macOS Keychain, Windows Credential Manager, or Linux Secret Service/keyutils).
Copying `library.db` to another host therefore copies no live network credential. Protected sources
on the new host report their credential as missing until an operator re-enters it.

Headless Linux services commonly have no unlocked desktop Secret Service session. Configure the
explicit file-backed host secret store in that case:

```ini
# systemd Environment= accepts this name even though POSIX shell assignment syntax does not.
Environment=3DAM_SOURCE_SECRET_DIR=/var/lib/3dam-host-secrets
```

The path must be absolute, outside `--data`, and a real directory rather than a symlink. 3DAM sets
and verifies mode `0700` on the directory and `0600` on its atomically-written files; the file
backend is Unix-only. Put the directory on persistent host storage, exclude it from portable-library
backups/exports, and grant access only to the service account. Windows headless services use Windows
Credential Manager rather than this fallback.

The file backend stores credential payloads as plaintext protected by the OS account boundary. This
matches the headless threat model: another unprivileged account cannot read the files; the service
account and root/host administrator can, and can already inspect the running process memory. Disk
encryption and secret-volume controls remain the operator's responsibility.

On first open of an older library, 3DAM writes every discovered credential to the host backend,
then rewrites all source rows in one transaction, checkpoints/truncates the WAL, and vacuums the DB
so plaintext does not remain in free pages. If the keychain is locked or unavailable, startup stops
without rewriting the old rows; unlock/fix the backend and retry. If a crash occurs after row
redaction but before physical cleanup, a durable migration marker remains absent and the next open
retries the scrub. Normal source listing/scan/upload/federation operations report a locked, missing,
or invalid credential explicitly without revealing the secret, opaque ref, provider error, or host
path. Source removal and catalog/factory reset queue opaque refs transactionally and retry host-store
deletion after a crash or temporary lock.

## Exposing over TLS (no `--insecure`)

Provide a PEM cert chain and private key; `serve` then speaks HTTPS and binds beyond localhost with
no `--insecure`:

```bash
3dam serve \
  --data /var/lib/3dam \
  --addr 0.0.0.0:7878 \
  --tls-cert /etc/3dam/tls/fullchain.pem \
  --tls-key  /etc/3dam/tls/privkey.pem
```

Both can also be set in the config file (`[server] tls_cert`/`tls_key`, see
[`deploy/3dam.example.toml`](../deploy/3dam.example.toml)); CLI flags win over the file.

### Alternative: terminate TLS in front

Keep the server on localhost and let a reverse proxy (Caddy, nginx, Traefik) terminate TLS and
proxy to `127.0.0.1:7878`. No `--insecure` is needed because the server itself stays on loopback.
Proxy both the REST paths and the `/api/v1/ws` WebSocket upgrade.

> **If you use user accounts behind a proxy, set these keys.** From the server's point of view every
> proxied request arrives from `127.0.0.1` over plaintext HTTP, so it cannot tell a local operator
> from an internet visitor, nor an HTTPS browser from a plaintext one. Add to the config file:
>
> ```toml
> [server]
> secure_cookies = true        # the browser really is on HTTPS — mark the session cookie Secure
> trusted_proxies = ["127.0.0.1", "::1"] # only these socket peers may assert a client IP
>
> [accounts]
> require_claim_token = true   # only the bootstrap owner token may claim this instance
> ```
>
> 3DAM already refuses the open first-run claim for any request carrying `X-Forwarded-For`,
> `X-Real-IP`, or `Forwarded` — but a proxy configured to strip those leaves no signal, and
> `require_claim_token` closes the path unconditionally. See [ADR 0014](adr/0014-first-run-claim.md).
> To claim such an instance, redeem the bootstrap owner token written to
> `<data>/bootstrap-owner-token.txt`:
>
> ```bash
> curl -sX POST https://host/api/v1/auth/claim \
>   -H "authorization: Bearer $(cat /var/lib/3dam/bootstrap-owner-token.txt)" \
>   -H 'content-type: application/json' \
>   -d '{"username":"owner","password":"a-long-passphrase"}'
> ```

`trusted_proxies` is deliberately an exact IP allow-list, not a switch that trusts every
`X-Forwarded-For`. Each listed proxy must remove the client-supplied header and set exactly one
bare IPv4 or IPv6 address. 3DAM ignores forwarding headers from every other socket peer. If a
trusted proxy sends no value, a malformed value, multiple header lines, or a comma-separated
chain, attribution becomes “unknown behind this proxy”; those requests share one rate-limit bucket
and therefore fail stricter rather than bypassing the limit. `X-Real-IP`, `Forwarded`, and
`X-Forwarded-Proto` never influence rate-limit identity.

The unauthenticated account surface uses smoothly refilling token buckets per attributed client:
login allows a burst of 20 (one token per 3 seconds), claim 5 (one per minute), OIDC start 10 (one
per 6 seconds), OIDC callback 30 (one per 2 seconds), and status 60 (one per second). Login and
claim also charge a separate normalized-account bucket, so rotating source addresses cannot spray
one account and rotating usernames cannot multiply password work. Expensive endpoints also have a
process-wide bucket (OIDC start bursts to 20 and then refills once per 3 seconds), so address
rotation cannot turn the per-client policy into an unbounded provider request rate. Four password
hashes and four OIDC discovery requests may execute concurrently; excess requests receive typed
HTTP 429 replies with `Retry-After` instead of joining an unbounded queue. Callback capacity is
reserved before its single-use state is consumed, so a capacity 429 can be retried without
stranding a completed login.

Rate-limit decisions emit the coalesced `account.auth_rate_limited` audit action and a warning with
only the endpoint, limiting dimension, and retry delay. Client addresses, usernames, passwords,
cookies, authorization codes, OIDC state, and callback query data are not logged.

## Authentication

Turn on token auth so only holders of a bearer token reach the API/MCP/admin surface:

```bash
3dam admin flags authentication --set token   # or seed `[flags] auth = "token"` in the config
3dam admin token create --label "web client" --scopes read,write
```

Clients present the token: web via the in-app **Connect** screen, native GUI via `--token` or its
Connect dialog, CLI via `--token`.

Browser-entered tokens last for the current tab/session only; they are never written to
`localStorage`, IndexedDB, persisted TanStack caches, media URLs, or WebSocket URLs. The native GUI
stores a hosted `--connect` token in the OS keychain and can remove it with **File → Forget Server
Credential**. A server upgrade deliberately removes any legacy durable browser token while keeping
the saved server address, so the browser may ask for the token once more.

### Reverse-proxy log redaction

3DAM's trace spans record only the request path. Media/WS URLs contain only narrow derived tickets,
never bearer tokens, but proxy access logs should still redact the `ticket` query key as defense in
depth. Prefer logging `$uri` (path) instead of `$request_uri` in nginx, or configure the equivalent
query-string exclusion in Caddy/Traefik/load-balancer logs. Do not enable request-header dumps for
`Authorization`, `Cookie`, or `Set-Cookie`. Tickets are bounded to one exact media target for five
minutes (replayable only for browser Range requests) or to one WebSocket upgrade for 30 seconds
(single-use); proxy redaction ensures even those limited credentials do not enter ordinary logs.

## Run as a service (systemd)

An example unit ships at [`deploy/systemd/3dam-server.service`](../deploy/systemd/3dam-server.service):

```bash
sudo useradd --system --home /var/lib/3dam --shell /usr/sbin/nologin 3dam
sudo install -m0755 target/release/3dam /usr/local/bin/3dam
sudo install -D -m0644 deploy/systemd/3dam-server.service /etc/systemd/system/3dam-server.service
sudo install -D -m0644 deploy/3dam.example.toml /etc/3dam/3dam.toml
sudo systemctl daemon-reload && sudo systemctl enable --now 3dam-server
```

The unit uses `Type=simple`, a dedicated `3dam` user, a systemd `StateDirectory`, and forwards
`SIGTERM` to the server's graceful drain (≤10s) so restarts don't cut connections mid-request. It
survives host restarts (`WantedBy=multi-user.target`) against the persistent data dir.

## Health & readiness

Two unauthenticated probes, distinct from the versioned API (`/api/version`):

| Path       | Meaning   | Use |
|------------|-----------|-----|
| `/healthz` | liveness  | process is up and answering; does no work |
| `/readyz`  | readiness | `200` only once the engine, stores, and background job pipeline are wired; `503` while starting |

Point a load balancer / systemd watchdog / container `HEALTHCHECK` at `/readyz` so traffic arrives
only when the server can actually serve.

## Container

### Pull the published image

Tagged releases publish a `linux/amd64` server image to the GitHub Container Registry (GHCR) via
[`.github/workflows/container.yml`](../.github/workflows/container.yml). Pull and run it — no local
build needed:

```bash
docker pull ghcr.io/krazyjakee/3dam:latest        # or a pinned tag, e.g. :1.2.3
docker run -p 7878:7878 -v 3dam-data:/var/lib/3dam ghcr.io/krazyjakee/3dam:latest
```

Tags: `latest` tracks the newest release; `MAJOR.MINOR.PATCH` and `MAJOR.MINOR` pin a version;
`edge`/`sha-…` come from manual/dispatch builds. The image runs the `serve` role as a non-root
`3dam` user, binding `0.0.0.0:7878` with the data dir on the `/var/lib/3dam` volume.

> **Exposure.** A container must bind `0.0.0.0` for its published port to be reachable, so the
> default command passes `--insecure` (plaintext, no auth) — the Docker network + port mapping is the
> trust boundary, and the server logs an `exposed beyond localhost with no auth and no TLS` warning
> on start. **Before exposing it publicly**, turn on token auth (`3dam admin flags authentication
> --set token`) and add TLS — either front it with a TLS-terminating proxy, or override the command
> to pass `--tls-cert/--tls-key` directly (see the run example below). Supplying TLS replaces the
> `--insecure` default.

### Build the image yourself

A server-focused multi-stage image ships at [`deploy/Dockerfile`](../deploy/Dockerfile):

```bash
docker build -f deploy/Dockerfile -t 3dam-server .
docker run -p 7878:7878 -v 3dam-data:/var/lib/3dam 3dam-server
```

Either way, the `HEALTHCHECK` polls `/readyz`. Mount TLS material and add `--tls-cert/--tls-key` for
HTTPS, or front it with a TLS-terminating proxy:

```bash
docker run -p 7878:7878 \
  -v 3dam-data:/var/lib/3dam \
  -v /etc/3dam/tls:/etc/3dam/tls:ro \
  ghcr.io/krazyjakee/3dam:latest \
  serve --data /var/lib/3dam --addr 0.0.0.0:7878 \
        --tls-cert /etc/3dam/tls/fullchain.pem --tls-key /etc/3dam/tls/privkey.pem
```

## Startup posture logging

On start the server logs (and prints) its bind, scheme (http/https), TLS state, auth mode, MCP mode,
and network-write ceiling — the "am I safe to expose?" line — plus a warning if it is bound beyond
localhost with neither auth nor TLS.
