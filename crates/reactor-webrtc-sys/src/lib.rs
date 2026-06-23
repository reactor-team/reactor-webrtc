//! `reactor-webrtc-sys` — low-level FFI to Reactor's owned `libwebrtc` build.
//!
//! This crate is the unsafe boundary: opaque handle types and `extern "C"`
//! declarations that the C++ glue (built by `../../webrtc-build`) implements.
//! Application code should use the safe [`reactor-webrtc`] crate instead.
//!
//! The surface below is the **initial M1 scaffold** — a representative subset
//! of the WebRTC objects `reactor-sdk-core` needs (factory, peer connection,
//! data channel, tracks, frame I/O, ADM, platform bootstrap). It will be
//! generated from the C++ glue (via `cxx`/`bindgen`) rather than hand-written
//! as the build pipeline matures; until then it documents the intended C ABI.
//!
//! [`reactor-webrtc`]: https://docs.rs/reactor-webrtc
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int};

// ── Opaque handles (owned by the C++ side) ───────────────────────────────────

#[repr(C)]
pub struct PeerConnectionFactory {
    _private: [u8; 0],
}
#[repr(C)]
pub struct PeerConnection {
    _private: [u8; 0],
}
#[repr(C)]
pub struct DataChannel {
    _private: [u8; 0],
}
#[repr(C)]
pub struct RtpTransceiver {
    _private: [u8; 0],
}
#[repr(C)]
pub struct MediaStreamTrack {
    _private: [u8; 0],
}
#[repr(C)]
pub struct VideoSource {
    _private: [u8; 0],
}
#[repr(C)]
pub struct AudioSource {
    _private: [u8; 0],
}
#[repr(C)]
pub struct AudioDeviceModule {
    _private: [u8; 0],
}

extern "C" {
    /// ABI version of this native build. The safe crate asserts compatibility.
    pub fn reactor_webrtc_abi_version() -> u32;

    // ── Factory ──────────────────────────────────────────────────────────────
    pub fn reactor_webrtc_factory_create() -> *mut PeerConnectionFactory;
    pub fn reactor_webrtc_factory_destroy(factory: *mut PeerConnectionFactory);

    /// Create a peer connection. `config_json` carries ICE servers / policies.
    pub fn reactor_webrtc_peer_connection_create(
        factory: *mut PeerConnectionFactory,
        config_json: *const c_char,
    ) -> *mut PeerConnection;
    pub fn reactor_webrtc_peer_connection_destroy(pc: *mut PeerConnection);

    // ── Audio Device Module (incl. synthetic/headless mode) ──────────────────
    pub fn reactor_webrtc_factory_acquire_platform_adm(factory: *mut PeerConnectionFactory);
    pub fn reactor_webrtc_factory_set_adm_playout_enabled(
        factory: *mut PeerConnectionFactory,
        enabled: c_int,
    );

    // ── Frame injection (sendonly) ───────────────────────────────────────────
    /// Push a BGRA frame into a sendonly video track's source.
    pub fn reactor_webrtc_push_video_frame(
        pc: *mut PeerConnection,
        track: *const c_char,
        data: *const u8,
        width: u32,
        height: u32,
    );
    /// Push interleaved i16 PCM into a sendonly audio track's source.
    pub fn reactor_webrtc_push_audio_frame(
        pc: *mut PeerConnection,
        track: *const c_char,
        data: *const i16,
        samples_per_channel: u32,
        sample_rate: u32,
        channels: u32,
    );

    // ── Platform bootstrap ───────────────────────────────────────────────────
    /// Android: hand the JavaVM to libwebrtc (call from JNI_OnLoad).
    pub fn reactor_webrtc_android_init(vm: *mut std::ffi::c_void);
    /// Android: provide the application Context (for the platform ADM).
    pub fn reactor_webrtc_android_init_context(
        vm: *mut std::ffi::c_void,
        context: *mut std::ffi::c_void,
    ) -> c_int;
}

// TODO(M1): expand to the full surface used by reactor-sdk-core —
//   create_offer / set_local/remote_description / add_ice_candidate,
//   on_connection_state_change / on_ice_candidate / on_track / on_data_channel,
//   add_transceiver (+ direction), data_channel send / on_message,
//   native video/audio sinks (NativeVideoStream/NativeAudioStream equivalents),
//   I420/BGRA conversion helpers — and move to generated bindings (cxx/bindgen).
