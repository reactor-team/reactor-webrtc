# libwebrtc patch series

Our deterministic patches on top of upstream Google WebRTC, applied by the build
in order after syncing the pinned ref.

- **Applied by:** `webrtc-build/build.sh` (POSIX) and `webrtc-build/build.ps1`
  (Windows), with `git apply --3way` (Windows normalizes the patched files to LF
  first — the checkout is CRLF).
- **Pinned ref:** `WEBRTC_BRANCH`/`WEBRTC_COMMIT` in `../../WEBRTC_VERSION`.
- **Versioning:** bump **`REACTOR_PATCH_LEVEL`** in `../../WEBRTC_VERSION`
  whenever this series changes (even without a milestone bump). It becomes the
  `-p<level>` suffix of the release tag `webrtc-<milestone>-<commit>-p<level>`,
  so consumers can tell two builds of the same upstream commit apart.
- **`--3way`:** on upstream drift `git apply` falls back to a 3-way merge against
  the blobs the patch references, so a patch keeps applying across minor upstream
  changes (and fails loudly, rather than silently mis-applying, if it can't).

Keep patches **minimal and well-commented** — every hunk should be greppable
back to this file (each carries a `Reactor:` comment). Prefer doing things in the
**glue or gn args** over patching upstream where possible (see "Not patches"
below); a patch is only warranted when neither can express the change.

---

## Applied

### 0001 — add builtin codec factories

`0001-webrtc-add-builtin-codec-factories.patch` · touches `BUILD.gn` (+6 lines)

**What.** Adds four deps to the `//:webrtc` umbrella target:

```
api/audio_codecs:builtin_audio_encoder_factory
api/audio_codecs:builtin_audio_decoder_factory
api/video_codecs:builtin_video_encoder_factory
api/video_codecs:builtin_video_decoder_factory
```

**Why.** The `//:webrtc` umbrella is built as a `complete_static_lib` — the one
monolithic `libwebrtc.a` we ship. Upstream's umbrella pulls in
`create_peerconnection_factory` / `enable_media` but *not* these "leaf"
convenience factories, so a consumer linking only `libwebrtc.a` gets
`undefined symbol: webrtc::CreateBuiltinVideo{Encoder,Decoder}Factory()` (and the
audio equivalents). Our glue calls exactly those helpers to build a
`PeerConnectionFactory`, so they must be in the archive.

**How it works.** Adding them to the umbrella's `deps` pulls each factory **and
its transitive closure** into the `complete_static_lib`: the simulcast encoder
adapter, the internal VP8/VP9/AV1 software codecs, the software-fallback wrapper,
the builtin audio codecs (Opus, G722, PCMU/PCMA), etc. That is why the final lib
must be linked **whole-archive** (`static:+whole-archive=webrtc` in `build.rs`) —
the members have back-references the linker won't otherwise revisit — with a
final `-dead_strip`/`--gc-sections` to drop what the FFI doesn't use.

**Verify.** The `reactor-webrtc-sys` lib-link test prints the live factory codec
list, e.g. `opus, G722, PCMU, PCMA, VP8, AV1, VP9`. (H.264 is intentionally off —
`rtc_use_h264=false`; macOS/iOS use hardware H.264 via VideoToolbox instead.)

---

## Planned (not yet authored — need their target builds to validate)

- **Symbol isolation** — keep WebRTC's C++ symbols from clashing when a consumer
  also links another libwebrtc in the same address space. The cdylib
  (`reactor-sdk-core`) already exports only `reactor_webrtc_*`; for static-lib
  consumers the plan is hidden visibility + a localize pass over `libwebrtc.a`.
  To author + validate on the Linux/Windows builds.
- **Android Java namespace** — repackage WebRTC's `org.webrtc` Java classes and
  the `JNI_OnLoad`/registration into our own namespace. This is a tree-wide
  rename applied during the Android build (the earlier PoC leaned on LiveKit's
  `livekit.org.webrtc`, which we must not ship). To author + validate once the
  Android Java companion is wired.

---

## Not patches (done in the glue or gn args)

These are deliberately **not** upstream patches — recording them here so nobody
goes looking for a patch that doesn't exist:

- **Synthetic / headless ADM** — a custom `AudioDeviceModule` that pushes/pulls
  bit-exact PCM (server forwarding, tests). Implemented in
  `crates/reactor-webrtc-sys/glue/`, selectable at runtime via `AdmMode`.
- **Audio processing chain** — AEC3 + noise suppression + AGC + high-pass filter,
  enabled via a `BuiltinAudioProcessingBuilder` APM for the **platform-ADM**
  factory (real mic). The synthetic ADM stays passthrough (no APM). In the glue.
- **Bandwidth estimation** (GoogCC / send-side BWE) — already compiled into the
  umbrella and active by default; no change needed.
- **Desktop capture disabled** — `rtc_use_x11=false` + `rtc_use_pipewire=false`
  gn args (a calling SDK needs no screen capture, and this drops the libX11
  dependency), not a patch.
- **Bundled libc++ packaging** (linux/android, ABI namespace `__Cr`) — handled by
  `package.sh` + `build.rs`, see the parent `README.md` → "Bundled libc++".
