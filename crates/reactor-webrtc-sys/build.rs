//! Build script for `reactor-webrtc-sys`.
//!
//! Resolves the native `libwebrtc` (our owned build — see ../../webrtc-build)
//! in one of three modes, in priority order:
//!
//!   1. `REACTOR_WEBRTC_LIB_DIR=/path`  — link a locally-built/extracted lib
//!      directory (contributors building WebRTC from source). The dir is either
//!      a packaged layout (`<dir>/lib/libwebrtc.a` + `<dir>/include`) or a bare
//!      dir containing `libwebrtc.a` (headers from `REACTOR_WEBRTC_INCLUDE_DIR`).
//!   2. `REACTOR_WEBRTC_PREBUILT_URL=...` (+ `REACTOR_WEBRTC_PREBUILT_SHA256`)
//!      — download a specific prebuilt archive, verify the checksum, extract
//!      into `OUT_DIR`, and link it.
//!   3. Nothing configured — **auto-detect**: derive the correct prebuilt URL
//!      from the baked-in `PREBUILT_TAG` and the current Cargo target triple,
//!      download + verify + link automatically. This is the default path for
//!      end users: `cargo add reactor-webrtc` + `cargo build` just works for
//!      all published targets (mac, ios, linux, android, windows).
//!      Falls back to API/check-only for unsupported targets.
//!
//! When a native lib is resolved we also compile the C++ glue in `glue/` (the
//! FFI implementation) and emit `cfg(have_libwebrtc)` so link-dependent tests
//! compile only when there is something to link against.
//!
//! The prebuilt archives themselves are produced and published by
//! `../../webrtc-build` (depot_tools + gn/ninja, pinned to ./WEBRTC_VERSION).

use std::env;
use std::path::{Path, PathBuf};

// ── Prebuilt location ─────────────────────────────────────────────────────────
// The repo is public, so release assets download anonymously — no token needed.
const PREBUILT_BASE: &str = "https://github.com/reactor-team/reactor-webrtc/releases/download";

// Fallback tag used when WEBRTC_VERSION is not accessible (e.g. builds from a
// published crate on crates.io). Patched automatically by publish.yml before
// cargo publish — never edit this line manually.
const PREBUILT_TAG_FALLBACK: &str = "webrtc-7907-a5ddff60-p4";

fn main() {
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_LIB_DIR");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_PREBUILT_URL");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_PREBUILT_SHA256");
    // Gate link-dependent tests/examples on an actual native lib being present.
    println!("cargo:rustc-check-cfg=cfg(have_libwebrtc)");

    if let Ok(dir) = env::var("REACTOR_WEBRTC_LIB_DIR") {
        // Watch the key prebuilt files so a restored Rust build cache does not
        // silently keep stale link directives when the prebuilt layout changes
        // (e.g. libc++.a appears for the first time after a p4 rebuild).
        // Cargo only reruns build scripts on env-var changes; watching the files
        // directly handles the case where REACTOR_WEBRTC_LIB_DIR stays the same
        // but its contents are updated.
        let lib = Path::new(&dir).join("lib");
        for name in &["libwebrtc.a", "libc++.a", "libc++abi.a", "build_profile"] {
            println!("cargo:rerun-if-changed={}", lib.join(name).display());
        }
        link(Path::new(&dir));
        return;
    }

    if let Ok(url) = env::var("REACTOR_WEBRTC_PREBUILT_URL") {
        let sha256 = env::var("REACTOR_WEBRTC_PREBUILT_SHA256").ok();
        let dir = download_prebuilt(&url, sha256.as_deref());
        link(&dir);
        return;
    }

    // Mode 3: auto-detect the correct prebuilt from WEBRTC_VERSION (or the
    // baked-in fallback tag) and the current Cargo target triple.
    if let Some(platform) = prebuilt_platform() {
        let tag = prebuilt_tag();
        let asset = format!("reactor-webrtc-{platform}-release.tar.zst");
        let sha_asset = format!("{asset}.sha256");

        let url = format!("{PREBUILT_BASE}/{tag}/{asset}");
        let sha_url = format!("{PREBUILT_BASE}/{tag}/{sha_asset}");

        // Treat the SHA file as an availability probe: if it doesn't resolve
        // (404 = release not yet published, network error, etc.) don't attempt
        // the download. This allows `cargo clippy` / `cargo fmt` to run in
        // API/check-only mode on PRs that bump REACTOR_PATCH_LEVEL before the
        // new prebuilt has been published. Explicit downloads via
        // REACTOR_WEBRTC_PREBUILT_URL bypass this check.
        let sha256 = fetch_sha256(&sha_url);
        if let Some(sha) = sha256 {
            let dir = download_prebuilt(&url, Some(&sha));
            link(&dir);
            return;
        }
        println!(
            "cargo:warning=reactor-webrtc-sys: checksum for {tag}/{sha_asset} not \
             reachable (release not yet published?). \
             API/check only — set REACTOR_WEBRTC_LIB_DIR to link a local build."
        );
    }

    println!(
        "cargo:warning=reactor-webrtc-sys: no prebuilt available for this target. \
         Set REACTOR_WEBRTC_LIB_DIR (local build) or REACTOR_WEBRTC_PREBUILT_URL \
         (custom archive). Building API/check only — final linking will fail."
    );
}

/// Resolve `(lib_dir, include_dir)` from the configured root, link the static
/// lib + its system dependencies, and compile the C++ glue against the headers.
fn link(root: &Path) {
    // Packaged layout produced by webrtc-build scripts:
    //   Unix (build.sh / package.sh): lib/libwebrtc.a  + include/
    //   Windows (build.ps1):          lib/libwebrtc.lib + include/
    // build.ps1 preserves the "lib" prefix (libwebrtc$ext) so that the link
    // name is consistent: on Unix `-lwebrtc` → libwebrtc.a; on MSVC
    // `libwebrtc` → libwebrtc.lib (MSVC does not add a lib prefix itself).
    // Bare layout: <root> holds the lib directly (REACTOR_WEBRTC_LIB_DIR).
    let (lib_dir, include_dir) =
        if root.join("lib/libwebrtc.a").is_file() || root.join("lib/libwebrtc.lib").is_file() {
            (root.join("lib"), root.join("include"))
        } else {
            let inc = env::var("REACTOR_WEBRTC_INCLUDE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| root.join("include"));
            (root.to_path_buf(), inc)
        };

    let is_debug_prebuilt = std::fs::read_to_string(lib_dir.join("build_profile"))
        .ok()
        .map(|s| s.trim() == "debug")
        .unwrap_or(false); // no marker = old prebuilt without the file; treat as release
    compile_glue(&include_dir, is_debug_prebuilt);

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // libwebrtc.a / libwebrtc.lib is a monolithic archive (gn complete_static_lib)
    // with back-references between members. +whole-archive loads every member so
    // all symbols resolve; -dead_strip / /OPT:REF trims unused code at final link.
    // On Unix rustc prepends "lib" → libwebrtc.a; on MSVC "libwebrtc" → libwebrtc.lib.
    let link_name = if lib_dir.join("libwebrtc.lib").is_file() {
        "libwebrtc"
    } else {
        "webrtc"
    };
    println!("cargo:rustc-link-lib=static:+whole-archive={link_name}");
    link_system_deps(&lib_dir);

    // A real lib is present: enable link-dependent tests/examples.
    println!("cargo:rustc-cfg=have_libwebrtc");
}

/// Compile `glue/*.cpp` against the WebRTC public headers (+ vendored abseil).
fn compile_glue(include_dir: &Path, is_debug_prebuilt: bool) {
    println!("cargo:rerun-if-changed=glue/reactor_webrtc.cpp");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file("glue/reactor_webrtc.cpp")
        .include(include_dir)
        .include(include_dir.join("third_party/abseil-cpp"))
        .include(include_dir.join("third_party/libyuv/include"))
        // WebRTC (this milestone) requires C++20 — its public headers use
        // std::span etc.
        .std("c++20")
        .warnings(false);

    // Real H.264 via a dynamically loaded OpenH264 (see src/openh264.rs for
    // why it's dlopen'd rather than compiled in). Opt-in: only compiled when
    // the `openh264` Cargo feature is enabled, so consumers who don't want it
    // don't pay for the extra translation unit. REACTOR_WEBRTC_OPENH264 gates
    // the corresponding FFI entry point + #include in reactor_webrtc.cpp.
    if env::var_os("CARGO_FEATURE_OPENH264").is_some() {
        println!("cargo:rerun-if-changed=glue/openh264/openh264_codec.cc");
        println!("cargo:rerun-if-changed=glue/openh264/openh264_codec.h");
        build
            .file("glue/openh264/openh264_codec.cc")
            .define("REACTOR_WEBRTC_OPENH264", None);
    }

    // WebRTC headers branch on these platform macros (mirrors gn's defines).
    match env::var("CARGO_CFG_TARGET_OS").unwrap_or_default().as_str() {
        "macos" => {
            build
                .define("WEBRTC_POSIX", None)
                .define("WEBRTC_MAC", None);
            compile_apple_hw_glue(include_dir, is_debug_prebuilt, &[]);
        }
        "ios" => {
            build
                .define("WEBRTC_POSIX", None)
                .define("WEBRTC_MAC", None)
                .define("WEBRTC_IOS", None);
            compile_apple_hw_glue(include_dir, is_debug_prebuilt, &[("WEBRTC_IOS", None)]);
        }
        "android" => {
            build
                .define("WEBRTC_POSIX", None)
                .define("WEBRTC_LINUX", None)
                .define("WEBRTC_ANDROID", None);
        }
        "linux" => {
            build
                .define("WEBRTC_POSIX", None)
                .define("WEBRTC_LINUX", None);
        }
        "windows" => {
            build.define("WEBRTC_WIN", None);
            // Windows SDK headers define min/max as macros, which conflicts with
            // std::numeric_limits<T>::max() in WebRTC's video_timing.h.
            build.define("NOMINMAX", None);
            // libwebrtc.lib is compiled with /MT (static CRT). The cc crate
            // defaults to /MD; mismatch causes LNK2038. Match /MT here.
            build.static_crt(true);
        }
        _ => {}
    }

    // NDEBUG controls RTC_DCHECK_IS_ON, which gates out-of-line SequenceChecker
    // methods and extra struct members. The glue must match the prebuilt:
    //   • release (NDEBUG set)   → inline stubs, small structs
    //   • debug  (NDEBUG absent) → out-of-line methods, larger structs
    // A mismatch on any platform causes either LNK2019 (missing symbol) or a
    // heap corruption (wrong struct size). Apply unconditionally, all targets.
    if !is_debug_prebuilt {
        build.define("NDEBUG", None);
    }

    // linux/android link WebRTC's *bundled* libc++, whose ABI namespace is __Cr
    // (not the platform libc++'s __1). Compile the glue against those exact
    // headers (shipped by package.sh) so std:: types match the archive; without
    // this the glue's std::__1::* symbols won't resolve against libwebrtc.a's
    // std::__Cr::*. mac/ios use the platform libc++ (clang default) and windows
    // uses the MSVC STL — both already match their glue toolchain.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "linux" | "android") {
        let libcxx = include_dir.join("third_party/libc++/src/include");
        let cfg_site = include_dir.join("buildtools/third_party/libc++");
        if libcxx.is_dir() {
            // libc++ requires clang (the default linux cc is gcc). Respect an
            // explicit CXX (e.g. the NDK clang for android).
            if target_os == "linux" && env::var_os("CXX").is_none() {
                build.compiler("clang++");
            }
            build.flag("-nostdinc++");
            // Don't let cc auto-link the system C++ stdlib (stdc++); we link the
            // bundled libc++.a/libc++abi.a ourselves in link_system_deps.
            build.cpp_link_stdlib(None);
            // WebRTC sets the hardening mode via -D (its __config_site leaves
            // _LIBCPP_HARDENING_MODE_DEFAULT unset); match it or <__config> errors.
            build.define("_LIBCPP_HARDENING_MODE", "_LIBCPP_HARDENING_MODE_NONE");
            build.flag("-isystem").flag(cfg_site.to_str().unwrap());
            build.flag("-isystem").flag(libcxx.to_str().unwrap());
            let libcxxabi = include_dir.join("third_party/libc++abi/src/include");
            if libcxxabi.is_dir() {
                build.flag("-isystem").flag(libcxxabi.to_str().unwrap());
            }
        } else {
            println!(
                "cargo:warning=reactor-webrtc-sys: bundled libc++ headers absent under {} — \
                 glue will use the platform stdlib and likely fail to link against the \
                 __Cr-namespaced lib. Rebuild the prebuilt with the updated package.sh.",
                include_dir.display()
            );
        }
    }

    build.compile("reactor_webrtc_glue");
}

/// Compile `glue/apple_hw/apple_hw_codec.mm` (real VideoToolbox H.264 via the
/// RTC_OBJC_TYPE bridging classes) as its own static lib, separate from
/// `compile_glue`'s `cc::Build`: Objective-C++ needs `-fobjc-arc` (WebRTC's
/// own gn build enables ARC for all its objc/objc++ sources) so the alloc'd
/// RTCVideoEncoderFactoryH264/RTCVideoDecoderFactoryH264 instances are kept
/// alive for as long as the native factory wrapping them holds a reference —
/// applying that flag to reactor_webrtc.cpp's plain-C++ translation unit as
/// well would be needless (and `cc` sets flags per-Build, not per-file).
fn compile_apple_hw_glue(
    include_dir: &Path,
    is_debug_prebuilt: bool,
    extra_defines: &[(&str, Option<&str>)],
) {
    println!("cargo:rerun-if-changed=glue/apple_hw/apple_hw_codec.mm");
    println!("cargo:rerun-if-changed=glue/apple_hw/apple_hw_codec.h");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file("glue/apple_hw/apple_hw_codec.mm")
        .include(include_dir)
        .include(include_dir.join("third_party/abseil-cpp"))
        .include(include_dir.join("third_party/libyuv/include"))
        // The objc SDK headers use two different quoted-include conventions
        // for the same files under sdk/objc/base/ — some as a bare filename
        // ("RTCVideoDecoderFactory.h", e.g. RTCVideoDecoderFactoryH264.h) and
        // others as "base/<file>.h" (e.g. sdk/objc/native/api/video_decoder_
        // factory.h) — matching how WebRTC's own gn build adds both
        // sdk/objc/base and sdk/objc to the header search path.
        .include(include_dir.join("sdk/objc/base"))
        .include(include_dir.join("sdk/objc"))
        .std("c++20")
        .flag("-fobjc-arc")
        .define("WEBRTC_POSIX", None)
        .define("WEBRTC_MAC", None)
        .warnings(false);
    for (name, value) in extra_defines {
        build.define(name, *value);
    }
    if !is_debug_prebuilt {
        build.define("NDEBUG", None);
    }

    build.compile("reactor_apple_hw_glue");
}

/// Per-target system libraries/frameworks libwebrtc needs at final link.
fn link_system_deps(lib_dir: &Path) {
    match env::var("CARGO_CFG_TARGET_OS").unwrap_or_default().as_str() {
        "macos" => {
            println!("cargo:rustc-link-lib=c++");
            for fw in [
                "Foundation",
                "CoreFoundation",
                "CoreAudio",
                "AudioToolbox",
                "CoreMedia",
                "CoreVideo",
                "CoreGraphics",
                "CoreImage",
                "VideoToolbox",
                "AVFoundation",
                "AppKit",
                "IOSurface",
                "IOKit",
                "Metal",
                "QuartzCore",
                "OpenGL",
                "ScreenCaptureKit",
                "Security",
                "SystemConfiguration",
            ] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
        }
        "ios" => {
            // NOTE: `IPHONEOS_DEPLOYMENT_TARGET` is deliberately left unset here.
            // Unset, rustc stamps the output `LC_VERSION_MIN_IPHONEOS 10.0` while
            // every member of the prebuilt `libwebrtc.a` is built for 14.0, and
            // the linker warns once per member:
            //   `ld: warning: object file (...) was built for newer 'iOS' version
            //    (14.0) than being linked (10.0)`
            // ~2.9k lines, surfaced whenever a link fails. Only a warning: the
            // link succeeds and the binary is correct. Setting a floor from this
            // build script would raise the minimum iOS version of every consumer
            // without them asking, so it stays their call - export
            // `IPHONEOS_DEPLOYMENT_TARGET=14.0` (or higher) to match the prebuilt
            // and silence it.
            println!("cargo:rustc-link-lib=c++");
            for fw in [
                "Foundation",
                "CoreFoundation",
                "CoreAudio",
                "AudioToolbox",
                "CoreMedia",
                "CoreVideo",
                "CoreGraphics",
                "VideoToolbox",
                "AVFoundation",
                "Metal",
                // `RTCNetworkMonitor` (the ObjC SDK's monitor, compiled in for
                // iOS only) calls `nw_path_monitor_*` / `nw_interface_*`. Without
                // this, every iOS link fails with nine undefined `_nw_*` symbols.
                // macOS does not need it: the mac prebuilt carries no
                // `RTCNetworkMonitor.o` member and no undefined `_nw_*` symbols
                // - it watches the network through `SystemConfiguration`.
                "Network",
                "UIKit",
            ] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
        }
        "android" => {
            // WebRTC for Android links the bundled libc++ (ABI namespace __Cr)
            // and the same NDK system libraries as a normal WebRTC Android build.
            // +whole-archive forces GNU ld to load all archive members (the
            // bundled libc++.a has internal circular deps that a single-pass
            // scan misses); --gc-sections trims the unused code afterward.
            println!("cargo:rustc-link-lib=static:+whole-archive=c++");
            if lib_dir.join("libc++abi.a").is_file() {
                println!("cargo:rustc-link-lib=static:+whole-archive=c++abi");
            }
            if lib_dir.join("libunwind.a").is_file() {
                println!("cargo:rustc-link-lib=static=unwind");
            }
            for l in ["EGL", "GLESv2", "OpenSLES", "log"] {
                println!("cargo:rustc-link-lib=dylib={l}");
            }
            // JNI_OnLoad is defined inside libwebrtc.a (sdk/android/src/jni/
            // jni_onload.cc). With --gc-sections / whole-archive the symbol may
            // be stripped if nothing references it by name. Keep it alive so the
            // Android runtime finds it when dlopen-ing libreactor_ffi.so.
            println!("cargo:rustc-link-arg=-Wl,--undefined=JNI_OnLoad");
            // Expose the JAR path so downstream tooling (build-android-libs.sh)
            // can locate it without a fragile find(1) over the whole target tree.
            let jar = lib_dir.join("libwebrtc.jar");
            if jar.is_file() {
                println!("cargo:rustc-env=REACTOR_WEBRTC_JAR={}", jar.display());
            }
        }
        "linux" => {
            // libwebrtc.a only *references* the bundled libc++ (ABI namespace
            // __Cr); its definitions live in separate libc++.a/libc++abi.a
            // (shipped by package.sh). Link them after webrtc so its symbols
            // resolve, and do NOT link the system stdc++. Fall back to the system
            // stdc++ only for an older/bare layout without the bundled archives.
            //
            // +whole-archive forces GNU ld to load all archive members. The
            // bundled libc++.a is a fat archive whose member ORDER differs from
            // the x64 side-effect archive: circular refs (e.g. ostream.o →
            // ios.o → ostream.o) break GNU ld's single-pass scan, leaving
            // std::__Cr::* symbols undefined. +whole-archive bypasses that
            // ordering issue; --gc-sections (already in the rustc link flags)
            // then strips unused code so binary size stays reasonable.
            let has_cxx = lib_dir.join("libc++.a").is_file();
            println!(
                "cargo:warning=reactor-webrtc-sys: libc++.a at {} → {}",
                lib_dir.join("libc++.a").display(),
                if has_cxx { "found" } else { "absent" }
            );
            if has_cxx {
                println!("cargo:rustc-link-lib=static:+whole-archive=c++");
                if lib_dir.join("libc++abi.a").is_file() {
                    println!("cargo:rustc-link-lib=static:+whole-archive=c++abi");
                }
                if lib_dir.join("libunwind.a").is_file() {
                    println!("cargo:rustc-link-lib=static=unwind");
                }
            } else {
                println!(
                    "cargo:warning=reactor-webrtc-sys: bundled libc++.a absent — \
                     falling back to system stdc++ (std::__Cr::* symbols will be unresolved)"
                );
                println!("cargo:rustc-link-lib=dylib=stdc++");
            }
            // Desktop capture (and its libX11 dep) is disabled in the build
            // (rtc_use_x11=false), so only the base system libs are needed.
            for l in ["dl", "pthread", "m"] {
                println!("cargo:rustc-link-lib=dylib={l}");
            }
        }
        "windows" => {
            for l in [
                // Core/networking
                "winmm",
                "secur32",
                "ole32",
                "ws2_32",
                "iphlpapi",
                // Screen capture (WGC path: DXGI + D3D11 + DWM + GDI + DPI)
                "dxgi",
                "d3d11",
                "dwmapi",
                "gdi32",
                "shcore",
                // Audio DSP (DirectShow + Windows Media DSP GUIDs)
                "dmoguids",
                "msdmo",
                "strmiids",
                "wmcodecdspuuid",
            ] {
                println!("cargo:rustc-link-lib=dylib={l}");
            }
        }
        _ => {}
    }
}

/// Resolve the release tag from `WEBRTC_VERSION` at the workspace root, or
/// fall back to `PREBUILT_TAG_FALLBACK` for builds from a published crate
/// (where the workspace root is absent).
fn prebuilt_tag() -> String {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let path = Path::new(&manifest_dir).join("../../WEBRTC_VERSION");
    if let Ok(src) = std::fs::read_to_string(&path) {
        println!("cargo:rerun-if-changed={}", path.display());
        return parse_webrtc_tag(&src);
    }
    PREBUILT_TAG_FALLBACK.to_string()
}

/// Parse `WEBRTC_VERSION` shell-variable format into a release tag string.
/// Tag format: `webrtc-<milestone>-<commit8>-p<patch>` (mirrors publish.sh).
fn parse_webrtc_tag(src: &str) -> String {
    let mut branch = "";
    let mut commit = "";
    let mut patch = "0";
    for line in src.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("WEBRTC_BRANCH=") {
            branch = v;
        } else if let Some(v) = line.strip_prefix("WEBRTC_COMMIT=") {
            commit = v;
        } else if let Some(v) = line.strip_prefix("REACTOR_PATCH_LEVEL=") {
            patch = v;
        }
    }
    let milestone = branch.strip_prefix("branch-heads/").unwrap_or(branch);
    let short = &commit[..commit.len().min(8)];
    format!("webrtc-{milestone}-{short}-p{patch}")
}

/// Map the current Cargo target triple to its prebuilt platform token, or
/// `None` if no prebuilt is available for this target.
///
/// Token format matches `package.sh`'s `$OS-$ARCH[-$VARIANT]` naming.
fn prebuilt_platform() -> Option<&'static str> {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let abi = env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default();

    match os.as_str() {
        "macos" => match arch.as_str() {
            "aarch64" => Some("mac-arm64"),
            "x86_64" => Some("mac-x64"),
            _ => None,
        },
        "ios" => {
            // aarch64-apple-ios           → device (abi = "")
            // aarch64-apple-ios-sim       → simulator (abi = "sim")
            // x86_64-apple-ios            → simulator (x64 is always sim)
            let is_sim = abi == "sim" || arch == "x86_64";
            Some(if is_sim {
                "ios-arm64-simulator"
            } else {
                "ios-arm64-device"
            })
        }
        "linux" => match arch.as_str() {
            "x86_64" => Some("linux-x64"),
            "aarch64" => Some("linux-arm64"),
            _ => None,
        },
        "android" => match arch.as_str() {
            "aarch64" => Some("android-arm64"),
            _ => None,
        },
        "windows" => match arch.as_str() {
            "x86_64" => Some("win-x64"),
            _ => None,
        },
        _ => None,
    }
}

/// Download and parse the `.sha256` sidecar file for a prebuilt asset.
/// Returns the hex digest on success, or `None` if the download fails.
fn fetch_sha256(sha_url: &str) -> Option<String> {
    let tmp = PathBuf::from(env::var("OUT_DIR").unwrap()).join("prebuilt.sha256");
    let ok = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&tmp)
        .arg(sha_url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    let content = std::fs::read_to_string(&tmp).ok()?;
    Some(content.split_whitespace().next()?.to_string())
}

/// Download our prebuilt archive (`.tar.zst`, the layout `package.sh` /
/// `publish.sh` produce: `lib/` + `include/`), verify its sha256, and extract
/// it into `OUT_DIR`. Returns the extracted root (packaged layout) for `link`.
///
/// Shells out to `curl` + `tar`/`zstd` (no extra Rust build-deps, so dev-mode
/// `cargo check` stays fast). Release assets are public, so the download needs
/// no credentials.
fn download_prebuilt(url: &str, sha256: Option<&str>) -> PathBuf {
    let out_root = PathBuf::from(env::var("OUT_DIR").unwrap());
    let out = out_root.join("libwebrtc");
    let archive = out_root.join("prebuilt.tar.zst");

    // Use the cached extraction only when the layout is present AND its SHA
    // matches a sentinel written after the last successful download.  Without
    // this check a restored Rust build cache (Swatinem/rust-cache restores the
    // whole OUT_DIR) containing an older prebuilt (e.g. p3 without libc++.a)
    // would be silently reused even though the caller supplied a different SHA
    // (the p4 prebuilt).  The sentinel is written below after a verified
    // extraction and is a no-op when no SHA is provided (Mode 1 / dev builds).
    let lib_present =
        out.join("lib/libwebrtc.a").is_file() || out.join("lib/libwebrtc.lib").is_file();
    if lib_present {
        let cache_valid = match sha256 {
            None => true, // no checksum → trust whatever is there
            Some(expected) => {
                let sentinel = out.join(".sha256");
                std::fs::read_to_string(&sentinel)
                    .map(|s| s.trim().to_lowercase() == expected.trim().to_lowercase())
                    .unwrap_or(false)
            }
        };
        if cache_valid {
            return out;
        }
        // SHA mismatch or missing sentinel → stale cache; re-download below.
    }

    // ── download ──────────────────────────────────────────────────────────
    let mut curl = std::process::Command::new("curl");
    curl.args(["-fSL", "--retry", "3", "--retry-delay", "2", "-o"])
        .arg(&archive)
        .arg(url);
    run(&mut curl, "download prebuilt (curl)");

    // ── verify sha256 ─────────────────────────────────────────────────────
    if let Some(expected) = sha256 {
        let got = sha256_file(&archive);
        let expected = expected.trim().to_lowercase();
        if got != expected {
            panic!("reactor-webrtc-sys: prebuilt sha256 mismatch\n  expected {expected}\n  got      {got}");
        }
    } else {
        println!("cargo:warning=reactor-webrtc-sys: REACTOR_WEBRTC_PREBUILT_SHA256 not set — skipping integrity check");
    }

    // ── extract (lib/ + include/) ─────────────────────────────────────────
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("create extract dir");
    // Modern tar (bsdtar / GNU ≥1.31) auto-detects zstd; fall back to a
    // `zstd | tar` pipe otherwise.
    let direct = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&out)
        .status();
    let ok = matches!(direct, Ok(s) if s.success());
    if !ok {
        let pipe = format!(
            "zstd -dc {} | tar -x -C {}",
            shell_quote(&archive),
            shell_quote(&out)
        );
        run(
            std::process::Command::new("sh").arg("-c").arg(&pipe),
            "extract prebuilt (zstd | tar)",
        );
    }

    if !out.join("lib/libwebrtc.a").is_file() && !out.join("lib/libwebrtc.lib").is_file() {
        panic!(
            "reactor-webrtc-sys: extracted prebuilt has no lib/libwebrtc.a or \
             lib/libwebrtc.lib (bad archive layout?)"
        );
    }
    verify_prebuilt_arch(&out.join("lib"));

    // Write the SHA sentinel so subsequent runs with the same checksum can
    // skip the download.  Silently ignore write errors (non-fatal).
    if let Some(sha) = sha256 {
        let _ = std::fs::write(out.join(".sha256"), sha.trim());
    }
    out
}

/// Verify that `libwebrtc.a` was built for the current target architecture by
/// inspecting the ELF `e_machine` field of the first object in the archive.
/// Panics with a clear message when the prebuilt was built for the wrong arch
/// (e.g. an x86_64 archive delivered as the linux-arm64 prebuilt), avoiding
/// the cryptic "unknown architecture of input file" error from the linker.
/// Only active on Linux; macOS/Windows use Mach-O/PE where the linker already
/// rejects mismatches at load time with a descriptive error.
fn verify_prebuilt_arch(lib_dir: &Path) {
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "linux" {
        return;
    }
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let expected_machine: u16 = match target_arch.as_str() {
        "x86_64" => 0x3E,  // EM_X86_64
        "aarch64" => 0xB7, // EM_AARCH64
        _ => return,
    };

    let lib = lib_dir.join("libwebrtc.a");
    let data = match std::fs::read(&lib) {
        Ok(d) => d,
        Err(_) => return,
    };

    // ar archive: 8-byte magic "!<arch>\n", then 60-byte member headers + content.
    const AR_MAGIC: &[u8] = b"!<arch>\n";
    if !data.starts_with(AR_MAGIC) {
        return;
    }

    let mut pos = AR_MAGIC.len();
    while pos + 60 <= data.len() {
        let member_size: usize = std::str::from_utf8(&data[pos + 48..pos + 58])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let content_start = pos + 60;
        let content_end = data.len().min(content_start + member_size);
        let content = &data[content_start..content_end];

        // ELF: 4-byte magic + 12 bytes ident fields + 2-byte type + 2-byte machine.
        if content.len() >= 20 && content.starts_with(b"\x7fELF") {
            let machine = u16::from_le_bytes([content[18], content[19]]);
            if machine != expected_machine {
                let got = match machine {
                    0x3E => "x86_64",
                    0xB7 => "aarch64",
                    0x28 => "arm",
                    _ => "unknown",
                };
                panic!(
                    "reactor-webrtc-sys: prebuilt architecture mismatch!\n  \
                     Target:   {} (e_machine={:#06x})\n  \
                     Prebuilt: {} (e_machine={:#06x})\n\n  \
                     The archive at\n    {}\n  \
                     was built for a different architecture than the current target.\n  \
                     Rebuild the prebuilt for {} or update REACTOR_WEBRTC_PREBUILT_URL.",
                    target_arch,
                    expected_machine,
                    got,
                    machine,
                    lib.display(),
                    target_arch,
                )
            }
            return; // arch matches
        }

        pos = content_start + member_size;
        if !member_size.is_multiple_of(2) {
            pos += 1;
        }
    }
}

/// Run a command, panicking with context on failure.
fn run(cmd: &mut std::process::Command, what: &str) {
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("reactor-webrtc-sys: {what} failed ({s})"),
        Err(e) => panic!("reactor-webrtc-sys: {what} could not start: {e}"),
    }
}

/// Extract a lowercase 64-char hex digest from a hashing tool's stdout.
///
/// GNU coreutils switches to an escaped output form when the file name contains
/// a backslash or newline: the line is prefixed with `\` and the backslashes in
/// the name are doubled. Every Windows path trips this (`D:\a\…`), so
/// `sha256sum` there reports `\<digest> *<name>` — hence the `\` strip. Returns
/// `None` if the output is not a well-formed digest, so callers fall through to
/// the next tool instead of comparing against garbage.
fn parse_digest(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let hex = text
        .split_whitespace()
        .next()?
        .trim_start_matches('\\')
        .to_lowercase();
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(hex)
}

/// Compute a file's sha256 via `sha256sum` (Linux, and Windows — it ships in
/// Git-for-Windows' `usr/bin`, which is on the PATH of GitHub's `windows-latest`
/// runners, where our Windows wheels are built) or `shasum -a 256` (macOS).
fn sha256_file(path: &Path) -> String {
    for (bin, args) in [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])] {
        if let Ok(out) = std::process::Command::new(bin)
            .args(args)
            .arg(path)
            .output()
        {
            if out.status.success() {
                if let Some(hex) = parse_digest(&out.stdout) {
                    return hex;
                }
            }
        }
    }
    panic!("reactor-webrtc-sys: need `sha256sum` or `shasum` to verify the prebuilt");
}

/// Minimal single-quote shell escaping for a path passed to `sh -c`.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
