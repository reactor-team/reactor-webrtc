# webrtc-build — our owned libwebrtc build pipeline

Produces the per-target `libwebrtc` static libraries (and the Android Java
companion) that `reactor-webrtc-sys` links, from Google's upstream WebRTC pinned
in `../WEBRTC_VERSION`. The output is published as versioned, checksummed
prebuilt archives (+ SBOM) to our artifact host; nothing here is committed.

## Pipeline

1. **Fetch** — `depot_tools` + `fetch webrtc`, checked out at `WEBRTC_VERSION`.
2. **Patch** — apply `patches/*.patch` deterministically (namespace/symbol
   prefixing to avoid clashes with an app's own WebRTC, ADM hooks for the
   synthetic/headless device, controlled BoringSSL exposure for our TLS story,
   and the Android Java repackaging into our namespace — replacing
   `livekit.org.webrtc`).
3. **Configure + compile** — `gn gen` + `ninja` with our args per
   `(os, arch, profile)` → a static `libwebrtc.a`/`.lib`.
4. **Package** — `package.sh` archives the lib + headers per target, computes
   SHA-256, and updates the prebuilt index manifest that
   `REACTOR_WEBRTC_PREBUILT_URL` points at.

## Target matrix

`linux x64/arm64` · `macos arm64/x64` · `ios device+sim` ·
`android arm64/armv7/x86_64` · `windows x64` · `visionos device+sim`
(as the toolchain allows).

## Reality

Builds are large and slow (hours, tens of GB). Use dedicated/self-hosted
runners, `ccache`/`sccache`, ideally Reclient/RBE; cache `gn`/`ninja` output.
Nightly milestone-tracking builds; release builds on `WEBRTC_VERSION` bump.

> **Status:** skeleton. `build.sh` / `package.sh` document the intended steps
> and are not yet runnable end-to-end. Upstream WebRTC build docs (and LiveKit's
> open build scripts as *reference only*, not a dependency) inform the recipe.
