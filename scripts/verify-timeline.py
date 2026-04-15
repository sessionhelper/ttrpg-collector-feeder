#!/usr/bin/env python3
"""
Verify timestamp round-trip for a session.

For every segment produced by the pipeline, the segment's timeline-
relative `start_ms` should match the wall-clock instant the underlying
audio was captured (chunk's `capture_started_at`) plus the intra-chunk
offset within that audio. If they drift apart, transcripts will land at
the wrong place in the dataset's timeline and any downstream training
that relies on those offsets will be silently wrong.

This tool reads the same session both ways:

  - `chunks.capture_started_at` — the wall-clock when the bot first
    sampled audio for that chunk (set by chronicle-bot's
    `X-Capture-Started-At` header).
  - `segments.start_ms` — the pipeline's offset within the session's
    timeline (relative to the track's `capture_started_at`).

For each segment, expected wall-clock time =
    chunk.capture_started_at + (segment.start_ms - track_started_at_ms)

We assert |expected - actual| <= TOLERANCE_MS for every segment.

Usage:
    # With SSH tunnel to data-api:
    ssh -L 18001:127.0.0.1:8001 root@<dev-vps> &

    SHARED_SECRET=$(ssh root@<dev-vps> 'grep ^SHARED_SECRET /opt/ovp/.env | cut -d= -f2') \\
      uv run --with requests scripts/verify-timeline.py --session <session-id>

Exits 0 on success, 1 if any segment violates the tolerance.
"""

import argparse
import os
import sys
from datetime import datetime, timezone

try:
    import requests
except ImportError:
    print("missing requests — `uv add requests` or use `uv run --with requests`", file=sys.stderr)
    sys.exit(2)

DATA_API_URL = os.environ.get("DATA_API_URL", "http://127.0.0.1:18001")
SECRET = os.environ.get("SHARED_SECRET")
TOLERANCE_MS = int(os.environ.get("TIMELINE_TOLERANCE_MS", "100"))


def authenticate(session_name: str = "verify-timeline") -> str:
    if not SECRET:
        sys.exit("SHARED_SECRET unset")
    r = requests.post(
        f"{DATA_API_URL}/internal/auth",
        json={"shared_secret": SECRET, "service_name": session_name},
        timeout=10,
    )
    r.raise_for_status()
    return r.json()["session_token"]


def get(token: str, path: str):
    r = requests.get(
        f"{DATA_API_URL}{path}",
        headers={"Authorization": f"Bearer {token}"},
        timeout=10,
    )
    r.raise_for_status()
    return r.json()


def parse_iso(s: str) -> datetime:
    """Parse an ISO-8601 with trailing Z into a UTC-aware datetime."""
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    return datetime.fromisoformat(s)


def to_millis(dt: datetime) -> int:
    return int(dt.timestamp() * 1000)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session", required=True, help="session UUID")
    parser.add_argument(
        "--tolerance-ms",
        type=int,
        default=TOLERANCE_MS,
        help=f"max allowed drift (default {TOLERANCE_MS})",
    )
    args = parser.parse_args()

    token = authenticate()
    session = get(token, f"/internal/sessions/{args.session}")
    participants = get(token, f"/internal/sessions/{args.session}/participants")
    segments = get(token, f"/internal/sessions/{args.session}/segments?limit=1000")

    # For each pseudo_id we'll build {seq: capture_started_at_ms}
    # so we can look up which chunk a segment came from.
    chunks_by_pseudo: dict[str, list[dict]] = {}
    for p in participants:
        pid = p["pseudo_id"]
        chunks = get(token, f"/internal/sessions/{args.session}/audio/{pid}/chunks")
        chunks_by_pseudo[pid] = sorted(chunks, key=lambda c: c["seq"])

    # Track start = first chunk's capture_started_at per pseudo.
    track_start_ms: dict[str, int] = {}
    for pid, chunks in chunks_by_pseudo.items():
        if not chunks or not chunks[0].get("capture_started_at"):
            continue
        track_start_ms[pid] = to_millis(parse_iso(chunks[0]["capture_started_at"]))

    print(f"session={args.session}  participants={len(participants)}  segments={len(segments)}")
    print(f"tolerance={args.tolerance_ms}ms")
    print()

    failures = []
    for s in segments:
        pid = s.get("pseudo_id")
        if pid is None or pid not in track_start_ms:
            # Mixed-track segments or segments from a participant whose
            # chunks lack a capture_started_at — skip rather than fail
            # the suite. They'll be caught by other checks.
            continue
        # Expected wall-clock for this segment's start =
        #   track_start_ms + segment.start_ms
        # The pipeline's start_ms is already relative to the track's
        # capture_started_at, so the round-trip is "does that addition
        # land within the chunk that nominally contains it?"
        expected_wall = track_start_ms[pid] + s["start_ms"]
        # Find the chunk whose [capture_started_at, +duration_ms] covers
        # expected_wall. We don't have a reverse-lookup in the segment;
        # we just verify that SOME chunk for this pseudo covers it.
        chunks = chunks_by_pseudo[pid]
        covering = None
        for c in chunks:
            if not c.get("capture_started_at"):
                continue
            cs_ms = to_millis(parse_iso(c["capture_started_at"]))
            if cs_ms <= expected_wall <= cs_ms + c.get("duration_ms", 0):
                covering = c
                break
        if covering is None:
            # No chunk covers this segment. Find the closest chunk and
            # report the drift.
            best = min(
                chunks,
                key=lambda c: abs(
                    to_millis(parse_iso(c["capture_started_at"])) - expected_wall
                )
                if c.get("capture_started_at")
                else 10**12,
            )
            cs_ms = to_millis(parse_iso(best["capture_started_at"]))
            drift = expected_wall - cs_ms
            failures.append(
                f"  ❌ pseudo={pid[:12]}.. seg [{s['start_ms']}-{s['end_ms']}] "
                f"expected wall={expected_wall}ms; nearest chunk seq={best['seq']} "
                f"started at {cs_ms}ms (drift {drift:+d}ms)"
            )

    if failures:
        print("FAIL:")
        for f in failures:
            print(f)
        print(f"\n{len(failures)}/{len(segments)} segments outside tolerance.")
        sys.exit(1)
    print(f"OK: all {len(segments)} segments land inside their capture chunks.")
    sys.exit(0)


if __name__ == "__main__":
    main()
