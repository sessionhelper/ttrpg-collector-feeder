# chronicle-feeder

Dev-only Discord bot used by the E2E test harness for `chronicle-bot`. Joins
a voice channel and plays a pre-encoded OGG Opus file on command. Four
identical containers (`moe`, `larry`, `curly`, `gygax`) run in the dev
compose stack, each with its own bot token and voice clip.

Authoritative spec: `sessionhelper-hub/docs/modules/chronicle-feeder.md`.

State machine: `Idle → Joined → Playing → Joined → Idle` — `/play` is
an explicit transition, `/stop` tears playback back to `Joined`, `/leave`
returns to `Idle`. Double-`/play` returns 409. No silence loop; between
`/join` and `/play` the feeder is silent.

Control API (loopback-only axum server):

- `GET  /health` — name, user id, in_voice, playing
- `POST /join`   — `{ "guild_id": u64, "channel_id": u64 }` — 409 if already joined
- `POST /play`   — play `AUDIO_FILE` — 400 if not in voice, 409 if already playing, 404 if file missing
- `POST /stop`   — stop track (no-op if not playing)
- `POST /leave`  — leave (implicit stop if playing, no-op if not joined)

Env: `DISCORD_TOKEN`, `FEEDER_NAME`, `AUDIO_FILE`, `CONTROL_PORT` (default
8003), `CONTROL_BIND` (default 127.0.0.1), `RUST_LOG`.

## Conventions

Org-wide Rust style, git workflow, and review conventions live in
`/home/alex/sessionhelper-hub/CLAUDE.md`. Follow them.

**Exception to the shared-secret auth model:** the feeder does not use the
`SHARED_SECRET` / `/internal/auth` flow that every other sibling Rust service
uses. It only talks to Discord (bot token) and its own loopback control
server. No service-to-service auth to worry about.

**Exception to consent:** feeders do not participate in consent via a bypass
list. Test runners drive the chronicle-bot harness endpoints (`/enrol`,
`/consent`, `/license`) with feeder user_ids as test-user identities, so
the same code paths human participants hit are exercised end-to-end.
