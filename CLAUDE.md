# ttrpg-collector-feeder

Dev-only Discord bot used by the E2E test harness for `ttrpg-collector`. Joins
a voice channel and plays a pre-recorded WAV on command. Four identical
containers (`moe`, `larry`, `curly`, `gygax`) run in the dev compose stack,
each with its own bot token and voice clip.

Control API (loopback-only axum server):

- `GET  /health` — name, user id, in_voice, playing
- `POST /join`   — `{ "guild_id": u64, "channel_id": u64 }`
- `POST /play`   — start playing `AUDIO_FILE`
- `POST /stop`   — stop current track
- `POST /leave`  — leave voice channel

Env: `DISCORD_TOKEN`, `FEEDER_NAME`, `AUDIO_FILE`, `CONTROL_PORT` (default 8003).

## Conventions

Org-wide Rust style, git workflow, and review conventions live in
`/home/alex/sessionhelper-hub/CLAUDE.md`. Follow them.

**Exception to the shared-secret auth model:** the feeder does not use the
`SHARED_SECRET` / `/internal/auth` flow that every other sibling Rust service
uses. It only talks to Discord (bot token) and its own loopback control
server. No service-to-service auth to worry about.
