#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CXX_BIN="${CXX:-c++}"
OUTPUT_BIN="${OUTPUT_BIN:-$SCRIPT_DIR/device_client}"

"$CXX_BIN" -std=c++17 -O2 -Wall -Wextra "$SCRIPT_DIR/device_client.cpp" -o "$OUTPUT_BIN"

echo "C++ SDK binary is ready: $OUTPUT_BIN"
