//! Real-link proof: this test only compiles when a native libwebrtc is
//! resolved (build.rs emits `cfg(have_libwebrtc)` then). It calls into the C++
//! glue, which in turn drives real WebRTC codec factories.
//!
//! Run it against a locally built lib:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc-sys -- --nocapture
//! ```
//!
//! Without the env var, `cargo test` builds an empty test binary (the symbols
//! have nothing to link against), so the workspace still checks cleanly.
#![cfg(have_libwebrtc)]

use std::ffi::CStr;
use std::os::raw::c_char;

#[test]
fn links_and_runs_libwebrtc() {
    // SAFETY: both symbols are implemented by the C++ glue compiled in build.rs
    // and resolved against our libwebrtc.a.
    unsafe {
        assert_eq!(
            reactor_webrtc_sys::reactor_webrtc_abi_version(),
            1,
            "ABI version mismatch"
        );

        let mut buf = [0u8; 1024];
        let n = reactor_webrtc_sys::reactor_webrtc_selftest(
            buf.as_mut_ptr() as *mut c_char,
            buf.len() as i32,
        );
        assert!(n > 0, "expected at least one codec from libwebrtc");

        let codecs = CStr::from_ptr(buf.as_ptr() as *const c_char).to_string_lossy();
        println!("libwebrtc linked OK — {n} codecs: {codecs}");
        let lower = codecs.to_lowercase();
        // Opus is always registered by the builtin audio encoder factory.
        assert!(
            lower.contains("opus"),
            "expected Opus among codecs, got: {codecs}"
        );
        // VP8 is always registered by the builtin video encoder factory.
        assert!(
            lower.contains("vp8"),
            "expected VP8 among codecs, got: {codecs}"
        );
    }
}
