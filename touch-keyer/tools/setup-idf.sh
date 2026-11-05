#!/usr/bin/env bash
# Install a project-local ESP-IDF toolchain (nothing outside this folder is touched).
#
# Layout:
#   .toolchain/esp-idf/        ESP-IDF v5.3.4
#   .toolchain/espressif/      toolchains and Python env (IDF_TOOLS_PATH)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IDF_VERSION="${IDF_VERSION:-v5.3.4}"

cd "$ROOT"
mkdir -p .toolchain
cd .toolchain

export IDF_TOOLS_PATH="$PWD/espressif"

echo "== Python 3.12 (via uv) =="
uv python install 3.12
PY="$(uv python find 3.12)"
echo "   $PY"
# Remember the interpreter's bin directory so tools/idf.sh resolves the same Python env.
dirname "$PY" > "$ROOT/.toolchain/python_bin"

if [ ! -d esp-idf ]; then
    echo "== ESP-IDF $IDF_VERSION =="
    git clone --depth 1 --branch "$IDF_VERSION" --recursive https://github.com/espressif/esp-idf.git esp-idf
fi

cd esp-idf
PATH="$(dirname "$PY"):$PATH" ./install.sh esp32c6

echo
echo "Toolchain ready. Build with:"
echo "  tools/idf.sh idf.py set-target esp32c6"
echo "  tools/idf.sh idf.py build"
