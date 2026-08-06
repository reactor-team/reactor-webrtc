# Documentation

Deep dives that don't fit in a crate's own README. Start with
[architecture.md](architecture.md) if you're new here — everything else
builds on it.

| Guide | Covers |
|-------|--------|
| [architecture.md](architecture.md) | Crate layering, ownership/lifetime, the one-factory-per-process constraint, threading model (Rust and Python) |
| [configuration.md](configuration.md) | Every `RtcConfiguration` field and `set_bitrate` — ICE servers/policy, port range, bundle policy, TCP candidates, ICE timeouts, congestion-control bitrate limits |
| [frame-metadata.md](frame-metadata.md) | Per-frame metadata trailers and custom encoded-frame transforms (inspect, drop, or rewrite a frame in flight) |

For the native build pipeline that produces `libwebrtc` itself, see
[`webrtc-build/README.md`](../webrtc-build/README.md) and the
[patch series](../webrtc-build/patches/README.md) it applies. For each
crate's own quick start and API table, see its README:
[`reactor-webrtc`](../crates/reactor-webrtc/README.md) (Rust),
[`reactor-webrtc-py`](../crates/reactor-webrtc-py/README.md) (Python),
[`reactor-webrtc-sys`](../crates/reactor-webrtc-sys/README.md) (the FFI
layer). For contributing to this repo, see
[`CONTRIBUTING.md`](../CONTRIBUTING.md).
