# Vendored OpenH264 public API headers

`codec_api.h`, `codec_app_def.h`, `codec_def.h`, `codec_ver.h` — copied
**verbatim, unmodified** from
[cisco/openh264](https://github.com/cisco/openh264) tag `v2.6.0`
(`codec/api/wels/`), BSD-2-Clause (license text embedded in each file's
header comment, preserved as required).

## Why vendored instead of a source dependency

We never compile OpenH264 — only `dlopen`/`LoadLibraryW` Cisco's official
prebuilt shared library at runtime (see
`../../src/openh264.rs`'s module doc for the licensing rationale) and call
into it through these declarations. `ISVCEncoder`/`ISVCDecoder` are real C++
abstract classes with virtual methods (Itanium/MSVC vtable ABI, not a C
struct-of-function-pointers) — for `EncodeFrame`/`DecodeFrameNoDelay`/etc.
calls through a pointer returned by `WelsCreateSVCEncoder`/`WelsCreateDecoder`
to dispatch to the right vtable slot, our copy of these class declarations
must have **exactly** the method order and signatures the binary was compiled
with. Copying the upstream header verbatim guarantees that; retyping from
memory would not.

## Keep in sync with the pinned version

The OpenH264 version downloaded at runtime is pinned in
`../../src/openh264.rs` (`OPENH264_VERSION`). If that version ever moves,
re-vendor these four files from the matching upstream tag first — don't bump
one without the other.
