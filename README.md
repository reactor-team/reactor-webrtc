# reactor-webrtc

Safe Rust API and Python bindings over an owned build of Google's WebRTC engine.
The low-level FFI lives in `reactor-webrtc-sys`; `reactor-webrtc-py` exposes
the API to Python as a self-contained wheel (no native runtime dependency).

```
reactor-webrtc/
├── WEBRTC_VERSION              pinned upstream milestone + our patch level
├── crates/
│   ├── reactor-webrtc-sys/     unsafe FFI + C++ glue (glue/) + build.rs
│   ├── reactor-webrtc/         safe, idiomatic Rust API
│   └── reactor-webrtc-py/      PyO3/Maturin Python bindings (reactor-webrtc wheel)
├── webrtc-build/               our build pipeline (depot_tools + gn/ninja)
│   ├── build.sh                POSIX build: fetch + patch + gn + ninja + assemble
│   ├── build.ps1               Windows build+package (PowerShell; native depot_tools)
│   ├── package.sh              archive + checksum + manifest for prebuilts
│   ├── sbom.py                 CycloneDX SBOM generator (cross-platform core)
│   ├── sbom.sh                 SBOM wrapper for the POSIX builds
│   ├── publish.sh              cut a GitHub Release with the per-target assets
│   └── patches/                deterministic patch series (see patches/README.md)
├── .github/workflows/ci.yml             fast checks (fmt / check / clippy)
├── .github/workflows/webrtc-build.yml   per-target libwebrtc builds (debug CI + manual release)
├── .github/workflows/lib-link-test.yml  fast lib-link test against a prebuilt
└── .github/workflows/publish.yml        crates.io + PyPI publish (triggered by semver tag)
```

## Python bindings (`reactor-webrtc-py`)

`crates/reactor-webrtc-py` exposes the Rust API to Python via
[PyO3](https://pyo3.rs) + [Maturin](https://maturin.rs). The wheel embeds
`libwebrtc` statically - no separate native dependency at runtime.

```python
import reactor_webrtc as rw

factory = rw.PeerConnectionFactory()

obs = rw.PeerConnectionObserver()
obs.on_ice_candidate     = lambda c: ...
obs.on_connection_state_change = lambda s: ...

pc = factory.create_peer_connection(rw.RtcConfiguration(), obs)
offer = pc.create_offer()
pc.set_local_description(offer)

# Stats (RTCStatsReport)
report = pc.get_stats()
for pair in report.candidate_pairs:
    print(pair.state, pair.current_round_trip_time_s)
```

**Building locally** (requires a prebuilt `libwebrtc`):

With [mise](https://mise.jdx.dev) (recommended - pins the whole toolchain):

```bash
mise install                 # rust (via rust-toolchain.toml) + uv/ruff/maturin/nextest/shellcheck
make check                   # fmt-check + cargo check + clippy (no native lib needed)

export REACTOR_WEBRTC_LIB_DIR=/path/to/libwebrtc   # a prebuilt is required to link/test
make test                                          # cargo nextest run + doctests
mise run //crates/reactor-webrtc-py:build          # build the wheel
mise run //crates/reactor-webrtc-py:test           # pytest the wheel
```

`make help` works at the repo root and in each module directory (`crates/reactor-webrtc-py/`, `webrtc-build/`).

Or drive the tools directly:

```bash
# Point at an extracted prebuilt or a local build output
export REACTOR_WEBRTC_LIB_DIR=/path/to/libwebrtc

uv venv .venv --python 3.12
uv pip install maturin pytest

maturin build --manifest-path crates/reactor-webrtc-py/Cargo.toml
pip install target/wheels/reactor_webrtc-*.whl

pytest crates/reactor-webrtc-py/tests/ -v
```

**Exposed types** (see `crates/reactor-webrtc-py/reactor_webrtc.pyi` for the
full typed surface):

| Type | Description |
|------|-------------|
| `PeerConnectionFactory` | Thread pool + media engine factory |
| `PeerConnection` | Offer/answer, ICE, tracks, data channels, stats |
| `PeerConnectionObserver` | Callbacks: state change, ICE candidate, track, data channel |
| `DataChannel` | Reliable/unreliable messaging over SCTP |
| `Track` / `EncodedVideoTrack` | Local media sources |
| `Transceiver` | RTP send/recv direction + MID |
| `StatsReport` | `inbound_rtp`, `outbound_rtp`, `candidate_pairs` |
| `SessionDescription`, `IceCandidate`, `RtcConfiguration` | Signaling types |

Requires **Python ≥ 3.10** (stable ABI `abi3-py310`).

## How the native library is resolved

`reactor-webrtc-sys`'s build script links the native `libwebrtc` in one of three
modes (priority order):

1. Nothing set - **auto-detect**: `build.rs` derives the correct prebuilt URL
   from the baked-in version tag and the Cargo target triple, downloads +
   verifies + links automatically. `cargo add reactor-webrtc && cargo build`
   just works for all published targets.
2. `REACTOR_WEBRTC_LIB_DIR=/path` - link a locally built/extracted lib
   (packaged layout `<dir>/lib` + `<dir>/include`, or a bare dir + optional
   `REACTOR_WEBRTC_INCLUDE_DIR`).
3. `REACTOR_WEBRTC_PREBUILT_URL=…` (+ `…_SHA256`) - download a specific
   prebuilt archive (overrides the auto-detected URL).

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

## Status

- ✅ Safe Rust API (`reactor-webrtc`) over the owned `libwebrtc` build.
- ✅ `build.rs` with 3-mode native resolution (local dir, prebuilt URL, or API-only).
- ✅ Full build pipeline (`webrtc-build/`): fetch → patch → gn → ninja → package → publish,
  pinned to `branch-heads/7907` in `WEBRTC_VERSION`.
- ✅ All matrix targets build and are published as a GitHub Release.
- ✅ FFI glue links and runs against the lib (lib-link tests on macOS arm64 and Linux x64 in CI).
- ✅ Python wheel (`reactor-webrtc-py`): full signaling API, data channels, media tracks,
  stats, and a loopback test suite - built and tested in CI.
- ✅ crates.io + PyPI publish CI (`publish.yml`), triggered by semver tag.

## Licensing

This repository is **Apache-2.0** licensed - see [`LICENSE`](LICENSE).

Upstream WebRTC is **BSD-3-Clause + the WebRTC patent grant**; redistributing
prebuilts is permitted with attribution (recorded in the SBOM + `NOTICE.md`).
