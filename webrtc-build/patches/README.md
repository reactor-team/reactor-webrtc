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

## Planned

- namespace/symbol prefixing, synthetic/headless ADM, Android Java namespace
  (replacing `livekit.org.webrtc`), controlled BoringSSL exposure.
