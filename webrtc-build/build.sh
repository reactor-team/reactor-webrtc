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
      # use_custom_libcxx=true: the debian sysroot's libstdc++ is too old for
      # WebRTC's C++20 (std::make_unique_for_overwrite, nullptr_t in ssl_stream_adapter).
      # The bundled libc++ is compiled from source and linked explicitly.
      # use_sysroot=true: the pinned Debian Bullseye sysroot for a stable ABI.
      # No screen/desktop capture: disable X11 + PipeWire (no libX11 dep).
      #
      # is_clang=true on all Linux hosts, including aarch64:
      #   Chromium only publishes a Linux_x64 bundled clang binary; there is no
      #   Linux_arm64 variant.  On aarch64 the build installs a filter wrapper at
      #   the expected bundled-clang path so gn's is_clang=true toolchain invokes
      #   system clang-21 + lld (see step 3b below).  is_clang=false is NOT used
      #   because Chromium does not support is_clang=false + use_custom_libcxx=true:
      #   that combination builds libc++ without the __Cr ABI namespace, leaving
      #   libwebrtc.a with unresolvable std::__Cr::* references at link time.
      args+=(
        "rtc_use_x11=false"
        "rtc_use_pipewire=false"
        "is_clang=true"
        "use_sysroot=true"
        "use_custom_libcxx=true"
        "symbol_level=1"
      )
      if [ "$(uname -m)" = "aarch64" ]; then
        # clang_use_chrome_plugins=false: system clang-21 (used via the filter
        # wrapper) does not ship Chromium's clang plugins (find-bad-constructs,
        # raw-ptr-plugin, unsafe-buffers).  Disable so the build system emits
        # no -Xclang -add-plugin / -plugin-arg-* flags for this host.
        #
        # libyuv_use_sme=false: ARM64 SME (Scalable Matrix Extension) in libyuv
        # is not universally available on all arm64 microarchs and causes
        # compilation issues with system clang-21.  LiveKit disables this too.
        #
        # use_crel=false / cflags filter: Chromium enables experimental CREL
        # (compact ELF relocations) via -Wa,--crel on Linux arm64.  This causes
        # runtime segfaults on arm64 Linux (crbug.com/376278218).  LiveKit
        # applies disable_crel.patch; we suppress it in the filter wrapper and
        # guard with this gn arg if Chromium exposes one.
        args+=(
          "clang_use_chrome_plugins=false"
          "libyuv_use_sme=false"
        )
      fi
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
# Also reset third_party sub-repos that our patches touch (gclient sync already
# pinned them; reset makes repeated builds idempotent).
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

# ── 3b. arm64: install clang-21 filter wrapper ───────────────────────────────
# Chromium ships only a Linux_x64 bundled clang (DEPS GCS dep); gclient sync
# downloads the x86_64 package even on arm64 hosts, leaving non-executable
# x86_64 ELF binaries in the bundled-clang dir.  Overwrite clang/clang++ and
# the LLVM helper tools with native arm64 versions so gn's is_clang=true
# toolchain can invoke them.
#
# The wrapper drops three Chromium-patched-only flags that standard clang-21
# does not recognise:
#   -fno-lifetime-dse              (LLVM lifetime-DSE opt control)
#   -fdiagnostics-show-inlining-chain  (Chromium diagnostics extension)
#   -fsanitize-ignore-for-ubsan-feature=*  (Chromium UBSan extension)
# All other flags, including --sysroot and the target triple, pass through.
# -fuse-ld=lld is injected so lld (not ld.bfd) links against the Bullseye
# sysroot — ld.bfd fails on GLIBC_PRIVATE ABI references in that sysroot.
if [ "$GN_OS" = linux ] && [ "$(uname -m)" = "aarch64" ]; then
  echo "==> installing clang-21 filter wrapper in bundled-clang dir (arm64 host)"
  CLANG_BIN="$SRC/src/third_party/llvm-build/Release+Asserts/bin"
  mkdir -p "$CLANG_BIN"
  for _bin in clang clang++; do
    # Use the binary in /usr/lib/llvm-21/bin/ rather than /usr/bin/clang{++}-21.
    # On Ubuntu arm64, /usr/bin/clang-21 may be a shell wrapper or alternatives
    # pointer that re-execs clang++-21 regardless of the invocation name; when
    # exec replaces the process, argv[0] becomes clang++-21 → C++ driver mode →
    # "error: invalid argument '-std=c11' not allowed with 'C++'" for C files.
    # /usr/lib/llvm-21/bin/clang is the real ELF; argv[0] basename = "clang" →
    # C driver mode, as the Chromium toolchain expects.
    _real=""
    for _p in "/usr/lib/llvm-21/bin/${_bin}" "/usr/bin/${_bin}-21"; do
      [ -x "$_p" ] && _real="$_p" && break
    done
    [ -z "$_real" ] && _real="/usr/bin/${_bin}-21"
    cat > "$CLANG_BIN/$_bin" << CLANG_WRAPPER_EOF
#!/bin/bash
# Filter Chromium-patched-only flags not present in standard clang-21.
filtered=()
for arg in "\$@"; do
  case "\$arg" in
    -fno-lifetime-dse|-fdiagnostics-show-inlining-chain|-fsanitize-ignore-for-ubsan-feature=*)
      ;; # discard: Chromium-patched flags not in standard clang-21
    -Wa,--crel,--allow-experimental-crel)
      ;; # discard: experimental CREL relocations cause runtime segfaults on arm64 Linux (crbug.com/376278218)
    *)
      filtered+=("\$arg")
      ;;
  esac
done
exec $_real -fuse-ld=lld "\${filtered[@]}"
CLANG_WRAPPER_EOF
    chmod +x "$CLANG_BIN/$_bin"
  done
  # llvm-ar: Chromium's is_clang=true toolchain uses it for thin archives.
  for _ar in /usr/lib/llvm-21/bin/llvm-ar /usr/bin/llvm-ar-21 /usr/bin/llvm-ar; do
    [ -e "$_ar" ] && { ln -sf "$_ar" "$CLANG_BIN/llvm-ar"; break; }
  done
  # Other LLVM tools gn may reference from the bundled dir.
  for _tool in llvm-nm llvm-readelf llvm-readobj llvm-objcopy llvm-objdump llvm-strip; do
    for _p in "/usr/lib/llvm-21/bin/$_tool" "/usr/bin/${_tool}-21" "/usr/bin/$_tool"; do
      [ -e "$_p" ] && { ln -sf "$_p" "$CLANG_BIN/$_tool" && break; }
    done
  done
  echo "   wrapper: $(head -2 "$CLANG_BIN/clang" | tail -1)"
  echo "   llvm-ar: $(readlink -f "$CLANG_BIN/llvm-ar" 2>/dev/null || echo 'not found')"
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
