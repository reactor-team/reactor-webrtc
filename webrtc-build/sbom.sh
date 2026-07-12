#!/usr/bin/env bash
#
# Generate a CycloneDX SBOM for a built libwebrtc target. Thin wrapper that
# resolves paths and defers to the cross-platform ./sbom.py (shared with
# build.ps1 on Windows). See ./README.md.
#
# Usage: sbom.sh <os> <arch> [debug|release]
set -euo pipefail

OS="${1:?usage: sbom.sh <os> <arch> [profile]}"
ARCH="${2:?usage: sbom.sh <os> <arch> [profile]}"
PROFILE="${3:-release}"

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck disable=SC1090
source "$ROOT/WEBRTC_VERSION"

VARIANT=""
case "$OS" in ios|visionos) VARIANT="-${IOS_ENV:-device}" ;; esac

SRC="$HERE/src/src"
OUT_OBJ="$HERE/out/$OS-$ARCH$VARIANT-$PROFILE/obj/third_party"
DIST="$HERE/dist"
NAME="reactor-webrtc-${OS}-${ARCH}${VARIANT}-${PROFILE}"
OUT="$DIST/$NAME.sbom.json"
mkdir -p "$DIST"

[ -d "$SRC/third_party" ] || { echo "sbom.sh: no checkout at $SRC (build first)" >&2; exit 1; }

RESOLVED="$(git -C "$SRC" rev-parse HEAD 2>/dev/null || echo "${WEBRTC_COMMIT:-unknown}")"

echo "==> generating SBOM: third_party compiled into this build ∧ Shipped:yes"
python3 "$HERE/sbom.py" \
  --src "$SRC" \
  --obj-dir "$OUT_OBJ" \
  --out "$OUT" \
  --name "$NAME" \
  --milestone "${WEBRTC_MILESTONE:-unknown}" \
  --commit "$RESOLVED"
