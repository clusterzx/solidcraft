# syntax=docker/dockerfile:1
# SolidCraft in the browser as containers. From the repository root:
#
#   docker build -f packaging/docker/Dockerfile -t solidcraft-web .
#   docker run --rm -p 8080:8080 solidcraft-web          then open http://localhost:8080/
#
# builds the web app: the static site (the one packaging/web/package.sh zips) served by nginx on
# port 8080, as an unprivileged user. `--target relay` builds solidcraft-relay instead, the MCP
# server that lets an agent drive the design in a browser tab (docs/web-mcp.md); nginx passes
# /mcp and /bridge on to it. `docker compose up` with packaging/docker/compose.yaml runs both.
# Saving designs needs HTTPS or localhost (the browser's private file storage), so put a TLS
# reverse proxy in front of it on a server; the site works under any URL path.
#
# The site is wasm, the same on every platform, so it is built once on the build machine's own
# platform even for a multi-platform image. CI builds it outside Docker (with its Rust cache and
# git commit for Help › About) and passes it in as the `site` stage: --build-context site=dist/web.
# The relay is a native program: it is built on the build machine's platform for each target
# platform (cross-compiled; it is pure Rust, so only the linker comes from Debian).

ARG RUST_VERSION=1.95
ARG TRUNK_VERSION=0.21.14

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim-bookworm AS build
ARG TRUNK_VERSION
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
# rust-src: the build with threads compiles its own standard library (docs/web-threads.md).
RUN rustup target add wasm32-unknown-unknown \
 && rustup component add rust-src \
 && curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-$(uname -m)-unknown-linux-gnu.tar.gz" \
    | tar -xz -C /usr/local/bin trunk
WORKDIR /src
COPY . .
WORKDIR /src/apps/solidcraft-web
# Trunk downloads the wasm-bindgen and wasm-opt its build needs; its post-build hook writes sw.js.
# Then the build with threads goes beside it (docs/web-threads.md).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    trunk build --release \
 && cd /src && cargo xtask web-threads dist/web

FROM scratch AS site
COPY --from=build /src/dist/web/ /

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim-bookworm AS relay-build
ARG TARGETARCH
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
# The target's Rust standard library and, building for another architecture, its linker and C
# library (Debian's cross toolchains).
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu; arch=x86_64; gcc=gcc-x86-64-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu; arch=aarch64; gcc=gcc-aarch64-linux-gnu ;; \
      *) echo "solidcraft-relay: no build for $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && echo "$target" > /rust-target \
 && rustup target add "$target" \
 && if [ "$(uname -m)" != "$arch" ]; then \
      apt-get update \
      && apt-get install -y --no-install-recommends "$gcc" "libc6-dev-${TARGETARCH}-cross" \
      && rm -rf /var/lib/apt/lists/*; \
    fi \
 && mkdir /data
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
WORKDIR /src
COPY . .
# Without the release profile's line tables (they are for the desktop app's crash reports and
# would make this binary several times its size).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target-relay,id=solidcraft-relay-${TARGETARCH} \
    CARGO_PROFILE_RELEASE_DEBUG=false CARGO_PROFILE_RELEASE_STRIP=debuginfo \
    cargo build --release --locked -p solidcraft-relay --target "$(cat /rust-target)" --target-dir /src/target-relay \
 && cp "/src/target-relay/$(cat /rust-target)/release/solidcraft-relay" /usr/local/bin/solidcraft-relay

# The relay: no shell, no package manager, an unprivileged user (65532). It listens on 8090 in
# the container; compose.yaml lets only the web app's nginx reach it. Its token comes from the
# environment (SOLIDCRAFT_RELAY_TOKEN or SOLIDCRAFT_RELAY_TOKEN_FILE); without one it does not
# start. /data is its headless session's folder (SOLIDCRAFT_RELAY_HEADLESS, off by default).
FROM gcr.io/distroless/cc-debian12:nonroot AS relay
COPY --from=relay-build /usr/local/bin/solidcraft-relay /usr/local/bin/solidcraft-relay
COPY --from=relay-build --chown=65532:65532 /data /data
COPY LICENSE-MIT LICENSE-APACHE NOTICE THIRD-PARTY-LICENSES.txt /usr/share/doc/solidcraft/
ENV SOLIDCRAFT_RELAY_LISTEN=0.0.0.0:8090 \
    SOLIDCRAFT_RELAY_DATA=/data
EXPOSE 8090
HEALTHCHECK --interval=30s --timeout=5s CMD ["/usr/local/bin/solidcraft-relay", "--health"]
ENTRYPOINT ["/usr/local/bin/solidcraft-relay"]

# The web app (the default target: last).
FROM nginxinc/nginx-unprivileged:1.29-alpine AS web
COPY packaging/docker/nginx.conf /etc/nginx/conf.d/default.conf
COPY LICENSE-MIT LICENSE-APACHE NOTICE THIRD-PARTY-LICENSES.txt /usr/share/doc/solidcraft/
COPY --from=site / /srv/solidcraft/
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s CMD wget -q --spider http://127.0.0.1:8080/ || exit 1
