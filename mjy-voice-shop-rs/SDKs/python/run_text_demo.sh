#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
DEVICE_ID="${DEVICE_ID:-DOLL-0001}"
DEVICE_SECRET="${DEVICE_SECRET:-}"
TEXT="${TEXT:-你好，设备端测试一下播报}"
OUTPUT="${OUTPUT:-/tmp/mjy-python-device-reply.mp3}"
PLAY="${PLAY:-0}"
IN_FORMAT="${IN_FORMAT:-mp3}"
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

if [ ! -x "$SCRIPT_DIR/.venv/bin/python" ]; then
  "$SCRIPT_DIR/setup.sh"
fi

ARGS=(
  "$SCRIPT_DIR/.venv/bin/python" "$SCRIPT_DIR/device_client.py"
  --base-url "$BASE_URL" \
  --device-id "$DEVICE_ID" \
  --text "$TEXT" \
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
fi

"${ARGS[@]}"

echo "Saved TTS ($OUT_FORMAT): $OUTPUT"
