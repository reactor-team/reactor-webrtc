//! `reactor-webrtc` — a safe, idiomatic Rust API over Reactor's **owned**
//! `libwebrtc` build (see the `reactor-webrtc-sys` crate and `../../webrtc-build`).
//!
//! This is the WebRTC engine shared across the platform:
//!
//! - **`reactor-sdk-core`** — the native client SDK core (C ABI), and
//! - **`reactor-runtime`** — the server (replacing GStreamer).
//!
//! ## Shape
//!
//! [`PeerConnectionFactory`] → [`PeerConnection`] (with a closure-based
//! [`PeerConnectionObserver`]) → SDP offer/answer, ICE, [`Track`]s (audio +
//! video send/receive) and [`DataChannel`]s. RAII throughout: dropping a handle
//! releases the native object. We do **not** depend on any LiveKit crate.
//!
//! Building a real binary/test requires a native `libwebrtc` (set
//! `REACTOR_WEBRTC_LIB_DIR` / `REACTOR_WEBRTC_PREBUILT_URL`); `cargo check`
//! works without one.

mod config;
mod media;
mod observer;
mod peer_connection;
pub mod platform;

use std::ffi::CString;
use std::os::raw::c_int;

pub use config::{ContinualGatheringPolicy, IceServer, IceTransportsType, RtcConfiguration};
pub use media::{AudioFrame, MediaKind, Track, VideoFrame};
pub use observer::PeerConnectionObserver;
pub use peer_connection::{
    DataChannel, IceCandidate, PeerConnection, PeerConnectionState, SdpType, SessionDescription,
    TransceiverDirection,
};

/// The ABI version of the linked native build. Used to assert that the safe
/// crate and the prebuilt `libwebrtc` agree.
pub fn native_abi_version() -> u32 {
    // Safe: a pure version getter with no arguments.
    unsafe { reactor_webrtc_sys::reactor_webrtc_abi_version() }
}

/// Errors surfaced by the WebRTC engine.
#[derive(Debug, Clone)]
pub enum Error {
    /// A WebRTC operation failed (SDP, ICE, transport, …).
    Webrtc(String),
    /// The requested track/transceiver/data-channel was not found.
    NotFound(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Webrtc(m) => write!(f, "webrtc error: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Entry point: creates peer connections and tracks, and owns the audio device
/// module (synthetic by default, or the platform device).
pub struct PeerConnectionFactory {
    raw: *mut reactor_webrtc_sys::PeerConnectionFactory,
}

// SAFETY: the native factory is internally thread-safe (it owns the WebRTC
// signaling/worker/network threads).
unsafe impl Send for PeerConnectionFactory {}
unsafe impl Sync for PeerConnectionFactory {}

impl PeerConnectionFactory {
    /// Create a factory using the **synthetic** audio device module — no audio
    /// hardware; feed audio with [`PeerConnectionFactory::push_audio_frame`].
    pub fn new() -> Result<Self> {
        Self::create(false)
    }

    /// Create a factory using the **platform** audio device module (real
    /// mic/speaker, e.g. CoreAudio on macOS).
    pub fn with_platform_adm() -> Result<Self> {
        Self::create(true)
    }

    fn create(use_platform_adm: bool) -> Result<Self> {
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_factory_create_with_adm(use_platform_adm as c_int)
        };
        if raw.is_null() {
            return Err(Error::Webrtc("factory creation returned null".into()));
        }
        Ok(Self { raw })
    }

    /// Create a peer connection with the given configuration and observer.
    pub fn create_peer_connection(
        &self,
        config: &RtcConfiguration,
        observer: PeerConnectionObserver,
    ) -> Result<PeerConnection> {
        let state = observer.into_state();
        let callbacks = state.callbacks();
        let json = CString::new(config.to_json())
            .map_err(|_| Error::Webrtc("config contains a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create(
                self.raw,
                json.as_ptr(),
                &callbacks,
            )
        };
        if raw.is_null() {
            return Err(Error::Webrtc(
                "peer connection creation returned null".into(),
            ));
        }
        Ok(PeerConnection::new(raw, state))
    }

    /// Create a local video track backed by a push-able source
    /// ([`Track::push_video_frame`]).
    pub fn create_video_track(&self, id: &str) -> Result<Track> {
        let cid = CString::new(id).map_err(|_| Error::Webrtc("id contains a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_track_create(self.raw, cid.as_ptr())
        };
        if raw.is_null() {
            return Err(Error::Webrtc("video track creation returned null".into()));
        }
        Ok(Track::from_raw(raw, MediaKind::Video))
    }

    /// Create a local audio track. Its samples come from this factory's ADM —
    /// feed it with [`PeerConnectionFactory::push_audio_frame`].
    pub fn create_audio_track(&self, id: &str) -> Result<Track> {
        let cid = CString::new(id).map_err(|_| Error::Webrtc("id contains a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_audio_track_create(self.raw, cid.as_ptr())
        };
        if raw.is_null() {
            return Err(Error::Webrtc("audio track creation returned null".into()));
        }
        Ok(Track::from_raw(raw, MediaKind::Audio))
    }

    /// Feed interleaved i16 PCM to the (synthetic) ADM, shared by all local
    /// audio tracks. Typically called with ~10ms blocks (e.g. 480 frames @
    /// 48kHz). No-op with the platform ADM.
    pub fn push_audio_frame(&self, pcm: &[i16], sample_rate: u32, channels: u32) {
        let channels = channels.max(1);
        let samples_per_channel = (pcm.len() / channels as usize) as c_int;
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_factory_push_audio_frame(
                self.raw,
                pcm.as_ptr(),
                samples_per_channel,
                sample_rate as c_int,
                channels as c_int,
            );
        }
    }

    /// Enable/disable the synthetic ADM's playout pump (no-op for the platform
    /// ADM). Disable to stay silent in send-only / headless scenarios.
    pub fn set_adm_playout_enabled(&self, enabled: bool) {
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_factory_set_adm_playout_enabled(
                self.raw,
                enabled as c_int,
            )
        }
    }
}

impl Drop for PeerConnectionFactory {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_factory_destroy(self.raw) }
    }
}
