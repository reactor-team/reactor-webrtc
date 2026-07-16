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

mod builder;
mod config;
mod encoded;
mod media;
mod observer;
mod peer_connection;
pub mod platform;

use std::collections::VecDeque;
use std::ffi::CString;
use std::os::raw::c_int;
use std::sync::{Arc, Mutex};

pub use builder::{EncodedVideoBuilder, MixedVideoTrack};
pub use config::{ContinualGatheringPolicy, IceServer, IceTransportsType, RtcConfiguration};
pub use encoded::{
    CustomVideoEncoder, EncodedFrame, EncodedVideoFrame, EncodedVideoTrack, FrameAction,
    FrameDirection, FrameTransform, RawVideoFrame, VideoCodec,
};
pub use media::{AudioFrame, MediaKind, Track, VideoFrame};
pub use observer::PeerConnectionObserver;
pub use peer_connection::{
    DataChannel, DataChannelState, IceCandidate, IceCandidatePairState, IceCandidatePairStats,
    IceGatheringState, InboundRtpStats, OutboundRtpStats, PeerConnection, PeerConnectionState,
    SdpType, SessionDescription, StatsReport, Transceiver, TransceiverDirection,
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

/// Which audio device module a [`PeerConnectionFactory`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdmMode {
    /// Headless, no audio hardware: the app pushes mic PCM via
    /// [`PeerConnectionFactory::push_audio_frame`] and receives decoded audio
    /// through the track sinks ([`Track::on_audio_frame`]). Right for servers /
    /// app-driven media.
    #[default]
    Synthetic,
    /// The platform audio device (CoreAudio / ALSA / WASAPI): real mic capture +
    /// speaker playout, with the AEC/NS/AGC processing chain enabled. Right for
    /// desktop client apps. (On Android the platform ADM needs the app Context;
    /// fall back to `Synthetic` there.)
    Platform,
}

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
    /// Create a factory with the given [`AdmMode`].
    pub fn with_adm(mode: AdmMode) -> Result<Self> {
        Self::create(matches!(mode, AdmMode::Platform))
    }

    /// Create a factory using the **synthetic** audio device module — no audio
    /// hardware; feed audio with [`PeerConnectionFactory::push_audio_frame`].
    /// (Shorthand for [`with_adm(AdmMode::Synthetic)`](PeerConnectionFactory::with_adm).)
    pub fn new() -> Result<Self> {
        Self::with_adm(AdmMode::Synthetic)
    }

    /// Create a factory using the **platform** audio device module (real
    /// mic/speaker, e.g. CoreAudio on macOS). Shorthand for
    /// [`with_adm(AdmMode::Platform)`](PeerConnectionFactory::with_adm).
    pub fn with_platform_adm() -> Result<Self> {
        Self::with_adm(AdmMode::Platform)
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

    /// Create a factory that replaces the builtin H.264 encoder with `encoder`.
    ///
    /// The encoder callback is invoked synchronously on the WebRTC encoder
    /// thread for every raw I420 frame ready to be sent. Return
    /// `Some(EncodedVideoFrame)` to push H.264 bytes into the RTP packetizer,
    /// or `None` to drop the frame silently.
    ///
    /// Audio encoding is unaffected; the builtin audio codecs (Opus, G.711,
    /// etc.) remain active.
    pub fn with_custom_video_encoder(encoder: crate::CustomVideoEncoder) -> Result<Self> {
        let encode_fn = encoder.encode_fn;
        let userdata = encoder.userdata;
        let free_ud = encoder.free_ud;
        let use_builtin = encoder.use_builtin;

        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_factory_create_with_custom_video_encoder(
                0, // synthetic ADM
                encode_fn,
                userdata,
                free_ud,
                use_builtin,
            )
        };
        if raw.is_null() {
            // Factory did not take ownership — free the state ourselves.
            if let Some(f) = free_ud {
                f(userdata);
            }
            return Err(Error::Webrtc(
                "factory with custom encoder returned null".into(),
            ));
        }
        Ok(Self { raw })
    }

    /// Create a builder for a factory that supports **multiple** pre-encoded
    /// video tracks.
    ///
    /// ```rust,ignore
    /// let mut b = PeerConnectionFactory::encoded_video_builder();
    /// let camera = b.add_track("camera", 1280, 720);
    /// let screen  = b.add_track("screen",  1920, 1080);
    /// let (factory, tracks) = b.build()?;
    ///
    /// // tracks[0] == camera stream, tracks[1] == screen stream
    /// tracks[0].push_encoded_frame(camera_frame);
    /// tracks[1].push_encoded_frame(screen_frame);
    /// ```
    pub fn encoded_video_builder() -> EncodedVideoBuilder {
        EncodedVideoBuilder::new()
    }

    /// Create a factory pre-wired for push-based encoded video.
    ///
    /// Returns both the factory and an [`EncodedVideoTrack`] handle. Call
    /// [`EncodedVideoTrack::push_encoded_frame`] whenever your encoder produces
    /// a frame — no raw pixel pumping required.
    ///
    /// ```rust,ignore
    /// let (factory, video) =
    ///     PeerConnectionFactory::with_encoded_video_track("cam", 1280, 720)?;
    ///
    /// let pc  = factory.create_peer_connection(&config, observer)?;
    /// let tx  = pc.add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)?;
    /// tx.set_track(video.track())?;
    ///
    /// // … later, on your encoder thread:
    /// video.push_encoded_frame(EncodedVideoFrame {
    ///     data: h264_annex_b_bytes,
    ///     is_key_frame: true,
    ///     width: 1280, height: 720, rtp_timestamp: 0,
    /// });
    /// ```
    ///
    /// `width` and `height` set the resolution advertised to libwebrtc's
    /// encoder pipeline. They must match the resolution you intend to encode.
    /// Pass `0` in [`EncodedVideoFrame`] fields to inherit them automatically.
    pub fn with_encoded_video_track(
        track_id: &str,
        width: u32,
        height: u32,
    ) -> Result<(Self, crate::EncodedVideoTrack)> {
        let queue: Arc<Mutex<VecDeque<EncodedVideoFrame>>> = Arc::new(Mutex::new(VecDeque::new()));
        let encoder = crate::CustomVideoEncoder::from_queue(queue.clone());
        let factory = Self::with_custom_video_encoder(encoder)?;
        let track = factory.create_video_track(track_id)?;
        let encoded = crate::EncodedVideoTrack::new(track, queue, width, height);
        Ok((factory, encoded))
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
