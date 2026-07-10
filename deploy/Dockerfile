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
# Rust >= 1.88: the locked deps have drifted past the workspace's nominal 1.85 MSRV — image@0.25.10
# needs rustc 1.88 and wgpu/naga@30 need 1.87 (they compile in both the wasm island build and the
# native binary). CI elsewhere uses @stable so never pinned; this image must not lag behind.
FROM rust:1.90-bookworm AS builder

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

USER 3dam
WORKDIR /var/lib/3dam
VOLUME ["/var/lib/3dam"]
EXPOSE 7878

# Readiness probe (issue #75): distinct from /api/version; 200 only once the pipeline is up.
HEALTHCHECK --interval=30s --timeout=3s --start-period=20s \
    CMD curl -fsS http://127.0.0.1:7878/readyz || exit 1

ENV RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/3dam", "serve", "--data", "/var/lib/3dam", "--addr", "0.0.0.0:7878"]
