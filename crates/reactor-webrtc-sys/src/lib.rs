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
pub struct FrameTransformer {
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

/// An encoded media frame handed to a frame-transform callback (Insertable
/// Streams / Encoded Transform). `data` and `mime_type` are valid only for the
/// duration of the callback; copy them to retain. `frame` is an opaque handle
/// for [`reactor_webrtc_encoded_frame_set_data`]. Layout must match the C
/// `ReactorEncodedFrame` in the glue.
#[repr(C)]
pub struct ReactorEncodedFrame {
    /// 0 = send (egress, encoder→packetizer), 1 = receive (ingress).
    pub direction: c_int,
    /// 1 = audio, 0 = video.
    pub is_audio: c_int,
    /// Video only (0 for audio): 1 if this is a key frame.
    pub is_key_frame: c_int,
    pub payload_type: u8,
    pub ssrc: u32,
    pub timestamp: u32,
    pub data: *const u8,
    pub data_len: usize,
    pub mime_type: *const c_char,
    /// Opaque frame handle for [`reactor_webrtc_encoded_frame_set_data`].
    pub frame: *mut c_void,
}

/// Raw I420 video frame delivered to a custom encoder callback. Planes are
/// valid only for the duration of the call — copy if encoding asynchronously.
/// Layout must match `ReactorRawVideoFrame` in the C++ glue.
#[repr(C)]
pub struct ReactorRawVideoFrame {
    pub y: *const u8,
    pub y_stride: c_int,
    pub u: *const u8,
    pub u_stride: c_int,
    pub v: *const u8,
    pub v_stride: c_int,
    pub width: u32,
    pub height: u32,
    pub rtp_timestamp: u32,
    /// 1 if the media engine requests a key frame (IDR), 0 otherwise.
    pub request_key_frame: c_int,
}

/// Filled by the custom encoder callback to deliver an encoded H.264 frame.
/// Set `data` to null (or return non-zero) to drop the frame.
/// Layout must match `ReactorEncodedVideoOutput` in the C++ glue.
#[repr(C)]
pub struct ReactorEncodedVideoOutput {
    pub data: *const u8,
    pub len: usize,
    pub is_key_frame: c_int,
    /// 0 = inherit width/height/rtp_timestamp from the raw frame.
    pub width: u32,
    pub height: u32,
    pub rtp_timestamp: u32,
    /// Called by C++ after the encoded bytes are copied; frees the buffer.
    /// May be null for static/frame-lifetime buffers.
    pub free_data: Option<extern "C" fn(data: *const u8, len: usize)>,
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
    /// A remote track was added. `track` is an owned handle the receiver must
    /// free with [`reactor_webrtc_media_stream_track_destroy`].
    pub on_track: Option<extern "C" fn(userdata: *mut c_void, track: *mut MediaStreamTrack)>,
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
    /// Create a factory with the synthetic (push-able) ADM — no audio hardware.
    pub fn reactor_webrtc_factory_create() -> *mut PeerConnectionFactory;
    /// Create a factory choosing the audio device backend. `use_platform_adm`:
    /// 0 → synthetic ADM (push PCM via [`reactor_webrtc_factory_push_audio_frame`]);
    /// nonzero → the platform default ADM (real mic/speaker, e.g. CoreAudio).
    pub fn reactor_webrtc_factory_create_with_adm(
        use_platform_adm: c_int,
    ) -> *mut PeerConnectionFactory;
    pub fn reactor_webrtc_factory_destroy(factory: *mut PeerConnectionFactory);

    /// Create a factory that routes all video encoding through `on_encode`.
    /// `on_encode` is called synchronously within `VideoEncoder::Encode()` with
    /// the raw I420 frame; fill `*out` and return 0 to inject encoded bytes into
    /// the RTP stack, or return non-zero to drop the frame. `userdata` lifetime
    /// follows the same contract as `reactor_webrtc_frame_transformer_create`.
    pub fn reactor_webrtc_factory_create_with_custom_video_encoder(
        use_platform_adm: c_int,
        on_encode: extern "C" fn(
            userdata: *mut c_void,
            raw: *const ReactorRawVideoFrame,
            out: *mut ReactorEncodedVideoOutput,
        ) -> c_int,
        userdata: *mut c_void,
        free_ud: Option<extern "C" fn(userdata: *mut c_void)>,
    ) -> *mut PeerConnectionFactory;

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
    /// Send bytes over a data channel. Returns 1 on success, 0 on failure.
    pub fn reactor_webrtc_data_channel_send(
        data_channel: *mut DataChannel,
        data: *const u8,
        len: usize,
        binary: c_int,
    ) -> c_int;
    /// Register data-channel callbacks. `on_message(userdata, data, len,
    /// binary)` fires per message (`data` valid only during the call);
    /// `on_open`/`on_close` on state transitions. Replaces any prior observer.
    pub fn reactor_webrtc_data_channel_register_observer(
        data_channel: *mut DataChannel,
        userdata: *mut c_void,
        on_message: extern "C" fn(
            userdata: *mut c_void,
            data: *const u8,
            len: usize,
            binary: c_int,
        ),
        on_open: extern "C" fn(userdata: *mut c_void),
        on_close: extern "C" fn(userdata: *mut c_void),
    );
    /// Release a data channel handle.
    pub fn reactor_webrtc_data_channel_destroy(data_channel: *mut DataChannel);

    // ── Video tracks ─────────────────────────────────────────────────────────
    /// Create a local video track backed by a push-able source. Returns an
    /// owned [`MediaStreamTrack`] handle (free with
    /// [`reactor_webrtc_media_stream_track_destroy`]) or null.
    pub fn reactor_webrtc_video_track_create(
        factory: *mut PeerConnectionFactory,
        id: *const c_char,
    ) -> *mut MediaStreamTrack;
    /// Push a BGRA frame (`width * height * 4` bytes) into a local video track's
    /// source; converted to I420 and timestamped internally.
    pub fn reactor_webrtc_video_track_push_frame(
        track: *mut MediaStreamTrack,
        bgra: *const u8,
        width: c_int,
        height: c_int,
    );
    /// Add a local audio or video track to the peer connection (creates a
    /// sendrecv transceiver). Returns 1 on success, 0 on failure.
    pub fn reactor_webrtc_peer_connection_add_track(
        pc: *mut PeerConnection,
        track: *mut MediaStreamTrack,
    ) -> c_int;
    /// Attach a frame sink to a (received) video track. `on_frame(userdata,
    /// bgra, width, height)` fires per decoded frame (BGRA, `width*height*4`
    /// bytes, valid only during the call) until the track is destroyed.
    pub fn reactor_webrtc_video_track_add_sink(
        track: *mut MediaStreamTrack,
        userdata: *mut c_void,
        on_frame: extern "C" fn(
            userdata: *mut c_void,
            bgra: *const u8,
            width: c_int,
            height: c_int,
        ),
    );
    /// Kind of a track handle: 0 = audio, 1 = video, -1 = unknown.
    pub fn reactor_webrtc_media_stream_track_kind(track: *mut MediaStreamTrack) -> c_int;

    // ── Transceivers ─────────────────────────────────────────────────────────
    /// Add a transceiver of `media_kind` (0=audio, 1=video) with `direction`
    /// (0=sendrecv, 1=sendonly, 2=recvonly, 3=inactive). Returns an owned
    /// [`RtpTransceiver`] handle (free with [`reactor_webrtc_rtp_transceiver_destroy`]).
    pub fn reactor_webrtc_peer_connection_add_transceiver(
        pc: *mut PeerConnection,
        media_kind: c_int,
        direction: c_int,
    ) -> *mut RtpTransceiver;
    /// Number of transceivers on the peer connection (post-negotiation this
    /// includes ones auto-created from the remote description).
    pub fn reactor_webrtc_peer_connection_transceiver_count(pc: *mut PeerConnection) -> c_int;
    /// Owned handle to the transceiver at `index` (free with
    /// [`reactor_webrtc_rtp_transceiver_destroy`]), or null if out of range.
    pub fn reactor_webrtc_peer_connection_get_transceiver(
        pc: *mut PeerConnection,
        index: c_int,
    ) -> *mut RtpTransceiver;
    /// Media kind of a transceiver: 0 = audio, 1 = video, -1 = unknown.
    pub fn reactor_webrtc_rtp_transceiver_media_kind(transceiver: *mut RtpTransceiver) -> c_int;
    /// Write the transceiver's mid into `out` (capped at `cap`); returns the mid
    /// length, or -1 if there is no mid yet (before set_local_description).
    pub fn reactor_webrtc_rtp_transceiver_mid(
        transceiver: *mut RtpTransceiver,
        out: *mut c_char,
        cap: c_int,
    ) -> c_int;
    /// Attach (or clear, with null) a local track on the transceiver's sender.
    /// Returns 1 on success, 0 on failure.
    pub fn reactor_webrtc_rtp_transceiver_set_track(
        transceiver: *mut RtpTransceiver,
        track: *mut MediaStreamTrack,
    ) -> c_int;
    /// Release a transceiver handle.
    pub fn reactor_webrtc_rtp_transceiver_destroy(transceiver: *mut RtpTransceiver);

    // ── Encoded-frame transform (codec bypass / forward) ─────────────────────
    /// Create an encoded-frame transformer. `on_frame(userdata, frame)` fires
    /// per encoded frame; it returns 0 to emit the frame downstream (after any
    /// [`reactor_webrtc_encoded_frame_set_data`]) or non-zero to drop it
    /// (receive: bypasses the decoder; send: nothing is sent). Returns an owned
    /// handle (free with [`reactor_webrtc_frame_transformer_destroy`]) or null.
    pub fn reactor_webrtc_frame_transformer_create(
        on_frame: extern "C" fn(userdata: *mut c_void, frame: *const ReactorEncodedFrame) -> c_int,
        userdata: *mut c_void,
        free_userdata: extern "C" fn(userdata: *mut c_void),
    ) -> *mut FrameTransformer;
    /// Replace the encoded payload of the frame currently in the callback
    /// (copies). `frame` is [`ReactorEncodedFrame::frame`].
    pub fn reactor_webrtc_encoded_frame_set_data(frame: *mut c_void, data: *const u8, len: usize);
    /// Attach the transformer to the transceiver's **sender** (encoder →
    /// packetizer). Returns 1 on success, 0 on failure.
    pub fn reactor_webrtc_rtp_transceiver_set_sender_transform(
        transceiver: *mut RtpTransceiver,
        transformer: *mut FrameTransformer,
    ) -> c_int;
    /// Attach the transformer to the transceiver's **receiver** (depacketizer →
    /// decoder). Returns 1 on success, 0 on failure.
    pub fn reactor_webrtc_rtp_transceiver_set_receiver_transform(
        transceiver: *mut RtpTransceiver,
        transformer: *mut FrameTransformer,
    ) -> c_int;
    /// Release a transformer handle (the sender/receiver keep their own ref).
    pub fn reactor_webrtc_frame_transformer_destroy(transformer: *mut FrameTransformer);
    /// Destroy a track handle (detaches any sink, releases the track + source).
    pub fn reactor_webrtc_media_stream_track_destroy(track: *mut MediaStreamTrack);

    // ── Audio tracks ─────────────────────────────────────────────────────────
    /// Create a local audio track. Its samples come from the factory's ADM —
    /// push PCM with [`reactor_webrtc_factory_push_audio_frame`]. Returns an
    /// owned [`MediaStreamTrack`] handle or null.
    pub fn reactor_webrtc_audio_track_create(
        factory: *mut PeerConnectionFactory,
        id: *const c_char,
    ) -> *mut MediaStreamTrack;
    /// Deliver interleaved i16 PCM to the factory's ADM (shared by all local
    /// audio tracks). `samples_per_channel` is the frame count (e.g. 480 for
    /// 10ms @ 48kHz).
    pub fn reactor_webrtc_factory_push_audio_frame(
        factory: *mut PeerConnectionFactory,
        pcm: *const i16,
        samples_per_channel: c_int,
        sample_rate: c_int,
        channels: c_int,
    );
    /// Attach a sink to a (received) audio track. `on_audio(userdata, pcm,
    /// sample_rate, channels, frames)` fires per 10ms block — `pcm` is
    /// interleaved i16 (`frames*channels` samples, valid only during the call)
    /// — until the track is destroyed.
    pub fn reactor_webrtc_audio_track_add_sink(
        track: *mut MediaStreamTrack,
        userdata: *mut c_void,
        on_audio: extern "C" fn(
            userdata: *mut c_void,
            pcm: *const i16,
            sample_rate: c_int,
            channels: c_int,
            frames: c_int,
        ),
    );

    // ── Audio Device Module ───────────────────────────────────────────────────
    /// Enable/disable the synthetic ADM's playout pump (no-op for the platform
    /// ADM). Disable to stay fully silent in send-only / headless scenarios.
    pub fn reactor_webrtc_factory_set_adm_playout_enabled(
        factory: *mut PeerConnectionFactory,
        enabled: c_int,
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
