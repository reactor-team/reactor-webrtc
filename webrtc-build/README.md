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
   a `.tar.zst` + `.sha256`, and writes a per-target `*.manifest.json` for the
   prebuilt index that `REACTOR_WEBRTC_PREBUILT_URL` points at.

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

## Target matrix

`linux x64/arm64` · `macos arm64/x64` · `ios device+sim` ·
`android arm64/armv7/x86_64` · `windows x64` · `visionos device+sim`
(as the toolchain allows). CI builds **macOS arm64 on push**; the full matrix
runs via `workflow_dispatch (run_all=true)` — move it to dedicated/self-hosted
runners (sccache/RBE) because hosted runners are tight on disk/time.

## Status

- ✅ `gn args` + `build.sh` (fetch→patch→gn→ninja→assemble) and `package.sh`
  (headers + archive + checksum + manifest) implemented.
- ✅ CI wired: macOS arm64 on push, full matrix on dispatch.
- ⏳ TODO: verify/lock the milestone in `WEBRTC_VERSION`; first green macOS arm64
  build; publish to the CDN host + index manifest (replace `upload-artifact`);
  the patch series (namespace, ADM, Android Java); SBOM.

> Upstream WebRTC build docs (and LiveKit's open build scripts as *reference
> only*, not a dependency) inform this recipe.
