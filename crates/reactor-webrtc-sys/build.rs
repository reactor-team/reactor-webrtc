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
    // libwebrtc.a is one monolithic archive (gn complete_static_lib) with
    // back-references between members (e.g. the simulcast adapter references the
    // software-fallback wrapper). A plain `-lwebrtc` leaves those unresolved
    // because the linker won't revisit earlier members; +whole-archive loads
    // every member, and the final `-dead_strip` drops what the FFI doesn't use.
    println!("cargo:rustc-link-lib=static:+whole-archive=webrtc");
    link_system_deps(&lib_dir);

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
        .include(include_dir.join("third_party/libyuv/include"))
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
            // Match the RELEASE lib's DCHECK config: without NDEBUG the glue
            // enables RTC_DCHECK_IS_ON and references debug-only symbols the
            // release archive omits (e.g. SequenceCheckerImpl::ExpectationToString).
            build.define("NDEBUG", None);
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
            // libwebrtc.a only *references* the bundled libc++ (ABI namespace
            // __Cr); its definitions live in separate libc++.a/libc++abi.a
            // (shipped by package.sh). Link them after webrtc so its symbols
            // resolve, and do NOT link the system stdc++. Fall back to the system
            // stdc++ only for an older/bare layout without the bundled archives.
            if lib_dir.join("libc++.a").is_file() {
                println!("cargo:rustc-link-lib=static=c++");
                if lib_dir.join("libc++abi.a").is_file() {
                    println!("cargo:rustc-link-lib=static=c++abi");
                }
                if lib_dir.join("libunwind.a").is_file() {
                    println!("cargo:rustc-link-lib=static=unwind");
                }
            } else {
                println!("cargo:rustc-link-lib=dylib=stdc++");
            }
            // Desktop capture (and its libX11 dep) is disabled in the build
            // (rtc_use_x11=false), so only the base system libs are needed.
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

/// Download our prebuilt archive (`.tar.zst`, the layout `package.sh` /
/// `publish.sh` produce: `lib/` + `include/`), verify its sha256, and extract
/// it into `OUT_DIR`. Returns the extracted root (packaged layout) for `link`.
///
/// Shells out to `curl` + `tar`/`zstd` (no extra Rust build-deps, so dev-mode
/// `cargo check` stays fast). For a private-repo release asset, set
/// `REACTOR_WEBRTC_PREBUILT_TOKEN` (a `repo`-scoped token) — `curl` follows the
/// GitHub redirect and drops the auth header on the cross-host hop.
fn download_prebuilt(url: &str, sha256: Option<&str>) -> PathBuf {
    let out_root = PathBuf::from(env::var("OUT_DIR").unwrap());
    let out = out_root.join("libwebrtc");
    let archive = out_root.join("prebuilt.tar.zst");

    // Cached from a previous build of this OUT_DIR.
    if out.join("lib/libwebrtc.a").is_file() {
        return out;
    }

    // ── download ──────────────────────────────────────────────────────────
    let mut curl = std::process::Command::new("curl");
    curl.args(["-fSL", "--retry", "3", "--retry-delay", "2", "-o"])
        .arg(&archive)
        .arg(url);
    if let Ok(token) = env::var("REACTOR_WEBRTC_PREBUILT_TOKEN") {
        // For a private GitHub release asset, point the URL at the API asset
        // endpoint (…/releases/assets/<id>); `Accept: application/octet-stream`
        // makes it 302 to the signed download (auth dropped on the cross-host
        // hop). Harmless for plain CDN URLs.
        curl.arg("-H")
            .arg(format!("Authorization: Bearer {token}"))
            .arg("-H")
            .arg("Accept: application/octet-stream");
    }
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

    if !out.join("lib/libwebrtc.a").is_file() {
        panic!(
            "reactor-webrtc-sys: extracted prebuilt has no lib/libwebrtc.a (bad archive layout?)"
        );
    }
    out
}

/// Run a command, panicking with context on failure.
fn run(cmd: &mut std::process::Command, what: &str) {
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("reactor-webrtc-sys: {what} failed ({s})"),
        Err(e) => panic!("reactor-webrtc-sys: {what} could not start: {e}"),
    }
}

/// Compute a file's sha256 via `sha256sum` (Linux) or `shasum -a 256` (macOS).
fn sha256_file(path: &Path) -> String {
    for (bin, args) in [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])] {
        if let Ok(out) = std::process::Command::new(bin)
            .args(args)
            .arg(path)
            .output()
        {
            if out.status.success() {
                if let Some(hex) = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                {
                    return hex.to_lowercase();
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
