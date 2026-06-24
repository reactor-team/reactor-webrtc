#!/usr/bin/env bash
#
# Publish the built prebuilts as a GitHub Release on this repo. Idempotent:
# creates the release for the pinned WebRTC version+patch level if missing, then
# uploads (clobbering) every per-target asset found in dist/ — the archive,
# its .sha256, the manifest, and the CycloneDX SBOM.
#
# `reactor-webrtc-sys`'s build.rs (mode 2) consumes the assets by their stable
# release-download URLs:
#   https://github.com/<repo>/releases/download/<tag>/reactor-webrtc-<os>-<arch>-<profile>.tar.zst
#
# Usage: publish.sh [dist_dir]
# Env:
#   PUBLISH_REPO=owner/name   (default: reactor-team/reactor-webrtc)
#   PUBLISH_DRAFT=1           (create as a draft)
#   PUBLISH_LATEST=1          (mark as the 'latest' release; default: prerelease)
#   GH_TOKEN / GITHUB_TOKEN   (auth for gh)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck disable=SC1090
source "$ROOT/WEBRTC_VERSION"

REPO="${PUBLISH_REPO:-reactor-team/reactor-webrtc}"
DIST="${1:-$HERE/dist}"

command -v gh >/dev/null || { echo "publish.sh: gh CLI not found" >&2; exit 1; }

# ── Release tag derived from the pinned upstream version + our patch level ────
MNUM="$(printf '%s' "${WEBRTC_MILESTONE:-}" | tr -dc '0-9')"
[ -n "$MNUM" ] || MNUM="unknown"
COMMIT_SHORT="${WEBRTC_COMMIT:0:8}"
[ -n "$COMMIT_SHORT" ] || COMMIT_SHORT="nocommit"
TAG="webrtc-${MNUM}-${COMMIT_SHORT}-p${REACTOR_PATCH_LEVEL:-0}"

# ── Collect assets ────────────────────────────────────────────────────────────
shopt -s nullglob
assets=("$DIST"/*.tar.zst "$DIST"/*.tar.zst.sha256 "$DIST"/*.manifest.json "$DIST"/*.sbom.json)
shopt -u nullglob
[ ${#assets[@]} -gt 0 ] || { echo "publish.sh: no artifacts in $DIST (build + package first)" >&2; exit 1; }

ARCHIVES="$(printf '%s\n' "$DIST"/*.tar.zst 2>/dev/null | grep -c . || true)"
echo "==> publishing $ARCHIVES target archive(s) (${#assets[@]} assets) to $REPO @ $TAG"

NOTES="Prebuilt \`libwebrtc\` for Reactor's owned build.

- WebRTC milestone: \`${WEBRTC_MILESTONE:-unknown}\`
- WebRTC commit: \`${WEBRTC_COMMIT:-unknown}\`
- Reactor patch level: \`${REACTOR_PATCH_LEVEL:-0}\`

Each target ships \`reactor-webrtc-<os>-<arch>-<profile>.tar.zst\` (lib +
headers) with a matching \`.sha256\`, \`.manifest.json\`, and CycloneDX
\`.sbom.json\`. Consumed by \`reactor-webrtc-sys\` via
\`REACTOR_WEBRTC_PREBUILT_URL\`+\`_SHA256\`."

# ── Create (if missing) then upload ───────────────────────────────────────────
if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  echo "    release $TAG exists — updating assets"
else
  flags=(--title "libwebrtc $TAG" --notes "$NOTES")
  [ "${PUBLISH_DRAFT:-0}" = 1 ] && flags+=(--draft)
  if [ "${PUBLISH_LATEST:-0}" = 1 ]; then flags+=(--latest); else flags+=(--prerelease); fi
  echo "    creating release $TAG"
  gh release create "$TAG" --repo "$REPO" "${flags[@]}"
fi

gh release upload "$TAG" --repo "$REPO" --clobber "${assets[@]}"

echo "✅ published to https://github.com/$REPO/releases/tag/$TAG"
echo "   asset URL base: https://github.com/$REPO/releases/download/$TAG/"
