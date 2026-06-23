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
│   ├── reactor-webrtc-sys/     unsafe FFI to the native libwebrtc (+ build.rs)
│   └── reactor-webrtc/         safe, idiomatic Rust API
├── webrtc-build/               our build pipeline (depot_tools + gn/ninja)
│   ├── build.sh                fetch + configure + compile libwebrtc per target
│   ├── package.sh              archive + checksum + manifest for prebuilts
│   └── patches/                our deterministic patch series
└── .github/workflows/          CI (Rust) + the heavy WebRTC build matrix
```

## How the native library is resolved

`reactor-webrtc-sys`'s build script links the native `libwebrtc` in one of three
modes (priority order):

1. `REACTOR_WEBRTC_LIB_DIR=/path` — link a locally built/extracted lib.
2. `REACTOR_WEBRTC_PREBUILT_URL=…` (+ `…_SHA256`) — download + verify our
   prebuilt for the target, extract, and link. **Default production path.**
3. Nothing set — **API/dev mode**: no link directives, so `cargo check` and
   rlib builds of the API succeed without a native library. A final binary that
   actually calls WebRTC must use mode 1 or 2.

So today, with no prebuilt yet:

```bash
cargo check        # ✅ builds the API surface (no native lib needed)
cargo build        # ✅ builds the rlibs; linking a binary needs a prebuilt
```

## Status (M1 scaffold)

- ✅ Workspace + two crates; safe API surface mirroring the shape
  `reactor-sdk-core` used from LiveKit's crate (drop-in intent).
- ✅ `build.rs` with the 3-mode native resolution.
- ✅ `webrtc-build/` pipeline skeleton + `WEBRTC_VERSION`.
- ⏳ **TODO:** pick/lock the WebRTC milestone; implement the build pipeline;
  produce the first prebuilts; generate the FFI (cxx/bindgen) and flesh out the
  safe API (`unimplemented!()` today); add the Android Java companion in our
  namespace; wire `reactor-sdk-core` to link this behind a flag, then make it
  the default and drop LiveKit.

See the SDK Architecture Plan for the full design and rollout.

## Licensing

Upstream WebRTC is **BSD-3-Clause + the WebRTC patent grant**; redistributing
prebuilts is permitted with attribution (recorded in the SBOM + `NOTICE.md`).
This crate's own license is TBD (`LicenseRef-Reactor-Proprietary` placeholder).
