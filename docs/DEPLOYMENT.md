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

## Authentication

Turn on token auth so only holders of a bearer token reach the API/MCP/admin surface:

```bash
3dam admin flags authentication --set token   # or seed `[flags] auth = "token"` in the config
3dam admin token create --label "web client" --scopes read,write
```

Clients present the token: web via the in-app **Connect** screen, native GUI via `--token` or its
Connect dialog, CLI via `--token`.

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
