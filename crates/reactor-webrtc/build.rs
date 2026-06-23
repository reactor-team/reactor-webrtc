//! Mirrors `reactor-webrtc-sys`'s link detection so this crate's integration
//! tests/examples can be gated on a native libwebrtc actually being linked.
//! The sys crate does the real linking; here we only set the cfg.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(have_libwebrtc)");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_LIB_DIR");
    println!("cargo:rerun-if-env-changed=REACTOR_WEBRTC_PREBUILT_URL");
    if std::env::var_os("REACTOR_WEBRTC_LIB_DIR").is_some()
        || std::env::var_os("REACTOR_WEBRTC_PREBUILT_URL").is_some()
    {
        println!("cargo:rustc-cfg=have_libwebrtc");
    }
}
