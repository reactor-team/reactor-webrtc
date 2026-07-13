# reactor-webrtc

Reactor's **owned** WebRTC stack: a safe Rust API (`reactor-webrtc`) over our
own build of Google's WebRTC (`libwebrtc`), with the low-level FFI in
`reactor-webrtc-sys`. **No dependency on LiveKit's `libwebrtc` / `webrtc-sys`
crates.**

It is the WebRTC engine shared across the platform:

- **`reactor-sdk-core`** — the native client SDK core (C ABI) consumed by the
  C++/Python/Swift/Kotlin/Go SDKs, and
- **`reactor-runtime`** — the server, where it replaces GStreamer.

This is **M1** of the SDK Architecture Plan — the foundation every other step
builds on.

```
reactor-webrtc/
├── WEBRTC_VERSION              pinned upstream milestone + our patch level
├── crates/
│   ├── reactor-webrtc-sys/     unsafe FFI + C++ glue (glue/) + build.rs
│   └── reactor-webrtc/         safe, idiomatic Rust API
├── webrtc-build/               our build pipeline (depot_tools + gn/ninja)
│   ├── build.sh                POSIX build: fetch + patch + gn + ninja + assemble
│   ├── build.ps1               Windows build+package (PowerShell; native depot_tools)
│   ├── package.sh              archive + checksum + manifest for prebuilts
│   ├── sbom.py                 CycloneDX SBOM generator (cross-platform core)
│   ├── sbom.sh                 SBOM wrapper for the POSIX builds
│   ├── publish.sh              cut a GitHub Release with the per-target assets
│   └── patches/                deterministic patch series (see patches/README.md)
├── .github/workflows/ci.yml             fast public checks (fmt / check / clippy)
├── .github/workflows/webrtc-build.yml   heavy per-target libwebrtc builds + publish
└── .github/workflows/lib-link-test.yml  fast lib-link test against a prebuilt
```

## How the native library is resolved

`reactor-webrtc-sys`'s build script links the native `libwebrtc` in one of three
modes (priority order):

1. `REACTOR_WEBRTC_LIB_DIR=/path` — link a locally built/extracted lib
   (packaged layout `<dir>/lib` + `<dir>/include`, or a bare dir + optional
   `REACTOR_WEBRTC_INCLUDE_DIR`).
2. `REACTOR_WEBRTC_PREBUILT_URL=…` (+ `…_SHA256`) — download + verify our
   prebuilt for the target, extract, and link. **Default production path.**
3. Nothing set — **API/dev mode**: no link directives, so `cargo check` and
   rlib builds of the API succeed without a native library. A final binary that
   actually calls WebRTC must use mode 1 or 2.

```bash
cargo check        # ✅ builds the API surface (no native lib needed)
cargo build        # ✅ builds the rlibs; linking a binary needs a prebuilt

# link + run the FFI/integration tests against a locally staged build:
REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
  cargo test --workspace
```

Prebuilts are published as a **GitHub Release** per pinned version, tag
`webrtc-<milestone>-<commit>-p<patch>` (current:
[`webrtc-7907-a5ddff60-p1`](https://github.com/reactor-team/reactor-webrtc/releases/tag/webrtc-7907-a5ddff60-p1)),
one `reactor-webrtc-<os>-<arch>-<profile>.tar.zst` per target (+ `.sha256`,
`.manifest.json`, CycloneDX `.sbom.json`).

## Target matrix

Built + packaged + published for every target:

| OS | arch | notes |
|----|------|-------|
| macOS | arm64, x64 | platform libc++; VideoToolbox H.264 |
| iOS | arm64 device + arm64 simulator | distinct `target_environment` builds |
| Linux | x64, arm64 | bundled clang + bundled libc++; arm64 cross-built from x64 |
| Android | arm64 | NDK; bundled libc++ |
| Windows | x64 | MSVC STL; native depot_tools via `build.ps1` |

`visionos` has no upstream gn `target_os`; more Android ABIs are a matrix add.
Per-target toolchain rationale lives at the top of `build.sh` (POSIX) and
`build.ps1` (Windows); the full build recipe is in
[`webrtc-build/README.md`](webrtc-build/README.md).

## Status (M1)

- ✅ Workspace + two crates; safe API surface mirroring the shape
  `reactor-sdk-core` used from LiveKit's crate (drop-in intent).
- ✅ `build.rs` with the 3-mode native resolution.
- ✅ `webrtc-build/` pipeline (fetch → patch → gn → ninja → package → publish),
  pinned to `branch-heads/7907` in `WEBRTC_VERSION`.
- ✅ All matrix targets build + package + upload green on GitHub Actions, and are
  published as a GitHub Release.
- ✅ FFI glue links + runs against the lib (lib-link tests on macOS arm64 and
  Linux x64 in CI).
- ⏳ **TODO:** flesh out the remaining safe API; the symbol-isolation and
  Android Java-namespace patches (see `webrtc-build/patches/README.md`); wire
  `reactor-sdk-core` to link this behind a flag, then make it the default and
  drop LiveKit.

See the SDK Architecture Plan for the full design and rollout.

## Licensing

Upstream WebRTC is **BSD-3-Clause + the WebRTC patent grant**; redistributing
prebuilts is permitted with attribution (recorded in the SBOM + `NOTICE.md`).
This crate's own license is TBD (`LicenseRef-Reactor-Proprietary` placeholder).
