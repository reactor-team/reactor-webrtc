#!/usr/bin/env bash
#
# Package a built libwebrtc target into a checksummed prebuilt archive:
# the static lib + the WebRTC public headers (so reactor-webrtc-sys's C++ glue
# can compile against it). See ./README.md.
#
# Usage: package.sh <os> <arch> [debug|release]
set -euo pipefail

OS="${1:?usage: package.sh <os> <arch> [profile]}"
ARCH="${2:?usage: package.sh <os> <arch> [profile]}"
PROFILE="${3:-release}"

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck disable=SC1090
source "$ROOT/WEBRTC_VERSION"

SRC="$HERE/src/src"
OUT="$HERE/out/$OS-$ARCH-$PROFILE"
DIST="$HERE/dist"
STAGE="$OUT/dist"
NAME="reactor-webrtc-${OS}-${ARCH}-${PROFILE}"

[ -f "$STAGE/lib/libwebrtc.a" ] || { echo "package.sh: build first (no libwebrtc.a)" >&2; exit 1; }

# ── Headers: mirror WebRTC's public .h tree (preserving paths) ────────────────
# The static lib has no "install headers" step upstream; our glue needs the
# source headers. Copy *.h/*.inc preserving directory structure.
echo "==> staging headers from $SRC"
mkdir -p "$STAGE/include"
rsync -am \
  --include='*/' \
  --include='*.h' --include='*.inc' \
  --exclude='*' \
  --exclude='out/**' --exclude='.git/**' --exclude='test/**' \
  "$SRC/" "$STAGE/include/"

# ── Archive + checksum ────────────────────────────────────────────────────────
mkdir -p "$DIST"
ARCHIVE="$DIST/$NAME.tar.zst"
echo "==> archiving $ARCHIVE"
tar --use-compress-program "zstd -19 -T0" \
    -cf "$ARCHIVE" -C "$STAGE" lib include

SHA="$( (command -v shasum >/dev/null && shasum -a 256 "$ARCHIVE" || sha256sum "$ARCHIVE") | awk '{print $1}')"
echo "$SHA  $NAME.tar.zst" > "$ARCHIVE.sha256"
echo "✅ $ARCHIVE"
echo "   sha256: $SHA"

# ── Manifest entry (consumed via REACTOR_WEBRTC_PREBUILT_URL) ─────────────────
# One JSON line per target; the publish step merges these into the index that
# reactor-webrtc-sys's build.rs reads.
RESOLVED="$(git -C "$SRC" rev-parse HEAD 2>/dev/null || echo "${WEBRTC_COMMIT:-unknown}")"
cat > "$DIST/$NAME.manifest.json" <<EOF
{
  "name": "$NAME",
  "os": "$OS",
  "arch": "$ARCH",
  "profile": "$PROFILE",
  "archive": "$NAME.tar.zst",
  "sha256": "$SHA",
  "webrtc_milestone": "${WEBRTC_MILESTONE:-unknown}",
  "webrtc_commit": "$RESOLVED",
  "reactor_patch_level": "${REACTOR_PATCH_LEVEL:-0}"
}
EOF
echo "   manifest: $DIST/$NAME.manifest.json"
