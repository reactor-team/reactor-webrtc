# reactor-webrtc-sys

Low-level FFI bindings to an owned build of Google's WebRTC engine
(`libwebrtc`). This crate is the unsafe boundary — opaque handles and
`extern "C"` declarations implemented by the C++ glue in `glue/`.

**Most users should depend on [`reactor-webrtc`](https://crates.io/crates/reactor-webrtc)
instead**, which wraps this crate in a safe, idiomatic API.

## When to use this crate directly

- You are building a new safe wrapper layer.
- You need direct access to the raw C ABI for interop with another FFI consumer.

## Native library resolution

The build script automatically downloads the correct `libwebrtc` prebuilt for
your target (`cargo build` just works for all published targets). Override with:

| Variable | Effect |
|----------|--------|
| `REACTOR_WEBRTC_LIB_DIR=/path` | Use a local build or extracted prebuilt |
| `REACTOR_WEBRTC_PREBUILT_URL=https://…` | Use a specific archive URL |

`cargo check` and rlib builds always succeed without a native library;
only the final link of a binary or test requires it.

## ABI

The C ABI is defined in `glue/reactor_webrtc.cpp` and documented by the
`extern "C"` block in `src/lib.rs`. All handles are opaque; lifetimes are
managed by explicit `_destroy` calls on the C++ side.

## License

Apache-2.0 — see [LICENSE](../../LICENSE).

Upstream WebRTC is BSD-3-Clause + the WebRTC patent grant; attribution recorded
in the SBOM published with every prebuilt release.
