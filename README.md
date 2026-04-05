# ttrpg-collector-feeder

E2E test harness feeder bot for [`ttrpg-collector`](https://github.com/sessionhelper/ttrpg-collector). Dev-only.

This is a minimal Discord bot that joins a voice channel and plays a
pre-recorded WAV file on command. It exists so the end-to-end test harness
can drive simulated participants into voice channels and verify that the
collector captures, consents, and uploads their audio correctly. It's not
part of the production collector image — it lives in its own repo and its
own container so prod builds stay clean.

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

## Control API

The feeder exposes a tiny axum server bound to `127.0.0.1:$CONTROL_PORT`.
An external test runner POSTs to it to orchestrate multi-bot scenarios.

| Method | Path | Body | Effect |
|---|---|---|---|
| GET | `/health` | — | `{ name, user_id, in_voice, playing }` |
| POST | `/join` | `{ "guild_id": u64, "channel_id": u64 }` | connect to voice channel |
| POST | `/play` | — | start playing `AUDIO_FILE` |
| POST | `/stop` | — | stop the current track |
| POST | `/leave` | — | leave the voice channel |

Errors come back as plain `500 Internal Server Error` with a message body —
the control server is loopback-only, so leaking error strings is fine and
useful for debugging.

## Configuration

All via env vars:

| Var | Purpose |
|---|---|
| `DISCORD_TOKEN` | Discord bot token for this feeder instance |
| `FEEDER_NAME` | Short name for structured logs (e.g. `moe`) |
| `AUDIO_FILE` | Absolute path to the WAV to play on `/play` |
| `CONTROL_PORT` | Loopback port for the control server (default `8003`) |

## Running one locally

You need a Discord bot application with voice intent enabled, invited to a
test guild where you can join a voice channel.

```bash
# Build
cargo build --release

# Run
DISCORD_TOKEN=... \
FEEDER_NAME=moe \
AUDIO_FILE=$PWD/assets/moe.wav \
CONTROL_PORT=8003 \
./target/release/ttrpg-collector-feeder

# In another terminal, drive it:
curl -sS -X POST http://127.0.0.1:8003/join \
  -H 'content-type: application/json' \
  -d '{"guild_id": 123, "channel_id": 456}'
curl -sS -X POST http://127.0.0.1:8003/play
curl -sS http://127.0.0.1:8003/health
curl -sS -X POST http://127.0.0.1:8003/stop
curl -sS -X POST http://127.0.0.1:8003/leave
```

## Regenerating the voice clips

The four WAVs in `assets/` were generated with [Piper TTS](https://github.com/rhasspy/piper).
The script is checked in:

```bash
# Install piper (one-time)
uv tool install piper-tts

# Regenerate
./assets/generate_voices.sh
```

Models are cached in `assets/piper-models/` (gitignored, ~60 MB each).

## Related repos

- [`ttrpg-collector`](https://github.com/sessionhelper/ttrpg-collector) — the consent-first voice capture bot this harness tests.
- [`sessionhelper-hub`](https://github.com/sessionhelper/sessionhelper-hub) — org-wide conventions, architecture, canonical compose file.
