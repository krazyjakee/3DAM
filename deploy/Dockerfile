# Server-focused container for a headless 3DAM deployment (issue #75). Builds the unified `3dam`
# binary and runs the `serve` role. Multi-stage: a fat builder (Rust + Node + Assimp/Draco toolchain)
# and a slim runtime carrying only the shared libraries the binary links.
#
#   docker build -f deploy/Dockerfile -t 3dam-server .
#   docker run -p 7878:7878 -v 3dam-data:/var/lib/3dam 3dam-server
#
# TLS: mount a cert/key and add `--tls-cert/--tls-key` (see deploy/3dam.example.toml), or terminate
# TLS in a reverse proxy and keep the container on localhost.

# ---- builder ------------------------------------------------------------------------------------
# Recent stable required: the locked deps have drifted well past the workspace's nominal 1.85 MSRV —
# image@0.25.10 needs rustc 1.88, wgpu/naga@30 need 1.87, and libsqlite3-sys's build script uses
# `std::cfg_select` (stable since 1.91). 1.95 clears all of these with headroom. CI elsewhere uses
# @stable so it never noticed; this pinned image must track a recent stable, not lag behind.
FROM rust:1.95-bookworm AS builder

# Native deps: egui/eframe (GTK/xcb/wayland — the unified binary links the GUI), audio (ALSA), TLS
# (openssl/pkg-config), and the Assimp/Draco 3D toolchain (cmake + clang, ADR 0011).
RUN apt-get update && apt-get install -y --no-install-recommends \
        libgtk-3-dev libxkbcommon-dev libwayland-dev \
        libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb1-dev \
        libasound2-dev libssl-dev pkg-config \
        cmake clang libclang-dev \
        curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Node + pnpm for the web client.
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g pnpm@9 \
    && rm -rf /var/lib/apt/lists/*

# wasm-pack for the viewer islands. web/src/wasm/ is a build artifact (not committed), so it must be
# generated here before the web build. Install the PREBUILT binary rather than `cargo install
# wasm-pack` — the static musl binary is toolchain-independent and avoids a slow from-source build.
ARG WASM_PACK_VERSION=0.13.1
RUN curl -fsSL "https://github.com/rustwasm/wasm-pack/releases/download/v${WASM_PACK_VERSION}/wasm-pack-v${WASM_PACK_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin --wildcards '*/wasm-pack' \
    && wasm-pack --version

WORKDIR /src
COPY . .

# Build the web client (→ web/dist, baked in by rust-embed) then the release binary.
RUN cargo xtask web \
    && cargo build -p dam --release --locked

# ---- runtime ------------------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Runtime shared libs the binary links (the -dev headers above are build-only). Mesa's software
# Vulkan (lavapipe) lets 3D turntable thumbnails render without a GPU; without it they fail soft.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libgtk-3-0 libxkbcommon0 libwayland-client0 \
        libxcb-render0 libxcb-shape0 libxcb-xfixes0 libxcb1 \
        libasound2 libssl3 ca-certificates curl \
        mesa-vulkan-drivers \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /var/lib/3dam --shell /usr/sbin/nologin 3dam \
    && mkdir -p /var/lib/3dam && chown 3dam:3dam /var/lib/3dam

COPY --from=builder /src/target/release/3dam /usr/local/bin/3dam
# First-run flag seeds for the container posture (see the file's comments): auth off +
# network_writes on, so the web UI is fully functional the moment the container is up.
COPY deploy/3dam-container.toml /etc/3dam/3dam.toml

USER 3dam
WORKDIR /var/lib/3dam
VOLUME ["/var/lib/3dam"]
EXPOSE 7878

# Readiness probe (issue #75): distinct from /api/version; 200 only once the pipeline is up.
HEALTHCHECK --interval=30s --timeout=3s --start-period=20s \
    CMD curl -fsS http://127.0.0.1:7878/readyz || exit 1

ENV RUST_LOG=info

# ENTRYPOINT is the binary; CMD is the default (overridable) serve invocation. The container must
# bind 0.0.0.0 for the published port to be reachable, and `serve` refuses a plaintext non-localhost
# bind unless TLS is configured or --insecure is passed. Here the Docker network boundary + port
# mapping is the trust boundary, so we default to --insecure, and /etc/3dam/3dam.toml seeds
# `auth = off` + `network_writes = true` on first run so the web UI works out of the box — no tokens,
# no flag flipping. Authentication exists only once the operator configures it; the server still logs
# a warning while exposed without auth/TLS.
#
# HARDEN for production (untrusted network): enable token auth —
#   docker exec <name> 3dam admin flag authentication --set token --confirm --data /var/lib/3dam
# (note --data: without it, admin commands hit a fresh $HOME database, not the server's) — then mint
# tokens with `3dam admin token add`, and either terminate TLS in a reverse proxy or override CMD:
#   docker run … 3dam-server serve --data /var/lib/3dam --addr 0.0.0.0:7878 \
#     --config /etc/3dam/3dam.toml --tls-cert /etc/3dam/tls/fullchain.pem --tls-key /etc/3dam/tls/privkey.pem
# Or mount deploy/3dam.example.toml (the hardened template) over /etc/3dam/3dam.toml before first run.
ENTRYPOINT ["/usr/local/bin/3dam"]
CMD ["serve", "--data", "/var/lib/3dam", "--addr", "0.0.0.0:7878", "--insecure", "--config", "/etc/3dam/3dam.toml"]
