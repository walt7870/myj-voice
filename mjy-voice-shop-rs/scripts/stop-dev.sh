#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PID_FILE=".run/dev.pid"
SCREEN_SESSION="mjy_voice_shop_dev"
PORT="$(awk -F= '/^PORT=/{print $2}' .env 2>/dev/null | tail -n 1 | tr -d '"' || true)"
PORT="${PORT:-8787}"

if command -v screen >/dev/null 2>&1; then
  screen -S "$SCREEN_SESSION" -X quit >/dev/null 2>&1 || true
fi

if [[ ! -f "$PID_FILE" ]]; then
  PORT_PID="$(lsof -tiTCP:"$PORT" -sTCP:LISTEN -n -P | head -n 1 || true)"
  if [[ -n "$PORT_PID" ]]; then
    kill "$PORT_PID" 2>/dev/null || true
    echo "Stopped port ${PORT}: pid=${PORT_PID}"
  else
    echo "No pid file found. Nothing to stop."
  fi
  exit 0
fi

PID="$(cat "$PID_FILE")"
if kill -0 "$PID" 2>/dev/null; then
  kill "$PID"
  for _ in $(seq 1 20); do
    if ! kill -0 "$PID" 2>/dev/null; then
      rm -f "$PID_FILE"
      echo "Stopped: pid=${PID}"
      exit 0
    fi
    sleep 0.2
  done
  kill -9 "$PID" 2>/dev/null || true
  echo "Force stopped: pid=${PID}"
else
  echo "Process not running: pid=${PID}"
fi

rm -f "$PID_FILE"
