//! Build script for `reactor-webrtc-sys`.
//!
//! Resolves the native `libwebrtc` (our owned build — see ../../webrtc-build)
//! in one of three modes, in priority order:
//!
//!   1. `REACTOR_WEBRTC_LIB_DIR=/path`  — link a locally-built/extracted lib
//!      directory (used by contributors building WebRTC from source). The dir
//!      is either a packaged layout (`<dir>/lib/libwebrtc.a` + `<dir>/include`)
//!      or a bare dir containing `libwebrtc.a` (headers then come from
//!      `REACTOR_WEBRTC_INCLUDE_DIR`).
//!   2. `REACTOR_WEBRTC_PREBUILT_URL=...` (+ `REACTOR_WEBRTC_PREBUILT_SHA256`)
//!      — download our prebuilt archive for this target, verify the checksum,
//!      extract into `OUT_DIR`, and link it. This is the default production path.
//!   3. Nothing configured — **API/dev mode**: emit no link directives so
//!      `cargo check` and rlib builds of the API succeed without a native lib.
//!      Any final binary/test that actually calls into WebRTC must set one of
//!      the env vars above, or linking will fail.
//!
//! When a native lib is resolved we also compile the C++ glue in `glue/` (the
//! FFI implementation) and emit `cfg(have_libwebrtc)` so link-dependent tests
//! compile only when there is something to link against.
//!
//! The prebuilt archives themselves are produced and published by
//! `../../webrtc-build` (depot_tools + gn/ninja, pinned to ./WEBRTC_VERSION).

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_LIB_DIR");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_PREBUILT_URL");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_PREBUILT_SHA256");
    // Gate link-dependent tests/examples on an actual native lib being present.
    println!("cargo:rustc-check-cfg=cfg(have_libwebrtc)");

    if let Ok(dir) = env::var("REACTOR_WEBRTC_LIB_DIR") {
        link(Path::new(&dir));
        return;
    }

    if let Ok(url) = env::var("REACTOR_WEBRTC_PREBUILT_URL") {
        let sha256 = env::var("REACTOR_WEBRTC_PREBUILT_SHA256").ok();
        let dir = download_prebuilt(&url, sha256.as_deref());
        link(&dir);
        return;
    }

    println!(
        "cargo:warning=reactor-webrtc-sys: no native libwebrtc configured \
         (set REACTOR_WEBRTC_PREBUILT_URL[+_SHA256] or REACTOR_WEBRTC_LIB_DIR). \
         Building API/check only — final linking will fail until a prebuilt is provided."
    );
}

/// Resolve `(lib_dir, include_dir)` from the configured root, link the static
/// lib + its system dependencies, and compile the C++ glue against the headers.
fn link(root: &Path) {
    // Packaged layout (webrtc-build/package.sh): <root>/lib + <root>/include.
    // Bare layout: <root> holds libwebrtc.a directly.
    let (lib_dir, include_dir) = if root.join("lib/libwebrtc.a").is_file() {
        (root.join("lib"), root.join("include"))
    } else {
        let inc = env::var("REACTOR_WEBRTC_INCLUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.join("include"));
        (root.to_path_buf(), inc)
    };

    compile_glue(&include_dir);

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=webrtc");
    link_system_deps();

    // A real lib is present: enable link-dependent tests/examples.
    println!("cargo:rustc-cfg=have_libwebrtc");
}

/// Compile `glue/*.cpp` against the WebRTC public headers (+ vendored abseil).
fn compile_glue(include_dir: &Path) {
    println!("cargo:rerun-if-changed=glue/reactor_webrtc.cpp");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file("glue/reactor_webrtc.cpp")
        .include(include_dir)
        .include(include_dir.join("third_party/abseil-cpp"))
        // WebRTC (this milestone) requires C++20 — its public headers use
        // std::span etc.
        .std("c++20")
        .warnings(false);

    // WebRTC headers branch on these platform macros (mirrors gn's defines).
    match env::var("CARGO_CFG_TARGET_OS").unwrap_or_default().as_str() {
        "macos" => {
            build
                .define("WEBRTC_POSIX", None)
                .define("WEBRTC_MAC", None);
        }
        "ios" => {
            build
                .define("WEBRTC_POSIX", None)
                .define("WEBRTC_MAC", None)
                .define("WEBRTC_IOS", None);
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
        }
        _ => {}
    }
    build.compile("reactor_webrtc_glue");
}

/// Per-target system libraries/frameworks libwebrtc needs at final link.
fn link_system_deps() {
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
                "VideoToolbox",
                "AVFoundation",
                "AppKit",
                "IOSurface",
                "Metal",
                "OpenGL",
                "Security",
                "SystemConfiguration",
            ] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
        }
        "ios" => {
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
            ] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
        }
        "android" => {
            // -lc++_static -lc++abi -lEGL -lGLESv2 -lOpenSLES + JNI companion.
        }
        "linux" => {
            println!("cargo:rustc-link-lib=dylib=stdc++");
            for l in ["dl", "pthread", "m"] {
                println!("cargo:rustc-link-lib=dylib={l}");
            }
        }
        "windows" => {
            for l in ["winmm", "secur32", "ole32", "ws2_32", "dmoguids", "msdmo"] {
                println!("cargo:rustc-link-lib=dylib={l}");
            }
        }
        _ => {}
    }
}

/// Download + verify our prebuilt archive and extract it into `OUT_DIR`.
fn download_prebuilt(url: &str, sha256: Option<&str>) -> PathBuf {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("libwebrtc");
    // TODO(M1): fetch `url`, verify `sha256`, extract into `out`. Kept network-
    // free in the scaffold; the archive format/layout is produced by
    // ../../webrtc-build/package.sh and indexed in its manifest.
    let _ = (url, sha256, &out);
    panic!(
        "reactor-webrtc-sys: prebuilt download not implemented yet (M1). \
         Use REACTOR_WEBRTC_LIB_DIR to link a locally built libwebrtc, \
         or build prebuilts with ../../webrtc-build."
    );
}
