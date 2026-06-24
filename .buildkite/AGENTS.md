<!-- Copyright (c) 2026 Reactor Technologies, Inc. All rights reserved. -->

# Buildkite agents for reactor-webrtc

The heavy lane (`.buildkite/pipeline.yml`) builds our own `libwebrtc` per
target. A WebRTC checkout + build is **large and slow** (~30GB checkout, peak
~60–80GB with `out/`, multi-hour compile), so it needs **dedicated** agents —
not the shared `heavy`/`arm64` service-build queues (which would starve other
jobs and lack disk). Every build step is `skip:`ed until its queue exists.

## Queues & agent spec

| Queue                | Host                         | Disk    | Notes |
|----------------------|------------------------------|---------|-------|
| `webrtc-linux`       | Linux x86_64                 | ≥80 GB  | builds `linux/x64` **and** `android/arm64` (gclient fetches the NDK/SDK) |
| `webrtc-linux-arm64` | Linux arm64 (e.g. Graviton)  | ≥80 GB  | native `linux/arm64` build + tests |
| `webrtc-macos`       | macOS arm64 + full Xcode     | ≥80 GB  | `mac/arm64` and `ios/arm64` |
| `webrtc-windows`     | Windows + VS build tools     | ≥80 GB  | `win/x64` |

Sizing: ≥8 vCPU / ≥16 GB RAM recommended (ninja is parallel; more cores = much
faster). Prefer ephemeral (Buildkite Elastic CI stack on AWS) with a large
ephemeral volume, or a persistent agent that keeps the `webrtc-build/src`
checkout warm across builds to skip the multi-GB `fetch`.

### Per-agent prerequisites

`webrtc-build/build.sh` brings its own toolchain via `depot_tools` (clang, gn,
ninja). The agent host only needs:

- `git`, `python3`, `curl`, and a POSIX shell
- `zstd` (packaging) and `rustup`/`cargo` (the lib-linked `cargo test` steps)
- **macOS**: full Xcode (`xcode-select -s /Applications/Xcode.app`), not just CLT
- **Windows**: Visual Studio C++ build tools + the Windows SDK
- network egress to `*.googlesource.com` / `chromium-*` storage

No system WebRTC deps are required — the build uses a downloaded sysroot
(`use_sysroot=true` on Linux) and depot_tools' clang.

## Registering the pipeline

In the Buildkite dashboard (or via the Buildkite Terraform provider / API):

1. **New Pipeline** → connect the `reactor-team/reactor-webrtc` GitHub repo.
2. Steps source: **"Read steps from repository"** → `.buildkite/pipeline.yml`.
3. Add the GitHub webhook so pushes/PRs trigger builds (the fast public checks
   stay on GitHub Actions; this pipeline is the heavy lane).
4. Secrets/OIDC for the (currently skipped) publish step: an AWS IAM role for
   the `reactor-webrtc` prebuilt bucket, assumed via the
   `aws-assume-role-with-web-identity` plugin (mirrors reactor-cli).

## Enabling a target

Once a queue's agent is online, delete that step's `skip:` line in
`pipeline.yml`. Validate end-to-end on `webrtc-linux` first (it also unblocks
authoring + validating the symbol-isolation and Android-Java-namespace patches,
which need a real Linux/Android `libwebrtc.a`).
