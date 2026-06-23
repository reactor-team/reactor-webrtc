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

use std::os::raw::{c_char, c_int, c_void};

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

/// PeerConnectionObserver callbacks, forwarded from the C++ glue. Every field
/// is optional (`None` = a null function pointer on the C side). `userdata` is
/// passed back verbatim to each callback. State arguments are the integer value
/// of the corresponding WebRTC enum.
///
/// Callbacks fire on WebRTC's signaling thread — handlers must be thread-safe
/// and must not block it.
#[repr(C)]
pub struct PeerConnectionCallbacks {
    pub userdata: *mut c_void,
    pub on_signaling_change: Option<extern "C" fn(userdata: *mut c_void, state: c_int)>,
    pub on_connection_change: Option<extern "C" fn(userdata: *mut c_void, state: c_int)>,
    pub on_ice_gathering_change: Option<extern "C" fn(userdata: *mut c_void, state: c_int)>,
    pub on_ice_candidate: Option<
        extern "C" fn(
            userdata: *mut c_void,
            sdp_mid: *const c_char,
            sdp_mline_index: c_int,
            candidate: *const c_char,
        ),
    >,
    pub on_data_channel:
        Option<extern "C" fn(userdata: *mut c_void, data_channel: *mut DataChannel)>,
    pub on_renegotiation_needed: Option<extern "C" fn(userdata: *mut c_void)>,
}

extern "C" {
    /// ABI version of this native build. The safe crate asserts compatibility.
    pub fn reactor_webrtc_abi_version() -> u32;

    /// Link/run self-test: builds the builtin audio+video encoder factories and
    /// writes a comma-separated, NUL-terminated list of supported codec names
    /// into `out` (capped at `cap`); returns the total codec count. A non-zero
    /// return proves real libwebrtc code linked and executed. (M1 scaffold —
    /// will be removed once the full object surface lands.)
    pub fn reactor_webrtc_selftest(out: *mut c_char, cap: c_int) -> c_int;

    // ── Factory ──────────────────────────────────────────────────────────────
    pub fn reactor_webrtc_factory_create() -> *mut PeerConnectionFactory;
    pub fn reactor_webrtc_factory_destroy(factory: *mut PeerConnectionFactory);

    /// Create a peer connection. `config_json` carries ICE servers / policies
    /// (may be null). `callbacks` may be null. Returns null on failure.
    pub fn reactor_webrtc_peer_connection_create(
        factory: *mut PeerConnectionFactory,
        config_json: *const c_char,
        callbacks: *const PeerConnectionCallbacks,
    ) -> *mut PeerConnection;
    pub fn reactor_webrtc_peer_connection_destroy(pc: *mut PeerConnection);

    /// Create an SDP offer. Exactly one callback fires asynchronously on the
    /// signaling thread: `on_success(userdata, type, sdp)` or
    /// `on_error(userdata, message)`. The C strings are valid only for the
    /// duration of the call.
    pub fn reactor_webrtc_peer_connection_create_offer(
        pc: *mut PeerConnection,
        userdata: *mut c_void,
        on_success: extern "C" fn(userdata: *mut c_void, ty: *const c_char, sdp: *const c_char),
        on_error: extern "C" fn(userdata: *mut c_void, message: *const c_char),
    );
    /// Create an SDP answer (the signaling state must hold a remote offer).
    /// Same callback contract as [`reactor_webrtc_peer_connection_create_offer`].
    pub fn reactor_webrtc_peer_connection_create_answer(
        pc: *mut PeerConnection,
        userdata: *mut c_void,
        on_success: extern "C" fn(userdata: *mut c_void, ty: *const c_char, sdp: *const c_char),
        on_error: extern "C" fn(userdata: *mut c_void, message: *const c_char),
    );

    /// Apply `(type, sdp)` as the local description. `on_complete` fires once
    /// (asynchronously) with a null `error` on success, or a message on failure.
    pub fn reactor_webrtc_peer_connection_set_local_description(
        pc: *mut PeerConnection,
        ty: *const c_char,
        sdp: *const c_char,
        userdata: *mut c_void,
        on_complete: extern "C" fn(userdata: *mut c_void, error: *const c_char),
    );
    /// Apply `(type, sdp)` as the remote description. Same contract as
    /// [`reactor_webrtc_peer_connection_set_local_description`].
    pub fn reactor_webrtc_peer_connection_set_remote_description(
        pc: *mut PeerConnection,
        ty: *const c_char,
        sdp: *const c_char,
        userdata: *mut c_void,
        on_complete: extern "C" fn(userdata: *mut c_void, error: *const c_char),
    );
    /// Add a remote ICE candidate (typically from the peer's `on_ice_candidate`).
    pub fn reactor_webrtc_peer_connection_add_ice_candidate(
        pc: *mut PeerConnection,
        sdp_mid: *const c_char,
        sdp_mline_index: c_int,
        candidate: *const c_char,
        userdata: *mut c_void,
        on_complete: extern "C" fn(userdata: *mut c_void, error: *const c_char),
    );

    /// Create an SDP-negotiated data channel. Returns an opaque handle (which
    /// must be freed with [`reactor_webrtc_data_channel_destroy`]) or null.
    pub fn reactor_webrtc_peer_connection_create_data_channel(
        pc: *mut PeerConnection,
        label: *const c_char,
    ) -> *mut DataChannel;
    /// Release a data channel handle.
    pub fn reactor_webrtc_data_channel_destroy(data_channel: *mut DataChannel);

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
//   create_answer / set_local/remote_description / add_ice_candidate,
//   on_track, add_transceiver (+ direction), data_channel send / on_message,
//   native video/audio sinks (NativeVideoStream/NativeAudioStream equivalents),
//   I420/BGRA conversion helpers — and move to generated bindings (cxx/bindgen).
