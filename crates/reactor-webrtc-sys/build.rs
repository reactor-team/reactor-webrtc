//! Build script for `reactor-webrtc-sys`.
//!
//! Resolves the native `libwebrtc` (our owned build — see ../../webrtc-build)
//! in one of three modes, in priority order:
//!
//!   1. `REACTOR_WEBRTC_LIB_DIR=/path`  — link a locally-built/extracted lib
//!      directory (used by contributors building WebRTC from source).
//!   2. `REACTOR_WEBRTC_PREBUILT_URL=...` (+ `REACTOR_WEBRTC_PREBUILT_SHA256`)
//!      — download our prebuilt archive for this target, verify the checksum,
//!      extract into `OUT_DIR`, and link it. This is the default production path.
//!   3. Nothing configured — **API/dev mode**: emit no link directives so
//!      `cargo check` and rlib builds of the API succeed without a native lib.
//!      Any final binary/test that actually calls into WebRTC must set one of
//!      the env vars above, or linking will fail.
//!
//! The prebuilt archives themselves are produced and published by
//! `../../webrtc-build` (depot_tools + gn/ninja, pinned to ./WEBRTC_VERSION).

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_LIB_DIR");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_PREBUILT_URL");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_PREBUILT_SHA256");

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

/// Emit the linker directives for a directory that contains our `libwebrtc`
/// static library plus any per-target system dependencies.
fn link(lib_dir: &Path) {
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=webrtc");

    // Per-target system libraries/frameworks libwebrtc needs at final link.
    // TODO(M1): fill these in per target as the prebuilts land (mirrors what
    // webrtc-sys linked: c++ runtime + platform A/V/network frameworks).
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" | "ios" => {
            // e.g. -lc++ + Foundation/AVFoundation/CoreMedia/VideoToolbox/...
        }
        "android" => {
            // e.g. -lc++_static -lc++abi -lEGL -lOpenSLES + JNI companion
        }
        "linux" => {
            // e.g. -lstdc++ -ldl -lpthread -lm
        }
        "windows" => {
            // e.g. winmm, secur32, ole32, ...
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
