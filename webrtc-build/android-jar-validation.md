# Android JAR relocation validation

The p6 Android archive contains `org/webrtc/WebRtcClassLoader.class`, while its
native image looks up `inc.reactor.org.webrtc.WebRtcClassLoader`. JNI Zero has the
same mismatch. The patch applies Chromium's `dist_jar.renaming_rules` to both
packages using `android_jni_package_prefix`.

Local validation on 2026-09-11:

- The added GN hunk applies cleanly to `sdk/android/BUILD.gn` at pinned upstream
  `a5ddff6086d96fd2356ad6c521525ea2803f7988`.
- Using the R8 CIPD instance pinned by that upstream DEPS
  (`DVH4goXudQ_5x7Anis0boVW2UGuqrsVsIp8Y2qhOTfIC`), both mapping rules were applied
  to the real p6 Android `libwebrtc.jar`. The checker rejects the original and
  accepts the resulting 444 classes.
- Six regression tests pass, including invoking the packager with a missing JAR
  and with the original namespace. Neither failure creates a distributable archive.
- A throwaway Android instrumentation app on an Android 15 arm64 emulator loaded
  the relocated WebRTC and JNI Zero bootstrap classes, loaded the real
  `libreactor_ffi.so`, and called `reactor_abi_version()` through a JNI probe
  (returned ABI 2). This test passed.
- The full WebRTC source build/p7 archive has not been produced locally. The
  `webrtc-build` workflow rebuilds it from this patch; publishing release
  prebuilts remains a separate workflow action.

For the real-artifact relocation check, the build's R8 operation is equivalent to:

```sh
java -cp third_party/r8/cipd/lib/r8.jar \
  com.android.tools.r8.relocator.RelocatorCommandLine \
  --input original.jar --output relocated.jar \
  --map 'org.webrtc.**->inc.reactor.org.webrtc' \
  --map 'org.jni_zero.**->inc.reactor.org.jni_zero'
python3 webrtc-build/check-android-jar.py relocated.jar /path/to/args.gn
```

Chromium's actual `rename_java_classes.py` also normalizes ZIP permissions after
running R8. `package.sh` validates the output from that GN target, not a manually
renamed substitute.

## Limits of the bootstrap proof

A separate attempt to create/destroy a client with synthetic ADM reached
`WebRtcVoiceEngine` and failed with SIGSEGV. That is later than Java class lookup
and remains part of the Kotlin SDK native integration work; this PR does not claim
working media or client lifecycle.

A JNI helper linked against the FFI must export its own no-op `JNI_OnLoad`
(returning JNI_VERSION_1_6), otherwise Android can resolve the dependency's
`JNI_OnLoad` and invoke WebRTC initialization twice. Only the FFI initializes
WebRTC. The passing bootstrap probe uses that arrangement.

## C++ ABI after bootstrap

The Android archive uses Clang's relative C++ vtable ABI. `reactor-webrtc-sys`
compiles its glue with `-fexperimental-relative-c++-abi-vtables` to match it.
A matching JAR alone is insufficient: compiling the glue with absolute vtables
links successfully but crashes when `WebRtcVoiceEngine` invokes `AddRef` on the
glue-created synthetic audio device.

Regression proof on Android 15 arm64 (NDK 29, API 26, official p7 archive): a JNI
probe loads the matching JAR and native FFI, creates a client with synthetic ADM,
and destroys it. The uncorrected 0.17.1 build crashes at
`WebRtcVoiceEngine::WebRtcVoiceEngine + 692`; the same probe passes with the
relative-vtable flag. The helper exports a no-op `JNI_OnLoad`; the FFI alone
owns WebRTC initialization. Native LOAD segments remain aligned to 16 KB.
