#!/usr/bin/env python3
"""
Inject a synthetic session directly into chronicle-data-api.

Bypasses Discord voice entirely. Takes a directory of per-speaker WAV
files and pushes them through the exact HTTP surface the bot uses during
a real capture: create session, upsert users, add participants, set
consent + license flags, upload PCM chunks, finalize. The resulting
session is indistinguishable (from the pipeline's perspective) from one
captured through Discord.

Why this exists: the feeder-over-Discord audio path is currently lossy
("raspy" captures from pre-encoded OGG Opus). Rather than keep fighting
that path, this tool gives us a deterministic way to load known-good
audio into the pipeline for worker/portal/transcription testing.

Usage:
    # SSH tunnel to dev VPS data-api:
    ssh -L 18001:127.0.0.1:8001 root@178.156.144.147 &

    # Inject a session from a directory of WAVs (sibling files per speaker):
    SHARED_SECRET=... uv run scripts/inject-session.py \\
        --audio-dir /path/to/wavs

    # Simulate real-time arrival (sleep chunk-duration between uploads):
    SHARED_SECRET=... uv run scripts/inject-session.py \\
        --audio-dir /path/to/wavs --realtime

    # Provide explicit metadata (pseudo_ids, display names, consent):
    SHARED_SECRET=... uv run scripts/inject-session.py \\
        --audio-dir /path/to/wavs --metadata meta.json

Metadata JSON shape (all fields optional):
    {
      "gygax_gm.wav": {
        "pseudo_id": "b8cbb85d58166b4a",
        "display_name": "Gygax",
        "consent_scope": "full",
        "no_llm_training": false,
        "no_public_release": false
      },
      ...
    }

Environment:
    DATA_API_URL    Base URL for chronicle-data-api (default: http://localhost:18001)
    SHARED_SECRET   Cross-service shared secret (required)
"""

import argparse
import hashlib
import io
import json
import os
import random
import subprocess
import sys
import time
import uuid
import wave
from datetime import datetime, timedelta, timezone
from pathlib import Path

try:
    import requests
except ImportError:
    print("pip install requests  (or: uv add requests)", file=sys.stderr)
    sys.exit(1)


# --- Config ---

DATA_API_URL = os.environ.get("DATA_API_URL", "http://localhost:18001")
SHARED_SECRET = os.environ.get("SHARED_SECRET", "")

# Match chronicle-bot's chunk & audio format exactly.
# See chronicle-bot/voice-capture/src/voice/receiver.rs:
#   const CHUNK_SIZE: usize = 2 * 1024 * 1024;
CHUNK_SIZE = 2 * 1024 * 1024
SAMPLE_RATE = 48000
CHANNELS = 2
SAMPLE_WIDTH = 2  # 16-bit = 2 bytes
BYTES_PER_SECOND = SAMPLE_RATE * CHANNELS * SAMPLE_WIDTH  # 192_000
CHUNK_DURATION_SECS = CHUNK_SIZE / BYTES_PER_SECOND  # ~10.92


# --- Data API client ---

class DataApiClient:
    def __init__(self, base_url: str, shared_secret: str):
        self.base_url = base_url.rstrip("/")
        self.shared_secret = shared_secret
        self.token = None

    def auth(self):
        resp = requests.post(
            f"{self.base_url}/internal/auth",
            json={"shared_secret": self.shared_secret, "service_name": "inject-tool"},
        )
        resp.raise_for_status()
        self.token = resp.json()["session_token"]

    def _headers(self, content_type: str = "application/json"):
        if not self.token:
            self.auth()
        return {"Authorization": f"Bearer {self.token}", "Content-Type": content_type}

    def create_session(self, session_id: str, guild_id: int, started_at: datetime,
                       game_system: str | None, campaign_name: str | None):
        # Match the bot's s3_prefix layout exactly — the chunk upload route
        # requires it and downstream services expect sessions/{guild}/{id}.
        body = {
            "id": session_id,
            "guild_id": guild_id,
            "started_at": started_at.isoformat().replace("+00:00", "Z"),
            "game_system": game_system,
            "campaign_name": campaign_name,
            "s3_prefix": f"sessions/{guild_id}/{session_id}",
        }
        resp = requests.post(
            f"{self.base_url}/internal/sessions",
            json=body, headers=self._headers(),
        )
        resp.raise_for_status()
        return resp.json()

    def upsert_user(self, pseudo_id: str):
        resp = requests.post(
            f"{self.base_url}/internal/users",
            json={"pseudo_id": pseudo_id},
            headers=self._headers(),
        )
        resp.raise_for_status()
        return resp.json()

    def record_display_name(self, pseudo_id: str, display_name: str):
        resp = requests.post(
            f"{self.base_url}/internal/users/{pseudo_id}/display_names",
            json={"display_name": display_name, "source": "bot"},
            headers=self._headers(),
        )
        resp.raise_for_status()

    def add_participant(self, session_id: str, pseudo_id: str):
        resp = requests.post(
            f"{self.base_url}/internal/sessions/{session_id}/participants",
            json={"pseudo_id": pseudo_id, "mid_session_join": False},
            headers=self._headers(),
        )
        resp.raise_for_status()
        return resp.json()

    def set_consent(self, participant_id: str, scope: str):
        resp = requests.patch(
            f"{self.base_url}/internal/participants/{participant_id}/consent",
            json={
                "consent_scope": scope,
                "consented_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            },
            headers=self._headers(),
        )
        resp.raise_for_status()

    def set_license(self, participant_id: str, no_llm_training: bool, no_public_release: bool):
        resp = requests.patch(
            f"{self.base_url}/internal/participants/{participant_id}/license",
            json={
                "no_llm_training": no_llm_training,
                "no_public_release": no_public_release,
            },
            headers=self._headers(),
        )
        resp.raise_for_status()

    def upload_chunk(self, session_id: str, pseudo_id: str, data: bytes,
                     capture_started_at: datetime, duration_ms: int,
                     client_chunk_id: str):
        headers = self._headers(content_type="application/octet-stream")
        headers["X-Capture-Started-At"] = (
            capture_started_at.isoformat().replace("+00:00", "Z")
        )
        headers["X-Duration-Ms"] = str(duration_ms)
        headers["X-Client-Chunk-Id"] = client_chunk_id
        resp = requests.post(
            f"{self.base_url}/internal/sessions/{session_id}/audio/{pseudo_id}/chunk",
            data=data,
            headers=headers,
        )
        resp.raise_for_status()

    def finalize_session(self, session_id: str, ended_at: datetime, participant_count: int):
        resp = requests.patch(
            f"{self.base_url}/internal/sessions/{session_id}",
            json={
                "ended_at": ended_at.isoformat().replace("+00:00", "Z"),
                "status": "uploaded",
                "participant_count": participant_count,
            },
            headers=self._headers(),
        )
        resp.raise_for_status()
        return resp.json()


# --- Audio helpers ---

def wav_to_pcm_48k_stereo(wav_path: Path) -> bytes:
    """Convert ANY input WAV to 48kHz stereo s16le PCM via ffmpeg.

    We shell out rather than rolling our own resampler — chronicle-bot's
    chunk format is strict (48000 Hz, 2 channels, 16-bit) and mismatches
    will land in the database and be silently wrong until someone plays
    them back at the wrong rate. ffmpeg is battle-tested; trust it.
    """
    result = subprocess.run(
        [
            "ffmpeg", "-y", "-loglevel", "error",
            "-i", str(wav_path),
            "-f", "s16le", "-ar", str(SAMPLE_RATE), "-ac", str(CHANNELS),
            "-",
        ],
        capture_output=True, check=True,
    )
    return result.stdout


def chunk_pcm(pcm: bytes, chunk_size: int = CHUNK_SIZE):
    """Yield (seq, chunk_bytes, chunk_duration_secs) for each chunk."""
    for i, start in enumerate(range(0, len(pcm), chunk_size)):
        chunk = pcm[start:start + chunk_size]
        duration = len(chunk) / BYTES_PER_SECOND
        yield i, chunk, duration


def fake_pseudo_id(stem: str) -> str:
    """Deterministic 24-char hex pseudo_id from a filename stem (matches
    production shape: hex(sha256(discord_id))[0:24])."""
    return hashlib.sha256(stem.encode()).hexdigest()[:24]


# --- Main ---

def inject(audio_dir: Path, metadata: dict, guild_id: int, game_system: str | None,
           campaign_name: str | None, realtime: bool, session_id: str | None) -> str:
    api = DataApiClient(DATA_API_URL, SHARED_SECRET)
    api.auth()

    session_id = session_id or str(uuid.uuid4())
    started_at = datetime.now(timezone.utc)

    wavs = sorted(audio_dir.glob("*.wav"))
    wavs = [w for w in wavs if w.name != "mixed_preview.wav"]
    if not wavs:
        print(f"no .wav files in {audio_dir}", file=sys.stderr)
        sys.exit(2)

    print(f"=== Injecting session {session_id} ===", file=sys.stderr)
    print(f"    audio_dir: {audio_dir}", file=sys.stderr)
    print(f"    speakers:  {len(wavs)}", file=sys.stderr)
    print(f"    realtime:  {realtime}", file=sys.stderr)

    api.create_session(session_id, guild_id, started_at, game_system, campaign_name)

    # Prepare each speaker: convert once up front so we know total duration
    # before we start streaming. Lets us report expected wall time under
    # --realtime.
    prepared = []
    for wav_path in wavs:
        meta = metadata.get(wav_path.name, {})
        pseudo_id = meta.get("pseudo_id") or fake_pseudo_id(wav_path.stem)
        display_name = meta.get("display_name") or wav_path.stem
        consent_scope = meta.get("consent_scope", "full")
        no_llm_training = bool(meta.get("no_llm_training", False))
        no_public_release = bool(meta.get("no_public_release", False))

        print(f"  converting {wav_path.name} -> 48kHz stereo s16le...", file=sys.stderr)
        pcm = wav_to_pcm_48k_stereo(wav_path)
        duration_secs = len(pcm) / BYTES_PER_SECOND

        api.upsert_user(pseudo_id)
        api.record_display_name(pseudo_id, display_name)
        participant = api.add_participant(session_id, pseudo_id)
        api.set_consent(participant["id"], consent_scope)
        api.set_license(participant["id"], no_llm_training, no_public_release)

        prepared.append({
            "path": wav_path,
            "pseudo_id": pseudo_id,
            "display_name": display_name,
            "pcm": pcm,
            "duration": duration_secs,
        })
        print(f"    pseudo_id={pseudo_id}  duration={duration_secs:.2f}s  pcm_bytes={len(pcm)}",
              file=sys.stderr)

    longest = max(p["duration"] for p in prepared)
    print(f"\n  longest speaker: {longest:.2f}s", file=sys.stderr)
    if realtime:
        print(f"  realtime mode: uploads will take ~{longest:.0f}s", file=sys.stderr)

    # Interleave uploads across speakers by chunk sequence. This more
    # closely mirrors what the bot does: during a real capture, the bot
    # is uploading chunks from all speakers concurrently, one per ~10.9s
    # of their audio. Under --realtime, we sleep between chunk rounds so
    # the worker's poll interval and any WS notifications fire at the
    # right cadence.
    chunk_plans = []
    for p in prepared:
        chunk_plans.append([(p["pseudo_id"], p["display_name"], c, b, d)
                            for c, b, d in chunk_pcm(p["pcm"])])

    max_chunks = max(len(plan) for plan in chunk_plans)
    t0 = time.monotonic()
    for round_idx in range(max_chunks):
        round_start = time.monotonic()
        round_duration = 0.0
        for plan in chunk_plans:
            if round_idx >= len(plan):
                continue
            pseudo_id, display_name, seq, chunk_bytes, dur = plan[round_idx]
            print(f"  [{round_idx+1}/{max_chunks}] {display_name} seq={seq} "
                  f"size={len(chunk_bytes)} dur={dur:.2f}s", file=sys.stderr)
            # Capture timestamp = session start + seq * chunk_duration. In
            # real captures the bot sets this per-chunk; here we synthesize it
            # so downstream timelining stays faithful.
            chunk_capture_started_at = started_at + timedelta(
                seconds=seq * CHUNK_DURATION_SECS
            )
            client_chunk_id = f"{pseudo_id}-{seq}"
            api.upload_chunk(
                session_id, pseudo_id, chunk_bytes,
                capture_started_at=chunk_capture_started_at,
                duration_ms=int(dur * 1000),
                client_chunk_id=client_chunk_id,
            )
            round_duration = max(round_duration, dur)

        if realtime and round_idx < max_chunks - 1:
            elapsed = time.monotonic() - round_start
            sleep_for = round_duration - elapsed
            if sleep_for > 0:
                print(f"    [realtime] sleeping {sleep_for:.2f}s to match audio cadence",
                      file=sys.stderr)
                time.sleep(sleep_for)

    total_elapsed = time.monotonic() - t0
    ended_at = datetime.now(timezone.utc)

    print(f"\n  finalizing session (upload wall time: {total_elapsed:.1f}s)", file=sys.stderr)
    api.finalize_session(session_id, ended_at, len(prepared))
    print(f"\n=== Session injected: {session_id} ===", file=sys.stderr)
    print(f"    status: uploaded", file=sys.stderr)
    print(f"    verify with: uv run scripts/verify-capture.py --session {session_id}",
          file=sys.stderr)

    return session_id


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--audio-dir", type=Path, required=True,
                        help="Directory of per-speaker WAV files")
    parser.add_argument("--metadata", type=Path,
                        help="Optional JSON file: filename -> {pseudo_id, display_name, consent_scope, ...}")
    parser.add_argument("--guild-id", type=int, default=None,
                        help="Fake Discord guild ID (default: random)")
    parser.add_argument("--game-system", type=str, default=None)
    parser.add_argument("--campaign-name", type=str, default=None)
    parser.add_argument("--session-id", type=str, default=None,
                        help="Explicit session UUID (default: generate)")
    parser.add_argument("--realtime", action="store_true",
                        help="Sleep chunk-duration between upload rounds to simulate live capture")
    args = parser.parse_args()

    if not SHARED_SECRET:
        print("SHARED_SECRET env var is required", file=sys.stderr)
        sys.exit(2)
    if not args.audio_dir.is_dir():
        print(f"audio-dir does not exist: {args.audio_dir}", file=sys.stderr)
        sys.exit(2)

    metadata = {}
    if args.metadata:
        metadata = json.loads(args.metadata.read_text())

    guild_id = args.guild_id if args.guild_id is not None else random.randint(10**17, 10**18 - 1)

    session_id = inject(
        audio_dir=args.audio_dir,
        metadata=metadata,
        guild_id=guild_id,
        game_system=args.game_system,
        campaign_name=args.campaign_name,
        realtime=args.realtime,
        session_id=args.session_id,
    )
    # Print the session ID on stdout so the tool composes cleanly in pipelines.
    print(session_id)


if __name__ == "__main__":
    main()
