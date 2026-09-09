#!/usr/bin/env bash
# Run inside a musllinux_1_2 container with the freshly built archive mounted.
set -euo pipefail
: "${REACTOR_WEBRTC_LIB_DIR:?Set the musl prebuilt directory}"
[ "$(cat "$REACTOR_WEBRTC_LIB_DIR/lib/linux_libc")" = musl ]
apk add --no-cache curl zstd build-base
# Alpine 3.22's distro clang is too old for the bundled libc++ headers.
# PyPA verifies the pinned static compiler's checksums and configures musl.
manylinux-install-clang -v 22.1.8.1
export PATH="/opt/clang/bin:$PATH"
curl --proto '=https' --tlsv1.2 -sSf --retry 3 https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain none
# shellcheck source=/dev/null
source "$HOME/.cargo/env"
export CXX=clang++
export RUSTFLAGS="-C target-feature=-crt-static -C linker=clang -C link-arg=-fuse-ld=lld"
# Keep the host's Cargo artifacts separate from these native musl artifacts.
export CARGO_TARGET_DIR=/io/target/musl-tests
cargo test -p reactor-webrtc-sys -p reactor-webrtc --lib
# Exercise real C++ factories and local media/data-channel connections as well.
cargo test -p reactor-webrtc-sys --test link
cargo test -p reactor-webrtc --test loopback --test datachannel
