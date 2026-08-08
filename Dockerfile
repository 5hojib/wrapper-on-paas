# wrapper-v2 image.
#
# Runs on privileged hosts (Docker/self-hosted, via chroot) and on rootless
# PaaS (Heroku/Render): when the container is unprivileged the supervisor
# execs the daemon directly through the Android linker instead of chrooting,
# using the /system + /data layout at the image root. The HTTP port follows
# the platform-injected $PORT (WRAPPER_PORT wins if set).
#
# x86_64 only. The heavy, immutable files (Android system binaries, Apple Music
# native libraries, the prebuilt NDK daemon) live in a base image built once by
# Dockerfile.base and pushed to a registry (default ghcr.io/<repo>/wrapper-base).
# This Dockerfile layers the Rust supervisor on top of it: no NDK, no APK
# download, no extraction at build time.
#
# To rebuild the base image (new Apple Music build, C++ changes):
#   docker build -f Dockerfile.base --build-arg APK_URL=... -t wrapper-base .
# or use the publish-base workflow. Then commit the updated base tag.

ARG BASE_IMAGE=ghcr.io/5hojib/wrapper-on-paas/wrapper-base:latest
ARG BUILD_PLATFORM=linux/amd64

# -----------------------------------------------------------------------------
# Base image carrying the staged rootfs
# -----------------------------------------------------------------------------
FROM --platform=${BUILD_PLATFORM} ${BASE_IMAGE} AS base

# -----------------------------------------------------------------------------
# Build stage: Rust supervisor only
# -----------------------------------------------------------------------------
FROM --platform=${BUILD_PLATFORM} debian:13.2 AS build

SHELL ["/bin/bash", "-c"]
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:${PATH}
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/rust ./src/rust

RUN cargo build --release && \
    cp target/release/wrapper /app/wrapper

# -----------------------------------------------------------------------------
# Runtime stage: base image + the Rust supervisor
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE}

# ca-certificates for SSL verification in rootless mode (the chroot mode uses
# the bundle committed under rootfs/etc/ssl/certs inside the base image).
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/wrapper /app/wrapper

# Rootless layout: expose the staged Android system at the image root so the
# supervisor can exec the daemon through linker64 without chroot.
RUN ln -s /rootfs/system /system && \
    chmod 755 /rootfs/system/bin/linker64 /rootfs/system/bin/main

# Persistent Apple state; PaaS mounts a volume here.
RUN mkdir -p /data/data/com.apple.android.music/files && \
    chown -R 1000:0 /data && \
    chmod -R g=u /data

# Bionic environment for rootless mode (chroot mode sets these itself).
ENV ANDROID_DATA=/data \
    ANDROID_ROOT=/system \
    ANDROID_DNS_MODE=local \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt \
    PORT=8080

# Heroku/Render inject the public port via $PORT; WRAPPER_PORT wins if set.
# Disable the raw TCP decrypt listener with WRAPPER_DECRYPT_PORT=0 and use
# HTTP POST /decrypt on the same single dynamic port.
EXPOSE 8080

USER 1000
ENTRYPOINT ["/app/wrapper"]
