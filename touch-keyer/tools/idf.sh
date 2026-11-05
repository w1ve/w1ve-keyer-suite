#!/usr/bin/env bash
# Run a command (idf.py, esptool, ...) with the project-local ESP-IDF environment.
#   tools/idf.sh idf.py build
#   tools/idf.sh idf.py -p /dev/ttyACM0 flash monitor
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

export IDF_TOOLS_PATH="$ROOT/.toolchain/espressif"

# ESP-IDF built its Python env with the interpreter recorded at setup time. Put that
# interpreter first on PATH so export.sh resolves the same env (the system python is
# usually too new), then fall back to whatever is already there.
if [ -f "$ROOT/.toolchain/python_bin" ]; then
    PATH="$(cat "$ROOT/.toolchain/python_bin"):$PATH"
    export PATH
fi

# shellcheck disable=SC1091
. "$ROOT/.toolchain/esp-idf/export.sh" >/dev/null

cd "$ROOT"
exec "$@"
