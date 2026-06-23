//! `reactor-webrtc` — a safe, idiomatic Rust API over Reactor's **owned**
//! `libwebrtc` build (see the `reactor-webrtc-sys` crate and `../../webrtc-build`).
//!
//! This is the WebRTC engine shared across the platform:
//!
//! - **`reactor-sdk-core`** — the native client SDK core (C ABI), and
//! - **`reactor-runtime`** — the server (replacing GStreamer).
//!
//! ## Design goal: drop-in for the PoC
//!
//! The public surface intentionally mirrors the shape `reactor-sdk-core`
//! already consumed from LiveKit's `libwebrtc` crate (factory → peer connection
//! → transceivers/tracks → data channels, plus native video/audio sources &
//! sinks and frame I/O). Swapping the dependency should be a path change, not a
//! rewrite. We do **not** depend on any LiveKit crate.
//!
//! ## Status
//!
//! **M1 scaffold.** Types and signatures are laid out; most bodies are
//! `unimplemented!()` pending the native build (`webrtc-build/`) and the
//! generated FFI. `cargo check` works without a native library; building a real
//! binary requires a prebuilt (see `reactor-webrtc-sys`'s build script).

mod config;
mod media;
mod peer_connection;
pub mod platform;

pub use config::{ContinualGatheringPolicy, IceServer, IceTransportsType, RtcConfiguration};
pub use media::{AudioFrame, AudioTrack, MediaKind, VideoFrame, VideoTrack};
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

/// Entry point: creates peer connections and owns the audio device module.
///
/// Mirrors the PoC's `PeerConnectionFactory` (incl. `acquire_platform_adm` /
/// `set_adm_playout_enabled`, and the synthetic/headless ADM behaviour).
pub struct PeerConnectionFactory {
    raw: *mut reactor_webrtc_sys::PeerConnectionFactory,
}

// SAFETY (TODO M1): the C++ factory is internally thread-safe; confirm and
// document the exact guarantees once the native layer is wired.
unsafe impl Send for PeerConnectionFactory {}
unsafe impl Sync for PeerConnectionFactory {}

impl PeerConnectionFactory {
    /// Create the factory (initializes the WebRTC threads + ADM).
    pub fn new() -> Result<Self> {
        let raw = unsafe { reactor_webrtc_sys::reactor_webrtc_factory_create() };
        if raw.is_null() {
            return Err(Error::Webrtc("factory_create returned null".into()));
        }
        Ok(Self { raw })
    }

    /// Create a peer connection with the given configuration.
    pub fn create_peer_connection(&self, _config: &RtcConfiguration) -> Result<PeerConnection> {
        // TODO(M1): serialize config → JSON, call sys create, wrap the handle.
        unimplemented!("M1: PeerConnectionFactory::create_peer_connection")
    }

    /// Acquire the platform Audio Device Module (real mic/speaker). On servers
    /// and headless clients, leave this unacquired to use the synthetic ADM.
    pub fn acquire_platform_adm(&self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_factory_acquire_platform_adm(self.raw) }
    }

    /// Route received audio to the platform speaker (vs. synthetic playout).
    pub fn set_adm_playout_enabled(&self, enabled: bool) {
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_factory_set_adm_playout_enabled(
                self.raw,
                enabled as std::os::raw::c_int,
            )
        }
    }
}

impl Drop for PeerConnectionFactory {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_factory_destroy(self.raw) }
    }
}
