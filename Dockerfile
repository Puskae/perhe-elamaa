# syntax=docker/dockerfile:1
# Build stage: compile the server and the WASM web app
# Base images are pinned by digest (tag kept for readability). Dependabot's
# `docker` ecosystem (.github/dependabot.yml) bumps the digests; don't hand-edit
# one without re-resolving it. `rust:1-slim-trixie` is the same image as the
# floating `rust:1-slim` at the time of pinning, named explicitly so the Debian
# release visibly matches the runtime stage below.
# NOTE: the digest pins the image, not the compiler — rust-toolchain.toml says
# `channel = "stable"`, so rustup installs whatever stable is current at build
# time once the source is copied in. Pin that file to an exact version (e.g.
# "1.xx.0") for a fully reproducible toolchain.
FROM rust:1-slim-trixie@sha256:b8e062b1c3a80183df83d9ba187cadd2b5ab84bc78d453dbfeb2fccd5f982247 AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
# dioxus-cli (`dx`) straight from the Dioxus GitHub release, pinned to the
# exact version of the `dioxus` crate in Cargo.lock and verified against a
# pinned SHA-256 per architecture (each matches the release's published
# dx-<arch>-unknown-linux-gnu.sha256). No piped install script, no
# cargo-binstall, no resolution at build time.
# TARGETARCH is set automatically by BuildKit (`amd64`, `arm64`, …) — declaring
# the ARG is what makes it visible to RUN. dx runs INSIDE the builder, so it
# must match the builder's architecture, i.e. the platform being built for.
# When bumping `dioxus` in Cargo.lock, bump DX_VERSION and BOTH SHAs together:
#   curl -fsSL https://github.com/DioxusLabs/dioxus/releases/download/v<ver>/dx-x86_64-unknown-linux-gnu.sha256
#   curl -fsSL https://github.com/DioxusLabs/dioxus/releases/download/v<ver>/dx-aarch64-unknown-linux-gnu.sha256
ARG TARGETARCH
ARG DX_VERSION=0.7.10
ARG DX_SHA256_AMD64=4363e4ed2a3f1eb7f4d38d2d59aed59ce43271c44c16b425e92c89a64761fbe7
ARG DX_SHA256_ARM64=8f1a17d3218700ffbe15e6540d936a178b2556fc801121a31082e3ba4ab9ef55
RUN set -eu; \
    case "${TARGETARCH:-}" in \
        amd64) dx_arch=x86_64;  dx_sha="${DX_SHA256_AMD64}" ;; \
        arm64) dx_arch=aarch64; dx_sha="${DX_SHA256_ARM64}" ;; \
        "") echo "TARGETARCH is not set: this Dockerfile needs BuildKit, which Docker only uses when the buildx plugin is installed (otherwise it falls back to the legacy builder). Install it (Arch: docker-buildx; Debian/Ubuntu with Docker's repo: docker-buildx-plugin), check with 'docker buildx version', then rebuild." >&2; exit 1 ;; \
        *) echo "Unsupported build architecture '${TARGETARCH}': the Dioxus CLI (dx) ships prebuilt for amd64 and arm64 only. Build for one of those platforms." >&2; exit 1 ;; \
    esac; \
    curl -fsSL --proto '=https' --tlsv1.2 -o /tmp/dx.tar.gz \
        "https://github.com/DioxusLabs/dioxus/releases/download/v${DX_VERSION}/dx-${dx_arch}-unknown-linux-gnu.tar.gz"; \
    echo "${dx_sha}  /tmp/dx.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/dx.tar.gz -C /usr/local/cargo/bin dx; \
    rm /tmp/dx.tar.gz; \
    dx --version

COPY . .
# BuildKit cache mounts: target/ and the cargo registry survive between image
# builds, so a redeploy only recompiles our own crates instead of every
# dependency from scratch (the `COPY . .` above invalidates this layer on any
# source change — without the mounts that meant a full cold build every time).
# Cache mounts are NOT part of the image, so anything the runtime stage needs
# must be copied out to a normal layer inside the same RUN.
RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release -p perkele-server \
    && cp target/release/perkele-server /build/perkele-server
RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cd crates/app && dx build --release \
    && cp -r /build/target/dx/perkele-app/release/web/public /build/public

# Runtime stage: one small image, one binary + static assets
# Must track the builder's Debian release (currently trixie, via the
# `rust:1-slim-trixie` pin above): the prebuilt dx binary in the builder needs
# glibc 2.39, newer than bookworm ships, and a Rust binary
# built here links dynamically against whatever libssl is present at build
# time — mismatched releases produce "GLIBC_x not found" or
# "OPENSSL_x not found" errors at container startup.
# Digest-pinned like the builder; Dependabot bumps it.
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a
# tzdata + TZ: the app stores calendar/chore times as FLOATING wall-clock and
# the reminder scheduler compares them against chrono::Local::now() (see
# crates/server/src/reminder.rs). That only fires on time if the container's
# local timezone IS the family's timezone. debian-slim defaults to UTC, which
# made reminders fire hours late (off by the UTC offset). Install tzdata so the
# zone database exists, then pin TZ to the family's zone (overridable via
# compose). chrono then tracks DST automatically.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl tzdata \
    && rm -rf /var/lib/apt/lists/*
ENV TZ=Europe/Helsinki
RUN useradd --system --home /app perkele
WORKDIR /app

COPY --from=builder /build/perkele-server /app/perkele-server
COPY --from=builder /build/public /app/public

# Create the data directory owned by the runtime user *before* the volume is
# mounted. A named volume mounted on an empty, image-created dir inherits that
# dir's ownership — so the non-root `perkele` user can write the database here.
RUN mkdir -p /app/data && chown perkele:perkele /app/data
VOLUME ["/app/data"]

USER perkele
ENV PERKELE_ADDR=0.0.0.0:8080
ENV PERKELE_DIST=/app/public
# Keep the SQLite database (and its -wal/-shm files) inside the volume so data
# survives container recreation and image rebuilds.
ENV DATABASE_URL=sqlite:///app/data/perkele.db
EXPOSE 8080

# Cheap liveness check so `docker ps` and compose report real health.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/health || exit 1

CMD ["/app/perkele-server"]
