#!/usr/bin/env bash
# Pre-encode WAVs to OGG Opus for feeder passthrough.
#
# The feeder relies on songbird's Opus passthrough path: if the input is
# already valid 48kHz / 20ms-frame OPUS, songbird forwards packets to
# Discord unchanged instead of decode→mix→reencode. Re-encoding through
# songbird's mixer mangled TTS and long-form audio; passthrough fixes it.
#
# Usage:
#   scripts/encode-opus.sh <input.wav> [output.ogg]
#   scripts/encode-opus.sh --dir <src_dir> <dst_dir>
#
# Requires: ffmpeg with libopus.

set -euo pipefail

encode_one() {
    local src="$1" dst="$2"
    ffmpeg -y -i "$src" \
        -c:a libopus -b:a 96k -ar 48000 -ac 2 \
        -frame_duration 20 -application voip -vbr on \
        -f ogg "$dst"
}

if [[ "${1:-}" == "--dir" ]]; then
    src_dir="$2"; dst_dir="$3"
    mkdir -p "$dst_dir"
    for f in "$src_dir"/*.wav; do
        [[ -e "$f" ]] || continue
        base="$(basename "$f" .wav)"
        encode_one "$f" "$dst_dir/$base.ogg"
    done
else
    src="$1"
    dst="${2:-${src%.wav}.ogg}"
    encode_one "$src" "$dst"
fi
