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

### 0002 — Android JNI package prefix + compat aliases

`0002-android-jni-package-prefix.patch` · touches `third_party/jni_zero/jni_zero.gni` (+28 lines) and `third_party/jni_zero/codegen/header_common.py` (+16 lines)

**What.** Two inseparable changes applied as one patch:

1. Adds a `declare_args() { android_jni_package_prefix = "" }` GN variable to
   `jni_zero.gni` and wires it as `--package-prefix <value>` into both the
   `generate_jni_registration` and `generate_jni_impl` templates (the two paths
   that invoke `jni_zero.py`). When the arg is non-empty, every JNI-generated
   Java class gets the prefix prepended to its package name.

2. Extends `class_accessors()` in jni_zero's C++ header codegen to emit a
   backward-compat `#define` alias whenever a package prefix is active:

   ```cpp
   // generated (new):
   inline jclass inc_reactor_org_webrtc_Foo_clazz(JNIEnv* env) { … }
   // alias (new, from this patch):
   #define org_webrtc_Foo_clazz inc_reactor_org_webrtc_Foo_clazz
   ```

**Why.** WebRTC's Android SDK ships Java classes under `org.webrtc.*`. Setting
`android_jni_package_prefix = "inc.reactor"` in `build.sh` produces
`inc.reactor.org.webrtc.*` Java classes in `libwebrtc.jar` and the matching
`Java_inc_reactor_org_webrtc_*` JNI symbols in `libwebrtc.a`, avoiding
collisions with any other WebRTC consumer in the same process.

The GN change alone is insufficient: it renames every generated `_clazz` C++
identifier from `org_webrtc_*` to `inc_reactor_org_webrtc_*`, but four static
`.cc` files in `sdk/android/src/jni/` (`encoded_image.cc`, `stats_observer.cc`,
`ice_candidate.cc`, `media_stream.cc`) reference the old names directly and fail
to compile. Patching all four is brittle; emitting the alias in the generator
fixes the root cause once. The two halves only make sense together.

**How it works.** `jni_zero.py` already supports `--package-prefix` natively
(it is used by Cronet for `"internal"`). The missing piece was a GN-level arg
to activate it project-wide. This patch adds that arg; `build.sh` sets it to
`"inc.reactor"` for the Android target. The same mechanism Cronet uses
(`_cronet_renaming_extra_args`) is the model.

**Note.** This patch targets files inside `third_party/jni_zero/`, which lives
in a separate gclient sub-repo. `build.sh` applies it with `patch -p1` (fallback
from `git apply`) from `src/` after `gclient sync` resets jni_zero to its
pinned state.

**Verify.** After an Android build, `jar tf out/android-*/lib.java/sdk/android/libwebrtc.jar`
should list `inc/reactor/org/webrtc/PeerConnection.class` (and equivalents).
`nm libwebrtc.a | grep Java_` should show `Java_inc_reactor_org_webrtc_*` symbols.
Android build completes without `use of undeclared identifier 'org_webrtc_*_clazz'` errors.

---

### 0003 — disable CREL relocations for linux/arm64

`0003-disable-crel-for-arm64.patch` · touches `build/config/compiler/BUILD.gn` (+3/-1 lines)

**What.** Extends an existing upstream exclusion in the `compiler` config so
`current_cpu != "arm64"` is also required before `-Wa,--crel,--allow-experimental-crel`
is added to `cflags` on Linux.

**Why.** Upstream already excludes `arm`/`s390x` from CREL (compact
relocations) because it segfaults there (see the upstream `TODO(crbug.com/376278218)`
and the linked `llvm-project` issue in the patch context) — but not `arm64`.
`reactor-webrtc-sys` links the prebuilt `libwebrtc.a` on Linux arm64 hosts with
**GNU ld**, not lld, and GNU ld does not yet support CREL relocations: a
`libwebrtc.a` compiled with them fails to link there with relocation errors.

**How it works.** One-line change to the existing `if` condition — no new gn
arg, no build.rs change. The linux/arm64 build (cross-compiled from x86_64,
see the parent `README.md` → "Bundled libc++") simply never emits CREL for
its objects, so GNU ld reads a compatible archive.

**Verify.** `reactor-webrtc-sys`'s lib-link test links and runs on Linux
arm64 without relocation errors from the linker.

---

## Planned (not yet authored — need their target builds to validate)

- **Symbol isolation** — keep WebRTC's C++ symbols from clashing when a consumer
  also links another libwebrtc in the same address space. The cdylib
  (`reactor-sdk-core`) already exports only `reactor_webrtc_*`; for static-lib
  consumers the plan is hidden visibility + a localize pass over `libwebrtc.a`.
  To author + validate on the Linux/Windows builds.

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
