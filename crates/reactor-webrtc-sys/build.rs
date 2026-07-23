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
const PREBUILT_BASE: &str = "https://github.com/reactor-team/reactor-webrtc/releases/download";

// Fallback tag used when WEBRTC_VERSION is not accessible (e.g. builds from a
// published crate on crates.io). Patched automatically by publish.yml before
// cargo publish — never edit this line manually.
const PREBUILT_TAG_FALLBACK: &str = "webrtc-7907-a5ddff60-p2";

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

    // Mode 3: auto-detect the correct prebuilt from WEBRTC_VERSION (or the
    // baked-in fallback tag) and the current Cargo target triple.
    if let Some(platform) = prebuilt_platform() {
        let tag = prebuilt_tag();
        let asset = format!("reactor-webrtc-{platform}-release.tar.zst");
        let url = format!("{PREBUILT_BASE}/{tag}/{asset}");
        let sha_url = format!("{url}.sha256");
        let sha256 = fetch_sha256(&sha_url);
        let dir = download_prebuilt(&url, sha256.as_deref());
        link(&dir);
        return;
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

    let is_debug_prebuilt = std::fs::read_to_string(lib_dir.join("build_profile"))
        .ok()
        .map(|s| s.trim() == "debug")
        .unwrap_or(false); // no marker = old prebuilt without the file; treat as release
    compile_glue(&include_dir, is_debug_prebuilt);

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
            // NDEBUG controls RTC_DCHECK_IS_ON, which gates extra SequenceChecker
            // members on WebRTC base classes. The glue must match the prebuilt:
            //   • release (NDEBUG set)   → small structs, no debug symbols
            //   • debug  (NDEBUG absent) → larger structs, debug symbols present
            // A mismatch makes the glue size objects incorrectly → heap overflow
            // (glibc: "malloc(): invalid size (unsorted)").
            if !is_debug_prebuilt {
                build.define("NDEBUG", None);
            }
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
                "UIKit",
            ] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
        }
        "android" => {
            // WebRTC for Android links the bundled libc++ (ABI namespace __Cr)
            // and the same NDK system libraries as a normal WebRTC Android build.
            println!("cargo:rustc-link-lib=static=c++");
            if lib_dir.join("libc++abi.a").is_file() {
                println!("cargo:rustc-link-lib=static=c++abi");
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
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsSL", "--retry", "3", "-o"])
        .arg(&tmp)
        .arg(sha_url);
    if let Ok(token) = env::var("REACTOR_WEBRTC_PREBUILT_TOKEN") {
        cmd.arg("-H")
            .arg(format!("Authorization: Bearer {token}"))
            .arg("-H")
            .arg("Accept: application/octet-stream");
    }
    let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
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
