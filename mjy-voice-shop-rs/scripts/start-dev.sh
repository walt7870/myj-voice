#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

mkdir -p .run logs

FOREGROUND=0
if [[ "${1:-}" == "--foreground" ]]; then
  FOREGROUND=1
fi

PORT="$(awk -F= '/^PORT=/{print $2}' .env 2>/dev/null | tail -n 1 | tr -d '"' || true)"
PORT="${PORT:-8787}"
PID_FILE=".run/dev.pid"
LOG_FILE="logs/dev.log"
SCREEN_SESSION="mjy_voice_shop_dev"

if [[ "$FOREGROUND" == "0" && -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  echo "Already running: pid=$(cat "$PID_FILE")"
  echo "Frontend: http://127.0.0.1:${PORT}/"
  echo "Backend:  http://127.0.0.1:${PORT}/api/health"
  exit 0
fi

if lsof -iTCP:"$PORT" -sTCP:LISTEN -n -P >/dev/null 2>&1; then
  echo "Port ${PORT} is already in use. Run scripts/status-dev.sh to inspect it." >&2
  exit 1
fi

echo "Building backend..."
CARGO_HTTP_PROXY="${CARGO_HTTP_PROXY:-http://127.0.0.1:17891}" cargo build >>"$LOG_FILE" 2>&1

if [[ "$FOREGROUND" == "1" ]]; then
  echo "Starting in foreground..."
  echo "Frontend: http://127.0.0.1:${PORT}/"
  echo "Backend:  http://127.0.0.1:${PORT}/api/health"
  exec ./target/debug/mjy-voice-shop-rs
fi

echo "Starting backend and frontend static server..."
screen -S "$SCREEN_SESSION" -X quit >/dev/null 2>&1 || true
ABS_LOG_FILE="${ROOT}/${LOG_FILE}"
screen -dmS "$SCREEN_SESSION" bash -lc "cd \"$ROOT\" && RUST_LOG=\"${RUST_LOG:-info}\" exec ./target/debug/mjy-voice-shop-rs >>\"$ABS_LOG_FILE\" 2>&1"
PID=""

for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:${PORT}/api/health" >/dev/null 2>&1; then
    if command -v lsof >/dev/null 2>&1; then
      PID="$(lsof -tiTCP:"$PORT" -sTCP:LISTEN -n -P | head -n 1 || true)"
      [[ -n "$PID" ]] && echo "$PID" > "$PID_FILE"
    fi
    echo "Started: pid=${PID}"
    echo "Frontend: http://127.0.0.1:${PORT}/"
    echo "Backend:  http://127.0.0.1:${PORT}/api/health"
    echo "Log:      ${ROOT}/${LOG_FILE}"
    exit 0
  fi
  if [[ -n "${PID:-}" ]] && ! kill -0 "$PID" 2>/dev/null; then
    echo "Service exited during startup. Recent log:" >&2
    tail -n 80 "$LOG_FILE" >&2 || true
    rm -f "$PID_FILE"
    exit 1
  fi
  sleep 0.5
done

echo "Service did not become healthy in time. Recent log:" >&2
tail -n 80 "$LOG_FILE" >&2 || true
exit 1
