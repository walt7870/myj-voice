#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-8787}"
DEVICE_ID="${DEVICE_ID:-DOLL-0001}"
DEVICE_SECRET="${DEVICE_SECRET:-}"
BASE_PATH="${BASE_PATH:-}"
PCM_FILE="${PCM_FILE:-/tmp/mjy-test.pcm}"
OUTPUT="${OUTPUT:-/tmp/mjy-cpp-device-pcm-reply.mp3}"
PLAY="${PLAY:-0}"
PLAY_CMD="${PLAY_CMD:-}"
IN_FORMAT="${IN_FORMAT:-pcm}"
IN_RATE="${IN_RATE:-16000}"
OUT_FORMAT="${OUT_FORMAT:-mp3}"
OUT_RATE="${OUT_RATE:-16000}"
output_dir="$(dirname -- "$OUTPUT")"
output_name="$(basename -- "$OUTPUT")"
if [[ "$output_name" == *.* && "$output_name" != .* ]]; then
  output_name="${output_name%.*}"
fi
output_suffix="$OUT_FORMAT"
[[ "$OUT_FORMAT" == "opus" ]] && output_suffix="opuspack"
OUTPUT="${output_dir}/${output_name}.${output_suffix}"

if [ ! -x "$SCRIPT_DIR/device_client" ]; then
  "$SCRIPT_DIR/build.sh"
fi

if [ ! -f "$PCM_FILE" ]; then
  python3 "$PROJECT_ROOT/SDKs/python/generate_test_pcm.py" "$PCM_FILE"
fi

ARGS=(
  "$SCRIPT_DIR/device_client"
  --host "$HOST" \
  --port "$PORT" \
  --device-id "$DEVICE_ID" \
  --base-path "$BASE_PATH" \
  --audio "$PCM_FILE" \
  --in-format "$IN_FORMAT" \
  --in-rate "$IN_RATE" \
  --out-format "$OUT_FORMAT" \
  --out-rate "$OUT_RATE" \
  --output "$OUTPUT"
)

if [ -n "$DEVICE_SECRET" ]; then
  ARGS+=(--device-secret "$DEVICE_SECRET")
fi

if [ "$PLAY" = "1" ]; then
  ARGS+=(--play)
  if [ -n "$PLAY_CMD" ]; then
    ARGS+=(--play-cmd "$PLAY_CMD")
  fi
fi

"${ARGS[@]}"

echo "Saved TTS ($OUT_FORMAT): $OUTPUT"
