#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PORT="$(awk -F= '/^PORT=/{print $2}' .env 2>/dev/null | tail -n 1 | tr -d '"' || true)"
PORT="${PORT:-8787}"
PID_FILE=".run/dev.pid"
LOG_FILE="logs/dev.log"
SCREEN_SESSION="mjy_voice_shop_dev"

if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  echo "Pid file: $(cat "$PID_FILE")"
elif [[ -f "$PID_FILE" ]]; then
  echo "Pid file: stale pid $(cat "$PID_FILE")"
else
  echo "Pid file: not running"
fi

echo
echo "Screen session:"
if command -v screen >/dev/null 2>&1; then
  SCREEN_OUTPUT="$(screen -ls 2>/dev/null || true)"
  if echo "$SCREEN_OUTPUT" | grep "$SCREEN_SESSION" >/dev/null 2>&1; then
    echo "$SCREEN_OUTPUT" | grep "$SCREEN_SESSION"
  else
    echo "not running"
  fi
else
  echo "screen not installed"
fi

echo
echo "Port ${PORT}:"
lsof -iTCP:"$PORT" -sTCP:LISTEN -n -P || true

echo
echo "Health:"
curl -fsS "http://127.0.0.1:${PORT}/api/health" || true
echo

if [[ -f "$LOG_FILE" ]]; then
  echo
  echo "Recent log:"
  tail -n 40 "$LOG_FILE"
fi
