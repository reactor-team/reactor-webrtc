//! Platform bootstrap. On Android, libwebrtc needs the `JavaVM` (and, for the
//! platform ADM, the application `Context`) before a peer connection is built.
//!
//! These mirror the PoC's `initialize_android` / `initialize_android_context`,
//! but against our own WebRTC build and our own Java namespace
//! (`inc.reactor.org.webrtc.*` — set via android_jni_package_prefix in the build).

#[cfg(target_os = "android")]
static INITIALIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether [`android_init`] or [`android_init_context`] has run on this
/// process yet — i.e. whether the JavaVM has been handed to libwebrtc, so
/// `AttachCurrentThreadIfNeeded()` is safe to call from Rust-initiated
/// factory constructors instead of aborting.
#[cfg(target_os = "android")]
pub(crate) fn is_initialized() -> bool {
    INITIALIZED.load(std::sync::atomic::Ordering::Acquire)
}

/// Hand libwebrtc the `JavaVM`. Call once, typically from `JNI_OnLoad`.
///
/// # Safety
/// `vm` must be a valid `JavaVM*` for the process lifetime.
#[cfg(target_os = "android")]
pub unsafe fn android_init(vm: *mut std::ffi::c_void) {
    reactor_webrtc_sys::reactor_webrtc_android_init(vm);
    INITIALIZED.store(true, std::sync::atomic::Ordering::Release);
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
    let ok = reactor_webrtc_sys::reactor_webrtc_android_init_context(vm, context) != 0;
    if ok {
        INITIALIZED.store(true, std::sync::atomic::Ordering::Release);
    }
    ok
}
