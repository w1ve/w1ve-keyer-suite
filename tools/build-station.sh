#!/usr/bin/env bash
# Builds the Station binary on this machine.
#
# Why this script exists: the VS 2026 Insiders install at
#   E:\Program Files\Microsoft Visual Studio\18\Insiders
# is incomplete (it has cl.exe and libs but no VC\Tools\MSVC\...\include), so cc-rs and rustc
# cannot compile or link against it. A complete MSVC 14.44 toolchain and the Windows 10 SDK are
# already on disk, so we point the include and library search paths at those instead of editing
# the broken install.
#
#   INCLUDE  headers for cl.exe (ring, vswhom-sys, tauri build scripts)
#   LIB      .lib files for link.exe (msvcrt.lib, kernel32.lib, ...)
#
# Remove this script once the Insiders install is repaired (Visual Studio Installer > Repair),
# at which point a plain `cargo build` works.
set -euo pipefail

MSVC='E:\devtools\BuildTools\VC\Tools\MSVC\14.44.35207'
SDK='C:\Program Files (x86)\Windows Kits\10'

export INCLUDE="${MSVC}\\include;${SDK}\\Include\\10.0.19041.0\\ucrt;${SDK}\\Include\\10.0.19041.0\\um;${SDK}\\Include\\10.0.19041.0\\shared"
export LIB="${MSVC}\\lib\\x64;${SDK}\\Lib\\10.0.19041.0\\ucrt\\x64;${SDK}\\Lib\\10.0.19041.0\\um\\x64"

exec cargo build -p rwk-app --release --bin w1ve-rkr-station --features custom-protocol "$@"
