#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -n "${BASE_URL:-}" ]]; then
  echo "Running public chat smoke against ${BASE_URL%%\?*}"
else
  echo "Running direct provider smoke from local configuration"
fi

cargo run --quiet --bin cloud_smoke
