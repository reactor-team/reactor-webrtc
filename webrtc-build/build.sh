#!/usr/bin/env bash
#
# Build our libwebrtc for one target: fetch (depot_tools) → patch → gn gen →
# ninja → assemble the static lib. See ./README.md.
#
# Usage: build.sh <os> <arch> [debug|release]
#   os:   mac | ios | android | linux | win | visionos
#   arch: arm64 | x64 | arm | x86
#
# Env:
#   IOS_ENV=device|simulator   (ios only; default device)
#   NINJA_TARGET=webrtc        (override the ninja target if needed)
set -euo pipefail

OS="${1:?usage: build.sh <os> <arch> [profile]}"
ARCH="${2:?usage: build.sh <os> <arch> [profile]}"
PROFILE="${3:-release}"

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck disable=SC1090
source "$ROOT/WEBRTC_VERSION"

DEPOT="$HERE/depot_tools"
SRC="$HERE/src"                       # gclient root (.gclient + src/)
# iOS/visionOS device and simulator share os+arch but differ by target
# environment, so fold IOS_ENV into the target slug to keep their outputs apart.
VARIANT=""
case "$OS" in ios|visionos) VARIANT="-${IOS_ENV:-device}" ;; esac
OUT="$HERE/out/$OS-$ARCH$VARIANT-$PROFILE"

# ── arch → gn target_cpu ──────────────────────────────────────────────────────
case "$ARCH" in
  x64|x86_64)     CPU=x64 ;;
  arm64|aarch64)  CPU=arm64 ;;
  arm|armv7)      CPU=arm ;;
  x86|i686)       CPU=x86 ;;
  *) echo "build.sh: unknown arch '$ARCH'" >&2; exit 2 ;;
esac

# ── os → gn target_os ─────────────────────────────────────────────────────────
case "$OS" in
  mac|macos)  GN_OS=mac ;;
  ios)        GN_OS=ios ;;
  android)    GN_OS=android ;;
  linux)      GN_OS=linux ;;
  win|windows) GN_OS=win ;;
  visionos)   GN_OS=ios ;;    # toolchain-dependent; treated as an iOS variant for now
  *) echo "build.sh: unknown os '$OS'" >&2; exit 2 ;;
esac

# ── gn args (the heart of the build) ──────────────────────────────────────────
# Base args shared by every target, then per-OS additions. Rationale:
#   is_component_build=false   → one static libwebrtc.a (what we ship)
#   use_custom_libcxx=false    → link the *platform* C++ stdlib so the lib
#     (per OS)                    interops with our Rust/cc glue and the consuming
#                                app (mixing WebRTC's bundled libc++ with the
#                                app's is a classic source of crashes). Set
#                                per-OS below, since the "platform" stdlib and
#                                how we reach a modern one differs by target.
#   rtc_include_tests/examples/tools=false → trim the build
#   rtc_libvpx_build_vp9=true  → VP9 software codec
#   treat_warnings_as_errors=false → tolerate upstream warnings across milestones
gn_args() {
  local args=(
    "is_debug=$([ "$PROFILE" = debug ] && echo true || echo false)"
    "is_component_build=false"
    "rtc_include_tests=false"
    "rtc_build_examples=false"
    "rtc_build_tools=false"
    "rtc_enable_protobuf=true"
    "treat_warnings_as_errors=false"
    "use_rtti=true"
    "rtc_libvpx_build_vp9=true"
    "target_os=\"$GN_OS\""
    "target_cpu=\"$CPU\""
  )
  case "$GN_OS" in
    mac)
      # Hardware H.264 via VideoToolbox; no software OpenH264 needed.
      # Modern Xcode libc++ is the platform stdlib for both lib and glue.
      args+=("rtc_use_h264=false" "use_custom_libcxx=false" "symbol_level=1")
      ;;
    ios)
      args+=(
        "ios_enable_code_signing=false"
        "rtc_enable_symbol_export=true"
        "rtc_use_h264=false"
        "use_custom_libcxx=false"
        "target_environment=\"${IOS_ENV:-device}\""
      )
      ;;
    android)
      # Bundled libc++: it ships the libunwind that Chromium's link flags
      # (--unwindlib=none) otherwise leave out, so the NDK link resolves
      # _Unwind_* (undefined with use_custom_libcxx=false).
      # android_static_analysis=off: the default "build_server" needs autoninja
      # (AUTONINJA_BUILD_ID) for the Java validate-deps step; we build with plain
      # ninja and don't need Java lint/errorprone.
      # android_jni_package_prefix: repackages all org.webrtc.* Java classes into
      # inc.reactor.org.webrtc.* so the JAR is namespaced to Reactor, not LiveKit.
      # Patch 0002 wires this arg into jni_zero.gni's generate_jni templates.
      args+=(
        "symbol_level=1"
        "rtc_use_h264=false"
        "use_custom_libcxx=true"
        "android_static_analysis=\"off\""
        "android_jni_package_prefix=\"inc.reactor\""
      )
      ;;
    linux)
      # Always use WebRTC's *bundled* clang + bundled libc++:
      #   • the bundled clang is a Chromium fork with flags no stock clang has
      #     (-fno-lifetime-dse, --crel, …), so a host clang can't compile this;
      #   • the pinned debian sysroot's libstdc++ is too old for WebRTC's C++20
      #     (std::make_unique_for_overwrite; ssl_stream_adapter.h's nullptr_t),
      #     so use the bundled (modern) libc++.
      # Self-contained via the sysroot. The bundled clang is published for x86_64
      # hosts only, so arm64 is cross-compiled from x86_64 (see the sysroot fetch
      # above + the CI runner mapping). NOTE: linking a consumer's glue against
      # this lib requires compiling that glue with libc++ too (follow-up).
      # No screen/desktop capture in a calling SDK: disable both linux backends
      # (X11 + PipeWire) so the lib carries no libX11 dependency.
      args+=(
        "rtc_use_x11=false"
        "rtc_use_pipewire=false"
        "is_clang=true"
        "use_sysroot=true"
        "use_custom_libcxx=true"
        "symbol_level=1"
      )
      ;;
    win)
      args+=("use_custom_libcxx=false" "symbol_level=1")
      ;;
  esac
  echo "${args[*]}"
}

echo "==> reactor-webrtc build: os=$GN_OS cpu=$CPU profile=$PROFILE"
echo "    pinned: ${WEBRTC_BRANCH:-?} (${WEBRTC_MILESTONE:-?}) commit='${WEBRTC_COMMIT:-<branch head>}' patch=${REACTOR_PATCH_LEVEL:-?}"

# ── 1. depot_tools ────────────────────────────────────────────────────────────
if [ ! -d "$DEPOT" ]; then
  echo "==> cloning depot_tools"
  git clone --depth 1 https://chromium.googlesource.com/chromium/tools/depot_tools.git "$DEPOT"
fi
export PATH="$DEPOT:$PATH"
export DEPOT_TOOLS_UPDATE="${DEPOT_TOOLS_UPDATE:-1}"

# ── 2. fetch + sync WebRTC at the pinned ref ──────────────────────────────────
mkdir -p "$SRC"
cd "$SRC"
if [ ! -d src ]; then
  echo "==> fetch webrtc (large; first run downloads ~tens of GB)"
  fetch --nohooks webrtc
fi
if [ "$GN_OS" = "android" ] && ! grep -q "target_os" .gclient 2>/dev/null; then
  echo "target_os=['android','linux']" >> .gclient
fi
REF="${WEBRTC_COMMIT:-}"
[ -z "$REF" ] && REF="$WEBRTC_BRANCH"
# Release builds live on branch-heads/*, which a default checkout does not
# fetch — add the refspec and sync --with_branch_heads.
if ! git -C src config --get-all remote.origin.fetch | grep -q branch-heads; then
  git -C src config --add remote.origin.fetch '+refs/branch-heads/*:refs/remotes/branch-heads/*'
fi
# A previous build leaves our patch series applied (modified tracked files);
# gclient sync refuses a dirty tree, so reset it first. Step 3 re-applies the
# patches after the sync.
if [ -d src/.git ]; then git -C src reset --hard >/dev/null 2>&1 || true; fi
echo "==> gclient sync -> src@$REF (--with_branch_heads)"
gclient sync --with_branch_heads --no-history --shallow -r "src@$REF" -D
RESOLVED="$(git -C src rev-parse HEAD)"
echo "==> resolved WebRTC commit: $RESOLVED  (lock this in WEBRTC_VERSION:WEBRTC_COMMIT)"

# ── 3. apply our patch series ─────────────────────────────────────────────────
cd "$SRC/src"
git reset --hard "$RESOLVED" >/dev/null
# Also reset sub-repos that our patches touch (gclient sync already pinned
# them; reset makes repeated builds idempotent).
[ -d build/.git ]              && git -C build              reset --hard >/dev/null 2>&1 || true
[ -d third_party/jni_zero/.git ] && git -C third_party/jni_zero reset --hard >/dev/null 2>&1 || true
shopt -s nullglob
for p in "$HERE"/patches/*.patch; do
  echo "==> applying patch $(basename "$p")"
  # Try git apply (works for files tracked by the main WebRTC repo); fall back
  # to patch(1) for files in third_party sub-repos (e.g. jni_zero).
  git apply --3way "$p" 2>/dev/null || patch -p1 < "$p"
done
shopt -u nullglob

# Cross-compiling linux/arm64 from an x86_64 host needs the arm64 sysroot, which
# the default sync (host arch only) does not fetch.
if [ "$GN_OS" = linux ] && [ "$CPU" != "$(uname -m | sed 's/x86_64/x64/;s/aarch64/arm64/')" ]; then
  echo "==> installing linux sysroot for $CPU (cross)"
  python3 build/linux/sysroot_scripts/install-sysroot.py --arch="$CPU" \
    || python3 build/linux/sysroot_scripts/install_sysroot.py --arch="$CPU"
fi

# ── 4. gn gen ─────────────────────────────────────────────────────────────────
ARGS="$(gn_args)"
echo "==> gn gen $OUT"
echo "    args: $ARGS"
gn gen "$OUT" --args="$ARGS"

# ── 5. build the monolithic static lib ────────────────────────────────────────
echo "==> ninja -C $OUT ${NINJA_TARGET:-webrtc}"
ninja -C "$OUT" "${NINJA_TARGET:-webrtc}"

# ── 6. assemble: copy the static lib next to the build dir ────────────────────
LIB="$OUT/obj/libwebrtc.a"
[ -f "$LIB" ] || { echo "build.sh: expected $LIB not found" >&2; exit 1; }
mkdir -p "$OUT/dist/lib"
cp "$LIB" "$OUT/dist/lib/libwebrtc.a"
echo "✅ built $OUT/dist/lib/libwebrtc.a ($(du -h "$LIB" | cut -f1))"
echo "   next: ./package.sh $OS $ARCH $PROFILE  (archives lib + headers, checksums)"
