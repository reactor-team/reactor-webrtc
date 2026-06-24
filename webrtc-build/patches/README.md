# Our deterministic libwebrtc patch series (applied in order by build.sh).
#
# build.sh applies every patches/*.patch with `git apply --3way` after syncing
# the pinned ref, so they survive minor upstream drift across milestones.
# Bump REACTOR_PATCH_LEVEL in ../../WEBRTC_VERSION whenever this series changes.

## Current

- **0001-webrtc-add-builtin-codec-factories.patch** — add the builtin audio/video
  codec factories (`api/{audio,video}_codecs:builtin_*_factory`) to the `//:webrtc`
  umbrella's deps. The umbrella is `complete_static_lib`, so this pulls the
  factories (and their transitive closure: simulcast adapter, internal codecs,
  software-fallback wrapper, …) into the single `libwebrtc.a`, exposing
  `webrtc::CreateBuiltinVideo{Encoder,Decoder}Factory()` to consumers.

## Planned (not yet validated — need their target builds)

- **symbol isolation** — keep WebRTC's C++ symbols from clashing when a
  consumer also links another libwebrtc. The cdylib (`reactor-sdk-core`) already
  exports only `reactor_webrtc_*`; for static-lib consumers the plan is hidden
  visibility + a localize pass. Exercise on the Linux/Windows builds.
- **Android Java namespace** — repackage WebRTC's `org.webrtc` Java +
  `JNI_OnLoad`/registration into our namespace (the PoC used LiveKit's
  `livekit.org.webrtc`; we must not). This is a tree-wide rename applied during
  the Android build — author + validate once the Android target is wired.
- controlled BoringSSL exposure; synthetic/headless ADM is implemented in the
  glue (not a patch).

> Note: the synthetic ADM, the builtin codec factories (0001), and the audio
> processing chain (AEC3 + noise suppression + AGC, enabled for the platform
> ADM) are wired in the glue / build, not as upstream patches.
