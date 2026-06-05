# syntax=docker/dockerfile:1.7
#
# VaultPlane Gateway data plane container image.
#
# Multi-stage build: a Debian-based Rust toolchain produces a release binary
# of the `vaultplane` crate, and the binary is copied into a distroless
# runtime image that has libc and libssl (required by the native-tls
# outbound provider client) but no shell, package manager, or root user.
#
# Built and pushed by `.github/workflows/docker.yml` to
# `ghcr.io/vaultplane/vaultplane-gateway`. The image is linux/amd64 today;
# multi-arch (linux/arm64) is a follow-up.

FROM rust:1.85-slim AS builder

# Build deps for native-tls (the outbound reqwest client uses it on Linux).
# Inbound TLS uses rustls via axum-server and does NOT need cmake or nasm.
RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the whole workspace so cargo can resolve every crate (`Cargo.toml`
# `exclude` paths still need to exist as directories on disk).
COPY . .

# BuildKit cache mounts keep the cargo registry and target dir warm across
# successive builds in CI.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release -p vaultplane && \
    cp /build/target/release/vaultplane /usr/local/bin/vaultplane

# Distroless `cc-debian12` ships libc, libssl3, and libcrypto3, has no shell
# or package manager, and runs as the non-root `nonroot` user (UID 65532).
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /usr/local/bin/vaultplane /usr/local/bin/vaultplane

# Apache 2.0 requires the NOTICE to travel with redistributed binaries.
COPY LICENSE /LICENSE
COPY NOTICE /NOTICE

# Proxy and admin ports (the listen addresses can be overridden via the
# config file or VAULTPLANE_* env vars).
EXPOSE 8080 9091

ENTRYPOINT ["/usr/local/bin/vaultplane"]
