#!/usr/bin/env python3
"""
Verify a captured session by running Whisper on the per-speaker audio.

Downloads per-speaker PCM chunks from chronicle-data-api, concatenates
them into per-speaker WAV files, runs Whisper on each, and prints the
transcript. A human reading the output can immediately tell whether the
audio is intact (intelligible English) or broken (garbled, silent, or
hallucination spam).

The tool does NOT use the pipeline's own transcription — it runs
Whisper independently as a verification oracle. If the pipeline says
"good" but this tool says "garbled", the pipeline is wrong.

Usage:
    # With an SSH tunnel to the dev VPS data-api:
    ssh -L 18001:127.0.0.1:8001 root@178.156.144.147 &

    uv run scripts/verify-capture.py --session <session-id>
    uv run scripts/verify-capture.py --session <session-id> --ground-truth ground-truth.json
    uv run scripts/verify-capture.py --latest --status transcribed

Environment:
    DATA_API_URL    Base URL for chronicle-data-api (default: http://localhost:18001)
    SHARED_SECRET   Cross-service shared secret (reads from VPS .env if not set)
    WHISPER_URL     Whisper server URL (default: http://localhost:8300)
    WHISPER_MODEL   Whisper model to use (default: Systran/faster-whisper-large-v3)
"""

import argparse
import io
import json
import os
import struct
import sys
import wave
from pathlib import Path

try:
    import requests
except ImportError:
    print("pip install requests  (or: uv add requests)", file=sys.stderr)
    sys.exit(1)


# --- Config ---

DATA_API_URL = os.environ.get("DATA_API_URL", "http://localhost:18001")
SHARED_SECRET = os.environ.get("SHARED_SECRET", "")
WHISPER_URL = os.environ.get("WHISPER_URL", "http://localhost:8300")
WHISPER_MODEL = os.environ.get("WHISPER_MODEL", "Systran/faster-whisper-large-v3")

# Audio format from chronicle-bot: s16le stereo 48kHz
SAMPLE_RATE = 48000
CHANNELS = 2
SAMPLE_WIDTH = 2  # 16-bit = 2 bytes


# --- Data API client ---

class DataApiClient:
    def __init__(self, base_url: str, shared_secret: str):
        self.base_url = base_url.rstrip("/")
        self.shared_secret = shared_secret
        self.token = None

    def auth(self):
        resp = requests.post(
            f"{self.base_url}/internal/auth",
            json={"shared_secret": self.shared_secret, "service_name": "verify-tool"},
        )
        resp.raise_for_status()
        self.token = resp.json()["session_token"]

    def _headers(self):
        if not self.token:
            self.auth()
        return {"Authorization": f"Bearer {self.token}"}

    def list_sessions(self, status: str = "transcribed"):
        resp = requests.get(
            f"{self.base_url}/internal/sessions",
            params={"status": status},
            headers=self._headers(),
        )
        resp.raise_for_status()
        return resp.json()

    def get_session(self, session_id: str):
        resp = requests.get(
            f"{self.base_url}/internal/sessions/{session_id}",
            headers=self._headers(),
        )
        resp.raise_for_status()
        return resp.json()

    def get_participants(self, session_id: str):
        resp = requests.get(
            f"{self.base_url}/internal/sessions/{session_id}/participants",
            headers=self._headers(),
        )
        resp.raise_for_status()
        return resp.json()

    def get_chunk(self, session_id: str, pseudo_id: str, seq: int) -> bytes:
        resp = requests.get(
            f"{self.base_url}/internal/sessions/{session_id}/audio/{pseudo_id}/chunk/{seq}",
            headers=self._headers(),
        )
        if resp.status_code == 404:
            return b""
        resp.raise_for_status()
        return resp.content

    def get_segments(self, session_id: str):
        resp = requests.get(
            f"{self.base_url}/internal/sessions/{session_id}/segments",
            headers=self._headers(),
        )
        resp.raise_for_status()
        return resp.json()


# --- Audio helpers ---

def pcm_to_wav(pcm_data: bytes, sample_rate: int = SAMPLE_RATE,
               channels: int = CHANNELS, sample_width: int = SAMPLE_WIDTH) -> bytes:
    """Wrap raw PCM bytes in a WAV header."""
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(channels)
        w.setsampwidth(sample_width)
        w.setframerate(sample_rate)
        w.writeframes(pcm_data)
    return buf.getvalue()


def download_speaker_audio(api: DataApiClient, session_id: str, pseudo_id: str) -> bytes:
    """Download and concatenate all chunks for a speaker."""
    all_pcm = b""
    seq = 0
    while True:
        chunk = api.get_chunk(session_id, pseudo_id, seq)
        if not chunk:
            break
        all_pcm += chunk
        seq += 1
    return all_pcm


def pcm_duration(pcm_data: bytes) -> float:
    """Duration in seconds of raw s16le stereo 48kHz PCM."""
    frames = len(pcm_data) // (CHANNELS * SAMPLE_WIDTH)
    return frames / SAMPLE_RATE


def detect_silence_gaps(pcm_data: bytes, window_ms: int = 500,
                        threshold: float = 100.0) -> list[tuple[float, float]]:
    """Find stretches of near-silence in raw PCM. Returns (start_s, end_s) pairs."""
    frame_size = CHANNELS * SAMPLE_WIDTH
    samples_per_window = int(SAMPLE_RATE * window_ms / 1000) * frame_size
    gaps = []
    gap_start = None

    for offset in range(0, len(pcm_data), samples_per_window):
        window = pcm_data[offset:offset + samples_per_window]
        if len(window) < frame_size:
            break
        # RMS of the window (treating as s16le samples)
        n_samples = len(window) // 2
        samples = struct.unpack(f"<{n_samples}h", window[:n_samples * 2])
        rms = (sum(s * s for s in samples) / n_samples) ** 0.5

        t = offset / (SAMPLE_RATE * frame_size / SAMPLE_RATE)  # time in seconds
        t = offset / frame_size / SAMPLE_RATE

        if rms < threshold:
            if gap_start is None:
                gap_start = t
        else:
            if gap_start is not None:
                gaps.append((gap_start, t))
                gap_start = None

    if gap_start is not None:
        gaps.append((gap_start, pcm_duration(pcm_data)))

    return gaps


# --- Whisper ---

def transcribe(wav_data: bytes, whisper_url: str = WHISPER_URL,
               model: str = WHISPER_MODEL) -> dict:
    """Send a WAV file to the Whisper server and return the response."""
    resp = requests.post(
        f"{whisper_url}/v1/audio/transcriptions",
        files={"file": ("audio.wav", wav_data, "audio/wav")},
        data={
            "model": model,
            "response_format": "verbose_json",
            "language": "en",
        },
    )
    resp.raise_for_status()
    return resp.json()


# --- Verification ---

def verify_speaker(api: DataApiClient, session_id: str, pseudo_id: str,
                   display_name: str | None, ground_truth: str | None = None) -> dict:
    """Download, transcribe, and verify one speaker's audio."""
    pcm = download_speaker_audio(api, session_id, pseudo_id)

    if not pcm:
        return {
            "pseudo_id": pseudo_id,
            "display_name": display_name,
            "status": "NO_AUDIO",
            "detail": "No chunks found for this speaker",
        }

    duration = pcm_duration(pcm)
    chunk_count = len(pcm) // (2 * 1024 * 1024) + (1 if len(pcm) % (2 * 1024 * 1024) else 0)
    wav = pcm_to_wav(pcm)

    # Silence detection
    gaps = detect_silence_gaps(pcm)
    total_silence = sum(end - start for start, end in gaps)

    # Whisper transcription
    try:
        whisper_result = transcribe(wav)
        transcript = whisper_result.get("text", "").strip()
    except Exception as e:
        return {
            "pseudo_id": pseudo_id,
            "display_name": display_name,
            "duration_s": round(duration, 1),
            "chunks": chunk_count,
            "status": "WHISPER_ERROR",
            "detail": str(e),
        }

    # Basic quality signals
    words = transcript.split()
    word_count = len(words)

    # Hallucination detection: repetition ratio
    if word_count > 5:
        from collections import Counter
        word_freq = Counter(words)
        most_common_pct = word_freq.most_common(1)[0][1] / word_count
    else:
        most_common_pct = 0.0

    # Build result
    result = {
        "pseudo_id": pseudo_id,
        "display_name": display_name,
        "duration_s": round(duration, 1),
        "chunks": chunk_count,
        "silence_gaps": len(gaps),
        "silence_total_s": round(total_silence, 1),
        "word_count": word_count,
        "repetition_ratio": round(most_common_pct, 2),
        "transcript": transcript,
    }

    # Verdict
    if word_count == 0:
        result["status"] = "FAIL"
        result["detail"] = "Whisper produced no text — audio is likely silent or noise"
    elif most_common_pct > 0.4 and word_count > 10:
        result["status"] = "FAIL"
        result["detail"] = f"Hallucination detected — single word is {most_common_pct:.0%} of transcript"
    elif total_silence > duration * 0.5 and duration > 5:
        result["status"] = "WARN"
        result["detail"] = f"{total_silence:.1f}s of silence in {duration:.1f}s of audio ({total_silence/duration:.0%})"
    else:
        result["status"] = "PASS"
        result["detail"] = "Transcript looks intelligible"

    # Ground truth comparison (simple word overlap for now)
    if ground_truth:
        gt_words = set(ground_truth.lower().split())
        tx_words = set(transcript.lower().split())
        if gt_words:
            overlap = len(gt_words & tx_words) / len(gt_words)
            result["ground_truth_overlap"] = round(overlap, 2)
            if overlap < 0.3:
                result["status"] = "FAIL"
                result["detail"] = f"Ground truth overlap {overlap:.0%} — transcript doesn't match expected content"

    return result


def format_result(result: dict) -> str:
    """Human-readable output for one speaker."""
    lines = []
    name = result.get("display_name") or result["pseudo_id"]
    lines.append(f"Speaker: {name} (pseudo_id: {result['pseudo_id']})")

    if result["status"] == "NO_AUDIO":
        lines.append(f"  {result['detail']}")
        return "\n".join(lines)

    if "duration_s" in result:
        lines.append(f"  Audio: {result['duration_s']}s across {result.get('chunks', '?')} chunks")

    if "silence_gaps" in result:
        lines.append(f"  Silence: {result['silence_total_s']}s across {result['silence_gaps']} gaps")

    status = result["status"]
    icon = {"PASS": "+", "WARN": "~", "FAIL": "X"}.get(status, "?")
    if "WHISPER_ERROR" in status:
        icon = "!"

    lines.append(f"  [{icon}] {status}: {result.get('detail', '')}")

    if "transcript" in result:
        tx = result["transcript"]
        if len(tx) > 300:
            tx = tx[:300] + "..."
        lines.append(f"  Transcript: \"{tx}\"")

    if "ground_truth_overlap" in result:
        lines.append(f"  Ground truth overlap: {result['ground_truth_overlap']:.0%}")

    if result.get("repetition_ratio", 0) > 0.2:
        lines.append(f"  Repetition ratio: {result['repetition_ratio']:.0%} (high = likely hallucination)")

    return "\n".join(lines)


# --- Main ---

def verify_local_wavs(wav_dir: Path, ground_truths: dict, args) -> list[dict]:
    """Verify local WAV files directly (bypass data-api). For testing
    the verification pipeline itself against known-good test data."""
    wav_files = sorted(wav_dir.glob("*.wav"))
    if not wav_files:
        print(f"No .wav files found in {wav_dir}", file=sys.stderr)
        return []

    results = []
    for wav_path in wav_files:
        if wav_path.name == "mixed_preview.wav":
            continue
        pseudo_id = wav_path.stem
        display_name = pseudo_id
        gt = ground_truths.get(pseudo_id)

        print(f"\n  Verifying {pseudo_id} ({wav_path.name})...", file=sys.stderr)

        wav_data = wav_path.read_bytes()
        try:
            whisper_result = transcribe(wav_data)
            transcript = whisper_result.get("text", "").strip()
        except Exception as e:
            results.append({
                "pseudo_id": pseudo_id,
                "display_name": display_name,
                "status": "WHISPER_ERROR",
                "detail": str(e),
            })
            continue

        words = transcript.split()
        word_count = len(words)

        if word_count > 5:
            from collections import Counter
            word_freq = Counter(words)
            most_common_pct = word_freq.most_common(1)[0][1] / word_count
        else:
            most_common_pct = 0.0

        result = {
            "pseudo_id": pseudo_id,
            "display_name": display_name,
            "word_count": word_count,
            "repetition_ratio": round(most_common_pct, 2),
            "transcript": transcript,
        }

        if word_count == 0:
            result["status"] = "FAIL"
            result["detail"] = "Whisper produced no text"
        elif most_common_pct > 0.4 and word_count > 10:
            result["status"] = "FAIL"
            result["detail"] = f"Hallucination detected — {most_common_pct:.0%} repetition"
        else:
            result["status"] = "PASS"
            result["detail"] = "Transcript looks intelligible"

        if gt:
            gt_words = set(gt.lower().split())
            tx_words = set(transcript.lower().split())
            if gt_words:
                overlap = len(gt_words & tx_words) / len(gt_words)
                result["ground_truth_overlap"] = round(overlap, 2)
                if overlap < 0.3:
                    result["status"] = "FAIL"
                    result["detail"] = f"Ground truth overlap {overlap:.0%}"

        results.append(result)

    return results


def main():
    parser = argparse.ArgumentParser(description="Verify captured session audio via Whisper")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--session", help="Session ID to verify")
    group.add_argument("--latest", action="store_true", help="Verify the most recent session")
    group.add_argument("--local", type=Path, help="Directory of local WAV files to verify (bypass data-api)")
    parser.add_argument("--status", default="transcribed", help="Session status filter for --latest")
    parser.add_argument("--ground-truth", type=Path, help="JSON file with ground truth transcripts keyed by pseudo_id")
    parser.add_argument("--save-wav", type=Path, help="Directory to save per-speaker WAV files for manual inspection")
    parser.add_argument("--json", action="store_true", help="Output raw JSON instead of human-readable text")
    args = parser.parse_args()

    # Load ground truth if provided
    ground_truths = {}
    if args.ground_truth:
        gt_data = json.loads(args.ground_truth.read_text())
        # Support both flat {pseudo_id: text} and structured {speakers: {LABEL: {pseudo_id, ...}}, segments: [...]}
        if "segments" in gt_data and "speakers" in gt_data:
            # Structured ground truth: concatenate segment text per speaker pseudo_id
            speaker_map = {v["pseudo_id"]: k for k, v in gt_data["speakers"].items()}
            for seg in gt_data["segments"]:
                pid = gt_data["speakers"].get(seg.get("speaker"), {}).get("pseudo_id")
                if pid:
                    ground_truths.setdefault(pid, "")
                    ground_truths[pid] += " " + seg.get("text", "")
            ground_truths = {k: v.strip() for k, v in ground_truths.items()}
        else:
            ground_truths = gt_data

    # --- Local WAV mode ---
    if args.local:
        print(f"=== Local verification: {args.local} ===", file=sys.stderr)
        results = verify_local_wavs(args.local, ground_truths, args)

        if args.json:
            print(json.dumps(results, indent=2))
        else:
            print()
            print(f"=== Verification: {args.local} ===")
            print(f"    {len(results)} speakers verified")
            print()
            for result in results:
                print(format_result(result))
                print()

            pass_count = sum(1 for r in results if r.get("status") == "PASS")
            fail_count = sum(1 for r in results if r.get("status") == "FAIL")
            warn_count = sum(1 for r in results if r.get("status") == "WARN")
            print(f"Summary: {pass_count} PASS, {fail_count} FAIL, {warn_count} WARN")
            if fail_count > 0:
                print("VERDICT: ISSUES DETECTED")
                sys.exit(1)
            else:
                print("VERDICT: ALL SPEAKERS PASS")
        return

    # --- Data-API mode ---

    # Resolve shared secret
    secret = SHARED_SECRET
    if not secret:
        print("SHARED_SECRET not set. Trying to read from VPS .env via SSH...", file=sys.stderr)
        import subprocess
        result = subprocess.run(
            ["ssh", "-o", "BatchMode=yes", "root@178.156.144.147",
             "grep ^SHARED_SECRET /opt/ovp/.env"],
            capture_output=True, text=True
        )
        if result.returncode == 0:
            secret = result.stdout.strip().split("=", 1)[1]
        else:
            print("ERROR: could not resolve SHARED_SECRET", file=sys.stderr)
            sys.exit(1)

    api = DataApiClient(DATA_API_URL, secret)

    # Resolve session ID
    if args.latest:
        sessions = api.list_sessions(args.status)
        if not sessions:
            print(f"No sessions with status={args.status}", file=sys.stderr)
            sys.exit(1)
        session = sessions[0]
        session_id = session["id"]
    else:
        session_id = args.session
        session = api.get_session(session_id)

    print(f"=== Session {session_id} ===", file=sys.stderr)
    print(f"    Status: {session['status']}", file=sys.stderr)
    print(f"    Created: {session['created_at'][:19]}", file=sys.stderr)

    # Get participants
    participants = api.get_participants(session_id)
    consented = [p for p in participants if p.get("consent_scope") == "full"]
    print(f"    Participants: {len(participants)} total, {len(consented)} consented", file=sys.stderr)

    # Verify each consented speaker
    results = []
    for p in consented:
        pseudo_id = p.get("pseudo_id") or p.get("user_pseudo_id") or p["id"]
        display = p.get("display_name") or p.get("character_name")
        gt = ground_truths.get(pseudo_id)

        print(f"\n  Verifying {display or pseudo_id}...", file=sys.stderr)
        result = verify_speaker(api, session_id, pseudo_id, display, gt)
        results.append(result)

        if args.save_wav and result.get("status") != "NO_AUDIO":
            wav_dir = args.save_wav
            wav_dir.mkdir(parents=True, exist_ok=True)
            pcm = download_speaker_audio(api, session_id, pseudo_id)
            wav_path = wav_dir / f"{pseudo_id}.wav"
            wav_path.write_bytes(pcm_to_wav(pcm))
            print(f"    Saved {wav_path}", file=sys.stderr)

    # Output
    if args.json:
        print(json.dumps(results, indent=2))
    else:
        print()
        print(f"=== Verification: {session_id} ===")
        print(f"    {len(results)} speakers verified")
        print()
        for result in results:
            print(format_result(result))
            print()

        # Summary
        pass_count = sum(1 for r in results if r.get("status") == "PASS")
        fail_count = sum(1 for r in results if r.get("status") == "FAIL")
        warn_count = sum(1 for r in results if r.get("status") == "WARN")
        no_audio = sum(1 for r in results if r.get("status") == "NO_AUDIO")

        print(f"Summary: {pass_count} PASS, {fail_count} FAIL, {warn_count} WARN, {no_audio} NO_AUDIO")
        if fail_count > 0:
            print("VERDICT: RECORDING HAS ISSUES — review transcripts above")
            sys.exit(1)
        elif warn_count > 0:
            print("VERDICT: RECORDING HAS WARNINGS — review transcripts above")
        else:
            print("VERDICT: ALL SPEAKERS PASS")


if __name__ == "__main__":
    main()
