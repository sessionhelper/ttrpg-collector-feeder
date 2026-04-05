# E2E feeder bot for ttrpg-collector.
#
# Dev-only Discord bot: joins a voice channel and plays a pre-recorded WAV on
# command via a loopback HTTP control API. Four identical containers run in
# the dev compose stack (moe / larry / curly / gygax), each with its own
# DISCORD_TOKEN and AUDIO_FILE.

FROM rust:1.94-bookworm AS builder
RUN apt-get update && apt-get install -y cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/ttrpg-collector-feeder /usr/local/bin/ttrpg-collector-feeder
COPY assets/ /assets/
CMD ["ttrpg-collector-feeder"]
