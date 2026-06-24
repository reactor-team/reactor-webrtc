#!/usr/bin/env bash
#
# Generate a CycloneDX SBOM for a built libwebrtc target from the WebRTC
# checkout's third_party/*/README.chromium metadata (the components Google
# marks `Shipped: yes`) plus the pinned upstream commit. See ./README.md.
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

SRC="$HERE/src/src"
OUT_OBJ="$HERE/out/$OS-$ARCH-$PROFILE/obj/third_party"
DIST="$HERE/dist"
NAME="reactor-webrtc-${OS}-${ARCH}-${PROFILE}"
OUT="$DIST/$NAME.sbom.json"
mkdir -p "$DIST"

[ -d "$SRC/third_party" ] || { echo "sbom.sh: no checkout at $SRC (build first)" >&2; exit 1; }

RESOLVED="$(git -C "$SRC" rev-parse HEAD 2>/dev/null || echo "${WEBRTC_COMMIT:-unknown}")"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo "==> generating SBOM: third_party compiled into this build ∧ Shipped:yes"
WEBRTC_MILESTONE="${WEBRTC_MILESTONE:-unknown}" \
WEBRTC_COMMIT="$RESOLVED" \
SBOM_NAME="$NAME" \
SBOM_TIMESTAMP="$TIMESTAMP" \
python3 - "$SRC" "$OUT_OBJ" > "$OUT" <<'PY'
import json, os, sys, glob

src = sys.argv[1]
obj_dir = sys.argv[2]
# Components actually compiled into this build (by third_party subdir name).
built = set()
if os.path.isdir(obj_dir):
    built = {d for d in os.listdir(obj_dir) if os.path.isdir(os.path.join(obj_dir, d))}

def parse(path):
    fields = {}
    with open(path, "r", errors="replace") as f:
        for line in f:
            s = line.strip()
            if s.lower().startswith("description"):
                break
            if ":" in s:
                k, v = s.split(":", 1)
                k = k.strip().lower()
                if k not in fields:
                    fields[k] = v.strip()
    return fields

components = []
for readme in sorted(glob.glob(os.path.join(src, "third_party", "*", "README.chromium"))):
    dirbase = os.path.basename(os.path.dirname(readme))
    # Only components compiled into this build and that Google ships in binaries.
    if built and dirbase not in built:
        continue
    f = parse(readme)
    if f.get("shipped", "").lower() != "yes":
        continue
    name = f.get("name") or f.get("short name")
    if not name:
        continue
    version = f.get("version", "")
    if version in ("", "N/A", "n/a"):
        version = f.get("revision", "") or "unknown"
    comp = {
        "type": "library",
        "name": name,
        "version": version,
        "bom-ref": f"pkg:generic/{name}@{version}",
    }
    lic = f.get("license", "")
    if lic:
        comp["licenses"] = [{"license": {"name": part.strip()}} for part in lic.split(",") if part.strip()]
    url = f.get("url", "")
    if url:
        comp["externalReferences"] = [{"type": "vcs", "url": url}]
    components.append(comp)

milestone = os.environ["WEBRTC_MILESTONE"]
commit = os.environ["WEBRTC_COMMIT"]
sbom = {
    "bomFormat": "CycloneDX",
    "specVersion": "1.5",
    "version": 1,
    "metadata": {
        "timestamp": os.environ["SBOM_TIMESTAMP"],
        "component": {
            "type": "library",
            "name": os.environ["SBOM_NAME"],
            "version": f"{milestone}+{commit[:12]}",
            "description": "Reactor's owned libwebrtc build",
        },
        "tools": [{"name": "reactor webrtc-build/sbom.sh"}],
    },
    "components": components,
}
json.dump(sbom, sys.stdout, indent=2, sort_keys=True)
print()
PY

COUNT="$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["components"]))' "$OUT")"
echo "✅ $OUT ($COUNT shipped components, webrtc $RESOLVED)"
