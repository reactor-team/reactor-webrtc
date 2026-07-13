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
2. **Patch** — applies `patches/*.patch` (`git apply --3way`) deterministically
   after the sync, so they survive minor upstream drift. Currently one patch
   (builtin codec factories); the full series + rationale is in
   [`patches/README.md`](patches/README.md). On Windows the patch targets are
   normalized to LF first (the checkout is CRLF).
3. **Configure + compile** — `gn gen` with our args (below) + `ninja -C … webrtc`
   → a single static `obj/libwebrtc.a` (`obj/webrtc.lib` on Windows).
4. **Package** — `package.sh` (POSIX) / `build.ps1` (Windows) stages the lib +
   mirrors the public headers, makes a `.tar.zst` + `.sha256`, writes a
   per-target `*.manifest.json`, and generates a CycloneDX `*.sbom.json` (via the
   cross-platform `sbom.py`) of the third_party components actually compiled into
   the lib (∧ `Shipped: yes`) with their versions + licenses. On linux/android it
   also stages the bundled libc++ (see "Bundled libc++" below).
5. **Publish** — `publish.sh` cuts a **GitHub Release** (tag
   `webrtc-<milestone>-<commit>-p<patch>`) and uploads every per-target asset.
   `reactor-webrtc-sys` (build.rs mode 2) consumes them via their stable
   release-download URLs:
   `…/releases/download/<tag>/reactor-webrtc-<os>-<arch>-<profile>.tar.zst`
   (+ the matching `.sha256`). The current release is
   [`webrtc-7907-a5ddff60-p1`](https://github.com/reactor-team/reactor-webrtc/releases/tag/webrtc-7907-a5ddff60-p1).

## Audio/network processing in the pipeline

- **Bandwidth estimation** (GoogCC / send-side BWE) is compiled into the
  umbrella and active for media by default.
- **AEC3 + noise suppression + AGC + high-pass filter** are enabled (via a
  `BuiltinAudioProcessingBuilder` APM) for the **platform-ADM** factory
  (`with_platform_adm`, real mic). The **synthetic** ADM stays passthrough
  (bit-exact PCM push, e.g. server forwarding) — no APM.

## gn args (implemented in `build.sh::gn_args`, mirrored in `build.ps1`)

Base (all targets): `is_debug=<profile>`, `is_component_build=false` (one static
lib), `rtc_include_tests/examples/tools=false`, `rtc_enable_protobuf=true`,
`use_rtti=true`, `rtc_libvpx_build_vp9=true`, `treat_warnings_as_errors=false`,
`target_os`/`target_cpu`.

The **C++ standard library** choice (`use_custom_libcxx`) is per-OS, because the
"platform" stdlib and how a modern one is reached differ by target — and it must
match the stdlib the consumer's glue is compiled/linked with:

| OS | `use_custom_libcxx` | stdlib the glue links | key extra args |
|----|--------------------|-----------------------|----------------|
| macOS | `false` | platform libc++ (Xcode) | `rtc_use_h264=false` (VideoToolbox) |
| iOS | `false` | platform libc++ | `ios_enable_code_signing=false`, `target_environment=device\|simulator`, `rtc_use_h264=false` |
| Linux | **`true`** | **bundled libc++ (`__Cr`)** | bundled clang, `use_sysroot=true`, `rtc_use_x11=false`, `rtc_use_pipewire=false` |
| Android | **`true`** | **bundled libc++ (`__Cr`)** | NDK, `android_static_analysis=off`, `rtc_use_h264=false` |
| Windows | `false` | MSVC STL | (`build.ps1`, native depot_tools) |

Why the Linux/Android exceptions:

- **Linux** must use WebRTC's *bundled* clang (a Chromium fork with flags stock
  clang lacks — `--crel`, `-fno-lifetime-dse`, …) and its *bundled* libc++ (the
  pinned debian sysroot's libstdc++ is too old for WebRTC's C++20). The bundled
  clang is published for x86_64 hosts only, so **linux/arm64 is cross-compiled
  from x86_64** against the arm64 sysroot. **Desktop capture is disabled**
  (`rtc_use_x11=false` + `rtc_use_pipewire=false`) so there's no libX11/PipeWire
  link dependency.
- **Android** uses the bundled libc++ because it ships the libunwind that
  Chromium's `--unwindlib=none` link otherwise omits (undefined `_Unwind_*`);
  `android_static_analysis=off` avoids the Java validate-deps step that needs
  `autoninja`.

`symbol_level=1` everywhere. See `build.sh` for the exact list — that is where
target tuning lives.

## Bundled libc++ (linux/android)

Those targets link WebRTC's bundled libc++, whose ABI namespace is **`__Cr`**
(not the platform libc++'s `__1`), and it is a *separate* static lib (not folded
into `libwebrtc.a`). So a consumer's C++ glue must be compiled against those
exact headers and linked against those archives. `package.sh` therefore ships,
under `include/` + `lib/`:

- the bundled `libc++`/`libc++abi` headers (extensionless — `<vector>`,
  `__config`, …) + the generated `__config_site` (pins `__Cr`);
- `libc++.a` / `libc++abi.a`, **repacked fat** (ninja emits them as thin
  archives that reference `.o` paths, unusable off-host).

`reactor-webrtc-sys`'s `build.rs` compiles the glue with
`clang -nostdinc++ -isystem <bundled libc++>` (+ `-D_LIBCPP_HARDENING_MODE=…`,
`-DNDEBUG` to match the release lib) and links the fat `libc++.a`/`libc++abi.a`.
**Consumer requirement on linux**: a recent **clang (≥ 21)** — the bundled libc++
headers (from Chromium's llvm-23 clang) use builtins (`__builtin_popcountg`, …)
absent from older clang.

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

- ✅ Full pipeline: `build.sh`/`build.ps1` (fetch→patch→gn→ninja→assemble),
  `package.sh`, `sbom.py`, `publish.sh`.
- ✅ CI on GitHub Actions: fast checks (`ci.yml`) + heavy per-target builds
  (`webrtc-build.yml`). **All matrix targets build + package + upload green**
  (`branch-heads/7907`, commit locked in `WEBRTC_VERSION`), and are **published**
  as GitHub Release `webrtc-7907-a5ddff60-p1`.
- ✅ `build.rs` mode 2 (prebuilt download + sha256 verify + extract) implemented
  and consumed from the published release.
- ✅ Real-link proof: `reactor-webrtc-sys` glue links + runs against the lib on
  **macOS arm64 and linux x64** (native lib-link test in the build workflow;
  `lib-link-test.yml` iterates it against a prebuilt without a rebuild).
- ✅ Linux/Android bundled-libc++ glue interop (see "Bundled libc++" above);
  SBOM on all targets incl. Windows (`sbom.py`).
- ⏳ Follow-ups:
  - **Android/Windows lib-link test** — android uses the bundled libc++ (via the
    NDK clang) and windows the MSVC STL; neither is run in CI yet. linux/arm64 is
    cross-built so it isn't lib-link tested either (its interop mirrors x64).
  - **Patch series** — symbol isolation + Android Java-namespace repackaging (see
    [`patches/README.md`](patches/README.md)).

> Upstream WebRTC build docs (and LiveKit's open build scripts as *reference
> only*, not a dependency) inform this recipe.
