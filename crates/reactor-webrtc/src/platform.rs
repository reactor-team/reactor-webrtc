//! Platform bootstrap. On Android, libwebrtc needs the `JavaVM` (and, for the
//! platform ADM, the application `Context`) before a peer connection is built.
//!
//! These mirror the PoC's `initialize_android` / `initialize_android_context`,
//! but against our own WebRTC build and our own Java namespace
//! (`inc.reactor.org.webrtc.*` — set via android_jni_package_prefix in the build).

/// Hand libwebrtc the `JavaVM`. Call once, typically from `JNI_OnLoad`.
///
/// # Safety
/// `vm` must be a valid `JavaVM*` for the process lifetime.
#[cfg(target_os = "android")]
pub unsafe fn android_init(vm: *mut std::ffi::c_void) {
    reactor_webrtc_sys::reactor_webrtc_android_init(vm);
}

/// Provide the application `Context` (enables the platform ADM). Returns
/// whether context init succeeded.
///
/// # Safety
/// `vm` and `context` must be valid JNI references.
#[cfg(target_os = "android")]
pub unsafe fn android_init_context(
    vm: *mut std::ffi::c_void,
    context: *mut std::ffi::c_void,
) -> bool {
    reactor_webrtc_sys::reactor_webrtc_android_init_context(vm, context) != 0
}
