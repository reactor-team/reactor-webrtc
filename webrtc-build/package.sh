#!/usr/bin/env bash
#
# Package a built libwebrtc target into a checksummed prebuilt archive and
# update the prebuilt index manifest. SKELETON — see ./README.md.
#
# Usage: package.sh <os> <arch> [debug|release]
set -euo pipefail

OS="${1:?usage: package.sh <os> <arch> [profile]}"
ARCH="${2:?usage: package.sh <os> <arch> [profile]}"
PROFILE="${3:-release}"

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out/$OS-$ARCH-$PROFILE"
DIST="$HERE/dist"
NAME="reactor-webrtc-$OS-$ARCH-$PROFILE.tar.zst"

mkdir -p "$DIST"

# 1. Archive the static lib + headers. TODO(M1):
#    tar --zstd -cf "$DIST/$NAME" -C "$OUT" libwebrtc.a include/
#    (Android also includes the Java companion compiled into our namespace.)

# 2. Checksum. TODO(M1):
#    ( cd "$DIST" && shasum -a 256 "$NAME" > "$NAME.sha256" )

# 3. Update the prebuilt index manifest (consumed via REACTOR_WEBRTC_PREBUILT_URL).
#    Maps reactor-webrtc x.y.z -> (WEBRTC milestone, patch level, per-target URL + sha256).

echo "!! SKELETON: packaging is not yet implemented (M1). See README.md."
exit 1
