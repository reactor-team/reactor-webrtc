<div align="center">

<img src="assets/banner.png" alt="Reactor WebRTC" width="100%" />

**Safe Rust API and Python bindings over an owned build of Google's WebRTC engine.**

[🌐 Reactor](https://reactor.inc) · [⚙️ Runtime](https://github.com/reactor-team/reactor-runtime) · [🛠️ Client SDKs](https://github.com/reactor-team/reactor-client-sdks) · [📖 Cookbook](https://github.com/reactor-team/reactor-cookbook)

[![CI](https://github.com/reactor-team/reactor-webrtc/actions/workflows/ci.yml/badge.svg)](https://github.com/reactor-team/reactor-webrtc/actions/workflows/ci.yml)
[![crates.io: reactor-webrtc](https://img.shields.io/crates/v/reactor-webrtc.svg?label=reactor-webrtc)](https://crates.io/crates/reactor-webrtc)
[![crates.io: reactor-webrtc-sys](https://img.shields.io/crates/v/reactor-webrtc-sys.svg?label=reactor-webrtc-sys)](https://crates.io/crates/reactor-webrtc-sys)
[![PyPI: reactor-webrtc-py](https://img.shields.io/pypi/v/reactor-webrtc.svg?label=reactor-webrtc-py)](https://pypi.org/project/reactor-webrtc/)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

</div>

---

An owned, self-contained build of Google's WebRTC engine, wrapped in a safe
Rust API with Python bindings on top — no system `libwebrtc`, no extra
native runtime dependency, just `cargo add` or `pip install`.

## Getting started

**🦀 Rust** — `cargo add reactor-webrtc`. `cargo build` auto-downloads the
matching prebuilt `libwebrtc` for your target, nothing else to install. Quick
start and full API reference:
[`crates/reactor-webrtc/README.md`](crates/reactor-webrtc/README.md).

**🐍 Python** — `pip install reactor-webrtc` (Python ≥ 3.10; the wheel embeds
`libwebrtc` statically, no separate native dependency). Quick start and full
API reference: [`crates/reactor-webrtc-py/README.md`](crates/reactor-webrtc-py/README.md).

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — crate layering,
  ownership/lifetime, the one-factory-per-process constraint, and the
  threading model (Rust and Python).
- [`docs/configuration.md`](docs/configuration.md) — every
  `RtcConfiguration` field and `set_bitrate`: ICE servers/policy, port
  range, bundle policy, TCP candidates, ICE timeouts, congestion-control
  bitrate limits.
- [`docs/frame-metadata.md`](docs/frame-metadata.md) — per-frame metadata
  trailers and custom encoded-frame transforms.
- [`docs/README.md`](docs/README.md) is the index if you're looking for
  something specific.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for dev setup, code style, commit
conventions, and how to open a pull request.

## Licensing

This repository is **Apache-2.0** licensed - see [`LICENSE`](LICENSE).

Upstream WebRTC is **BSD-3-Clause + the WebRTC patent grant**; redistributing
prebuilts is permitted with attribution (recorded in the SBOM + `NOTICE.md`).
