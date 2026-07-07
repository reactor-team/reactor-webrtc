# webrtc-build — our owned libwebrtc build pipeline

Produces the per-target `libwebrtc` static library (+ public headers, and later
the Android Java companion) that `reactor-webrtc-sys` links, from Google's
upstream WebRTC pinned in `../WEBRTC_VERSION`. Output is published as versioned,
checksummed prebuilt archives (+ SBOM) to our artifact host; nothing here is
committed (see `../.gitignore`).

## Pipeline

1. **Fetch** — `build.sh` clones `depot_tools`, `fetch webrtc`, and
   `gclient sync` to the pinned `WEBRTC_BRANCH`/`WEBRTC_COMMIT`. It prints the
   resolved commit so it can be locked in `WEBRTC_VERSION`.
2. **Patch** — applies `patches/*.patch` (`git apply --3way`) deterministically:
   namespace/symbol prefixing, the synthetic/headless ADM hooks, controlled
   BoringSSL exposure, and the Android Java repackaging into our namespace
   (replacing `livekit.org.webrtc`). Empty for now.
3. **Configure + compile** — `gn gen` with our args (below) + `ninja -C … webrtc`
   → a single static `obj/libwebrtc.a`.
4. **Package** — `package.sh` stages the lib + mirrors the public headers, makes
   a `.tar.zst` + `.sha256`, writes a per-target `*.manifest.json`, and generates
   a CycloneDX `*.sbom.json` (`sbom.sh`) of the third_party components actually
   compiled into the lib (∧ `Shipped: yes`) with their versions + licenses.
5. **Publish** — `publish.sh` cuts a **GitHub Release** (tag
   `webrtc-<milestone>-<commit>-p<patch>`) and uploads every per-target asset.
   `reactor-webrtc-sys` (build.rs mode 2) consumes them via their stable
   release-download URLs:
   `…/releases/download/<tag>/reactor-webrtc-<os>-<arch>-<profile>.tar.zst`
   (+ the matching `.sha256`).

## Audio/network processing in the pipeline

- **Bandwidth estimation** (GoogCC / send-side BWE) is compiled into the
  umbrella and active for media by default.
- **AEC3 + noise suppression + AGC + high-pass filter** are enabled (via a
  `BuiltinAudioProcessingBuilder` APM) for the **platform-ADM** factory
  (`with_platform_adm`, real mic). The **synthetic** ADM stays passthrough
  (bit-exact PCM push, e.g. server forwarding) — no APM.

## gn args (implemented in `build.sh::gn_args`)

Base (all targets): `is_debug=<profile>`, `is_component_build=false` (one static
lib), `rtc_include_tests/examples/tools=false`, `rtc_enable_protobuf=true`,
`use_rtti=true`, **`use_custom_libcxx=false`** (link the *platform* libc++ so it
interops with our Rust/cc glue and the app — mixing libc++ is a classic crash
source), `rtc_libvpx_build_vp9=true`, `treat_warnings_as_errors=false`,
`target_os`/`target_cpu`.

Per-OS: macOS/iOS use hardware H.264 (VideoToolbox) so `rtc_use_h264=false`; iOS
adds `ios_enable_code_signing=false` + `target_environment=device|simulator`;
Linux adds `use_sysroot=true`/`rtc_use_pipewire=false`. See `build.sh` for the
exact list (this is where target tuning lives).

## Run locally (macOS arm64)

```bash
# Prereqs: Xcode + command-line tools, ~40 GB free disk, good bandwidth.
./webrtc-build/build.sh mac arm64 release      # fetch + patch + gn + ninja
brew install zstd
./webrtc-build/package.sh mac arm64 release     # → dist/reactor-webrtc-mac-arm64-release.tar.zst (+ .sha256)
```

The first `fetch` downloads tens of GB and the compile takes a while.

### Verify the build actually links (real-link proof)

`reactor-webrtc-sys` compiles a small C++ glue (`crates/reactor-webrtc-sys/glue/`)
against the built lib and exposes a self-test. Point the sys crate at the
staged output and run its test:

```bash
REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
  cargo test -p reactor-webrtc-sys -- --nocapture
# libwebrtc linked OK — 8 codecs: opus,G722,PCMU,PCMA,VP8,AV1,VP9,VP9
```

This links the Rust test binary against `libwebrtc.a` + the platform
frameworks and runs real WebRTC code (the builtin audio **and video** encoder
factories — VP8/VP9/AV1; H.264 is off via `rtc_use_h264=false`). Notes:

- The glue is compiled as **C++20** (this milestone's public headers use
  `std::span`).
- `libwebrtc.a` is linked **whole-archive** because it is one monolithic
  `complete_static_lib` with back-references between members; `-dead_strip`
  trims what the FFI doesn't use.
- Without `REACTOR_WEBRTC_LIB_DIR`/`_PREBUILT_URL` the test is `cfg`-gated out,
  so a plain `cargo test`/`check` stays green.

## Target matrix

Green today: `macos arm64/x64` · `ios device+sim` · `linux x64/arm64` ·
`android arm64` · `windows x64`. (`visionos` has no upstream gn `target_os`;
more android ABIs are a matrix add.)

**CI split**: the fast public checks (fmt / check / clippy, dev-mode) run on
GitHub Actions (`.github/workflows/ci.yml`). The heavy per-target libwebrtc
builds also run on **GitHub Actions** (`.github/workflows/webrtc-build.yml`): a
`targets`-driven matrix over `macos-15` (mac + iOS device/sim; Xcode 16),
`ubuntu-latest` (linux x64 + android + linux/arm64 cross) and `windows-latest`.
The ~30GB+ checkout fits after the workflow's disk-cleanup step. Windows uses a
dedicated PowerShell path (`build.ps1`) — depot_tools' CIPD bootstrap breaks
under Git Bash. `workflow_dispatch` with `publish=true` cuts a Release.

Per-target toolchain notes (why each differs) live at the top of `build.sh`
(POSIX) and `build.ps1` (Windows).

## Status

- ✅ `gn args` + `build.sh` (fetch→patch→gn→ninja→assemble) and `package.sh`
  (headers + archive + checksum + manifest) implemented.
- ✅ CI: fast checks + heavy per-target builds on GitHub Actions
  (`.github/workflows/ci.yml`, `.github/workflows/webrtc-build.yml`). All matrix
  targets build + package + upload green (`branch-heads/7907`, commit locked in
  `WEBRTC_VERSION`).
- ✅ Real-link proof: `reactor-webrtc-sys` glue links + runs against the lib on
  macOS arm64 (the workflow's native lib-link test).
- ⏳ TODO: prebuilt download+checksum (`build.rs` mode 2); publish via GitHub
  Releases (`publish.sh`, wired into the workflow's publish job); patch series
  (namespace, ADM, Android Java).
- ⏳ Follow-ups from the build bring-up:
  - **Linux/Android/Windows lib-link test** — those libs use the bundled libc++
    (linux/android) / MSVC STL (windows); the native lib-link test runs on macOS
    arm64 only for now. To test linux, the sys crate's glue must be compiled with
    libc++ and the link's system-lib list completed.
  - **SBOM on Windows** — `sbom.sh` is bash+python (POSIX targets only); port to
    PowerShell/python for `build.ps1`.
  - **linux/arm64 is cross-built** (no linux-arm64 bundled clang); consuming its
    prebuilt needs the libc++ glue work above.

> Upstream WebRTC build docs (and LiveKit's open build scripts as *reference
> only*, not a dependency) inform this recipe.
