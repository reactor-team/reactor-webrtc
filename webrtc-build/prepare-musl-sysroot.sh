#!/usr/bin/env bash
# Build a target-only Alpine sysroot. No target executables run, so preparing
# arm64 on x64 needs no emulation. The compiler itself runs on the glibc host.
# Usage: prepare-musl-sysroot.sh <x64|arm64> <output-directory>
set -euo pipefail

case "${1:?usage: prepare-musl-sysroot.sh <x64|arm64> <output-directory>}" in
  x64) arch=x86_64 ;;
  arm64) arch=aarch64 ;;
  *) echo "unsupported musl architecture: $1" >&2; exit 2 ;;
esac
destination="${2:?output directory is required}"
mkdir -p "$destination"
destination="$(cd "$destination" && pwd)"
# Alpine 3.22 uses musl 1.2, matching the musllinux_1_2 wheel builders.
# /etc/apk/keys contains only host keys; cross builds need the target keys.
docker run --rm -v "$destination:/sysroot" alpine:3.22 \
  apk --arch "$arch" --root /sysroot --initdb --no-scripts \
    --keys-dir "/usr/share/apk/keys/$arch" --repositories-file /etc/apk/repositories \
    add --no-cache musl-dev linux-headers alsa-lib-dev pulseaudio-dev
echo "Sysroot ready: REACTOR_MUSL_SYSROOT=$destination"
