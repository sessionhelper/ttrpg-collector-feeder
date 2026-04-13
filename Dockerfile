# E2E feeder bot for chronicle-bot.
#
# Dev-only Discord bot: joins a voice channel and plays a pre-recorded WAV on
# command via a loopback HTTP control API. Four identical containers run in
# the dev compose stack (moe / larry / curly / gygax), each with its own
# DISCORD_TOKEN and AUDIO_FILE.
#
# Multi-arch (amd64 + arm64) build via cross-compilation — see chronicle-bot
# Dockerfile for the rationale.

FROM --platform=$BUILDPLATFORM rust:1.94-bookworm AS builder

ARG BUILDPLATFORM
ARG TARGETPLATFORM
ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross \
        gcc-x86-64-linux-gnu g++-x86-64-linux-gnu libc6-dev-amd64-cross \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    case "$TARGETARCH" in \
        amd64) RUST_TARGET=x86_64-unknown-linux-gnu ;; \
        arm64) RUST_TARGET=aarch64-unknown-linux-gnu ;; \
        *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    rustup target add "$RUST_TARGET"; \
    echo "$RUST_TARGET" > /tmp/rust_target

ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CXX_x86_64_unknown_linux_gnu=x86_64-linux-gnu-g++ \
    AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --target "$(cat /tmp/rust_target)" \
 && mkdir -p /out \
 && cp "target/$(cat /tmp/rust_target)/release/chronicle-feeder" /out/chronicle-feeder

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/chronicle-feeder /usr/local/bin/chronicle-feeder
COPY assets/ /assets/
CMD ["chronicle-feeder"]
