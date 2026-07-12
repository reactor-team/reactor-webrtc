#!/usr/bin/env python3
"""Generate a CycloneDX SBOM for a built libwebrtc target.

Cross-platform core shared by sbom.sh (POSIX) and build.ps1 (Windows): scans the
WebRTC checkout's third_party/*/README.chromium for components Google marks
`Shipped: yes`, intersected with the components actually compiled into this build
(the subdirs under <out>/obj/third_party), and emits CycloneDX 1.5 JSON.

Usage:
  sbom.py --src <checkout> --obj-dir <out/obj/third_party> --out <file.sbom.json>
          --name <artifact-name> --milestone <m> --commit <sha>
"""
import argparse
import datetime
import glob
import json
import os


def parse_readme(path):
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


def collect(src, obj_dir):
    # Components actually compiled into this build (by third_party subdir name).
    built = set()
    if os.path.isdir(obj_dir):
        built = {d for d in os.listdir(obj_dir) if os.path.isdir(os.path.join(obj_dir, d))}

    components = []
    for readme in sorted(glob.glob(os.path.join(src, "third_party", "*", "README.chromium"))):
        dirbase = os.path.basename(os.path.dirname(readme))
        # Only components compiled into this build and that Google ships in binaries.
        if built and dirbase not in built:
            continue
        f = parse_readme(readme)
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
            comp["licenses"] = [
                {"license": {"name": part.strip()}} for part in lic.split(",") if part.strip()
            ]
        url = f.get("url", "")
        if url:
            comp["externalReferences"] = [{"type": "vcs", "url": url}]
        components.append(comp)
    return components


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", required=True)
    ap.add_argument("--obj-dir", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--name", required=True)
    ap.add_argument("--milestone", default="unknown")
    ap.add_argument("--commit", default="unknown")
    args = ap.parse_args()

    components = collect(args.src, args.obj_dir)
    timestamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    sbom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "timestamp": timestamp,
            "component": {
                "type": "library",
                "name": args.name,
                "version": f"{args.milestone}+{args.commit[:12]}",
                "description": "Reactor's owned libwebrtc build",
            },
            "tools": [{"name": "reactor webrtc-build/sbom.py"}],
        },
        "components": components,
    }
    with open(args.out, "w") as f:
        json.dump(sbom, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"OK {args.out} ({len(components)} shipped components, webrtc {args.commit})")


if __name__ == "__main__":
    main()
