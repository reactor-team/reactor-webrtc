#!/usr/bin/env bash
#
# Build our libwebrtc for one target. SKELETON — documents the intended steps;
# not yet runnable end-to-end (see ./README.md).
#
# Usage: build.sh <os> <arch> [debug|release]
#   os:   linux | macos | ios | android | windows | visionos
#   arch: x64 | arm64 | armv7 | x86_64 | ...
set -euo pipefail

OS="${1:?usage: build.sh <os> <arch> [profile]}"
ARCH="${2:?usage: build.sh <os> <arch> [profile]}"
PROFILE="${3:-release}"

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck disable=SC1091
source "$ROOT/WEBRTC_VERSION" 2>/dev/null || true   # WEBRTC_BRANCH / WEBRTC_COMMIT / REACTOR_PATCH_LEVEL

SRC="$HERE/src"          # gclient checkout (git-ignored)
OUT="$HERE/out/$OS-$ARCH-$PROFILE"

echo "==> reactor-webrtc build: os=$OS arch=$ARCH profile=$PROFILE"
echo "    pinned: ${WEBRTC_BRANCH:-?} @ ${WEBRTC_COMMIT:-?} (patch ${REACTOR_PATCH_LEVEL:-?})"

# 1. depot_tools + fetch (cached). TODO(M1):
#    export PATH="$HERE/depot_tools:$PATH"
#    [ -d "$SRC" ] || (mkdir -p "$SRC" && cd "$SRC" && fetch --nohooks webrtc && gclient sync -r "$WEBRTC_COMMIT")

# 2. Apply our patch series deterministically. TODO(M1):
#    (cd "$SRC/src" && git reset --hard "$WEBRTC_COMMIT" && for p in "$HERE"/patches/*.patch; do git apply "$p"; done)

# 3. gn gen + ninja with our args per target. TODO(M1):
#    gn gen "$OUT" --args="target_os=\"...\" target_cpu=\"...\" is_debug=... rtc_include_tests=false ..."
#    ninja -C "$OUT" webrtc

echo "!! SKELETON: build steps are not yet implemented (M1). See README.md."
exit 1
