chronicle-feeder
================

E2E test harness feeder bot for [`chronicle-bot`](https://github.com/sessionhelper/chronicle-bot).
Dev-only.

A minimal Discord bot that joins a voice channel and plays a pre-encoded
OGG Opus file on demand. Exists so the end-to-end test harness can drive
simulated participants into voice channels and verify that `chronicle-bot`
captures, consents, and uploads their audio correctly. Not part of the
production image; lives in its own repo and its own container so prod
builds stay clean.

Authoritative spec: `sessionhelper-hub/docs/modules/chronicle-feeder.md`.

Four identical containers run in the dev compose stack, each with its own
Discord bot token and voice clip:

| Feeder | Voice (Piper TTS) |
|---|---|
| moe | `en_US-lessac-medium` |
| larry | `en_US-ryan-medium` |
| curly | `en_US-kusal-medium` |
| gygax | `en_GB-alan-medium` |

See the canonical compose file at
[`sessionhelper-hub/infra/dev-compose.yml`](https://github.com/sessionhelper/sessionhelper-hub/blob/main/infra/dev-compose.yml)
for how they're wired up.

State machine
-------------

```
Idle  ──/join──▶  Joined  ──/play──▶  Playing
  ▲                  │                    │
  │                  │                    │
  └───/leave─────────┴────/stop (or EOF)──┘
```

The feeder exposes the state via `GET /health` (`in_voice`, `playing`).
Between `/join` and `/play` the feeder is **silent** — no voice frames are
transmitted. `chronicle-bot`'s stabilization gate must be opened by a real
`/play` from each feeder; there is no silence loop anymore.

Control API
-----------

Loopback-only axum server bound to `127.0.0.1:$CONTROL_PORT`. JSON bodies
throughout; errors come back as `{ "error": "<reason>" }` with a status
chosen to match the spec.

| Method | Path | Body | Effect | Errors |
|---|---|---|---|---|
| GET | `/health` | — | `{ name, user_id, in_voice, playing }` | — |
| POST | `/join` | `{ "guild_id": u64, "channel_id": u64 }` | connect | 409 `already joined; leave first` |
| POST | `/play` | — | start `AUDIO_FILE` | 400 `not in voice channel`; 409 `already playing`; 404 `audio file missing` |
| POST | `/stop` | — | stop track (no-op if not playing) | — |
| POST | `/leave` | — | leave (implicit stop; no-op if not joined) | — |

Harness-driven test flow
------------------------

Feeders no longer participate in consent via a bypass list. Test runners
drive the full lifecycle through `chronicle-bot`'s harness endpoints
(`POST /enrol`, `POST /consent`, `POST /license`) with feeder `user_id`s as
the test-user identities. From the bot's perspective the feeders are
indistinguishable from human participants; that's the point.

A typical test sequence per feeder:

1. `chronicle-feeder POST /join` → voice connects.
2. `chronicle-bot   POST /harness/enrol` → feeder is a participant.
3. `chronicle-bot   POST /harness/consent` → consent recorded.
4. `chronicle-feeder POST /play` → audio flows.
5. Harness waits on `chronicle-bot`'s stabilization gate.
6. `chronicle-feeder POST /stop` → audio ends.
7. `chronicle-feeder POST /leave` → voice disconnects.

Configuration
-------------

All via env vars:

| Var | Required | Default | Purpose |
|---|---|---|---|
| `DISCORD_TOKEN` | yes | — | Bot token |
| `FEEDER_NAME` | no | `feeder` | Short label for logs |
| `AUDIO_FILE` | yes | — | Absolute path to OGG Opus file |
| `CONTROL_PORT` | no | `8003` | Loopback port |
| `CONTROL_BIND` | no | `127.0.0.1` | Bind address (compose sets `0.0.0.0`, host safety via port mapping) |
| `RUST_LOG` | no | `info,serenity=warn,songbird=warn` | Tracing filter |

Audio input
-----------

`AUDIO_FILE` must be a pre-encoded OGG Opus stream. Songbird's passthrough
path forwards packets unchanged; no runtime transcoding. The feeder logs
a WARN at startup if the file is anything other than OGG Opus @ 48 kHz,
but still attempts playback (songbird will fall back to decode+reencode).

Regenerate the clips with:

```bash
./assets/generate_voices.sh              # Piper TTS → WAV
./scripts/encode-opus.sh --dir assets/ assets/
```

Running one locally
-------------------

```bash
cargo build --release

DISCORD_TOKEN=... \
FEEDER_NAME=moe \
AUDIO_FILE=$PWD/assets/moe.ogg \
CONTROL_PORT=8003 \
./target/release/chronicle-feeder

# In another terminal:
curl -sS -X POST http://127.0.0.1:8003/join \
  -H 'content-type: application/json' \
  -d '{"guild_id": 123, "channel_id": 456}'
curl -sS -X POST http://127.0.0.1:8003/play
curl -sS http://127.0.0.1:8003/health
curl -sS -X POST http://127.0.0.1:8003/stop
curl -sS -X POST http://127.0.0.1:8003/leave
```

Related repos
-------------

- [`chronicle-bot`](https://github.com/sessionhelper/chronicle-bot) — the consent-first voice capture bot this harness tests.
- [`sessionhelper-hub`](https://github.com/sessionhelper/sessionhelper-hub) — org-wide conventions, architecture, canonical compose file, module specs.
