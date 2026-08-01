//! `reactor-webrtc` — a safe, idiomatic Rust API over an owned build of Google's
//! WebRTC engine (see the `reactor-webrtc-sys` crate and `../../webrtc-build`).
//!
//! ## Shape
//!
//! [`PeerConnectionFactory`] → [`PeerConnection`] (with a closure-based
//! [`PeerConnectionObserver`]) → SDP offer/answer, ICE, [`Track`]s (audio +
//! video send/receive) and [`DataChannel`]s. RAII throughout: dropping a handle
//! releases the native object.
//!
//! Building a real binary or test requires a native `libwebrtc`; set
//! `REACTOR_WEBRTC_LIB_DIR` or `REACTOR_WEBRTC_PREBUILT_URL`. `cargo check`
//! works without one.

mod builder;
mod config;
mod encoded;
mod media;
pub mod metadata;
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
pub use metadata::FrameMetadata;
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
    /// speaker playout. Right for desktop client apps.
    Platform,
}

/// Audio Processing Module configuration passed to [`PeerConnectionFactory`].
///
/// All fields default to `false` — no processing applied, true audio
/// passthrough. This is the right choice for server-side apps (bots,
/// recorders, SFUs) and for any app that does its own DSP upstream.
///
/// For real mic/speaker hardware ([`AdmMode::Platform`]) you typically want
/// the full chain so captured audio is clean before encoding:
///
/// ```no_run
/// use reactor_webrtc::{ApmConfig, AdmMode, PeerConnectionFactory};
///
/// // Real hardware capture: enable the full processing chain.
/// let factory = PeerConnectionFactory::with_adm_apm(
///     AdmMode::Platform,
///     ApmConfig {
///         echo_canceller:    true,
///         noise_suppression: true,
///         agc:               true,
///         high_pass_filter:  true,
///     },
/// )?;
/// # Ok::<_, reactor_webrtc::Error>(())
/// ```
///
/// You can also use the platform ADM with *no* processing — for example when
/// your app feeds audio through its own DSP pipeline, or when capturing from a
/// virtual audio source that is already clean:
///
/// ```no_run
/// use reactor_webrtc::{AdmMode, PeerConnectionFactory};
///
/// let factory = PeerConnectionFactory::with_adm(AdmMode::Platform)?;
/// # Ok::<_, reactor_webrtc::Error>(())
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ApmConfig {
    /// AEC3 acoustic echo cancellation: removes loudspeaker echo from the
    /// microphone signal. Required when mic and speaker are in the same room;
    /// unnecessary (and potentially harmful) with headphones or line-in.
    pub echo_canceller: bool,
    /// Noise suppression (WebRTC NS at kHigh level): attenuates stationary
    /// background noise (fans, HVAC). Safe to leave off if the capture
    /// environment is quiet or the source is already noise-gated.
    pub noise_suppression: bool,
    /// Automatic gain control (gain_controller1 / AGC1): normalises microphone
    /// volume across devices. Disable when the calling app manages gain itself
    /// or when pushing synthetic PCM.
    pub agc: bool,
    /// High-pass filter: removes DC offset and low-frequency rumble below
    /// ~80 Hz. Costs almost nothing; usually safe to leave on for real mics.
    pub high_pass_filter: bool,
}

impl ApmConfig {
    fn to_flags(self) -> c_int {
        let mut f: c_int = 0;
        if self.echo_canceller {
            f |= 0x01;
        }
        if self.noise_suppression {
            f |= 0x02;
        }
        if self.agc {
            f |= 0x04;
        }
        if self.high_pass_filter {
            f |= 0x08;
        }
        f
    }
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
    /// Create a factory with the given [`AdmMode`] and **no APM processing**
    /// (true audio passthrough — no echo cancellation, noise suppression, AGC,
    /// or high-pass filter). Use this when your app does its own DSP, or when
    /// you are using [`AdmMode::Synthetic`] and pushing pre-processed PCM.
    pub fn with_adm(mode: AdmMode) -> Result<Self> {
        Self::with_adm_apm(mode, ApmConfig::default())
    }

    /// Create a factory with explicit control over both the audio device module
    /// and the APM (audio processing) chain.
    ///
    /// `apm` defaults to all-false (no processing). Set individual flags to
    /// enable only the processing stages you need. See [`ApmConfig`] for
    /// guidance on when each stage is appropriate.
    pub fn with_adm_apm(mode: AdmMode, apm: ApmConfig) -> Result<Self> {
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_factory_create_with_adm_apm(
                matches!(mode, AdmMode::Platform) as c_int,
                apm.to_flags(),
            )
        };
        if raw.is_null() {
            return Err(Error::Webrtc("factory creation returned null".into()));
        }
        Ok(Self { raw })
    }

    /// Create a factory using the **synthetic** audio device module — no audio
    /// hardware; feed audio with [`PeerConnectionFactory::push_audio_frame`].
    pub fn new() -> Result<Self> {
        Self::with_adm(AdmMode::Synthetic)
    }

    /// Create a factory using the **platform** audio device module (real
    /// mic/speaker — CoreAudio on macOS, WASAPI on Windows, ALSA/PulseAudio on
    /// Linux) with the full APM chain enabled: AEC3 echo cancellation, noise
    /// suppression, AGC, and high-pass filter.
    ///
    /// This is the recommended starting point for desktop client apps. If you
    /// need the platform ADM with specific processing stages disabled (e.g. no
    /// AGC because your app manages gain), use [`PeerConnectionFactory::with_adm_apm`]
    /// directly.
    pub fn with_platform_adm() -> Result<Self> {
        Self::with_adm_apm(
            AdmMode::Platform,
            ApmConfig {
                echo_canceller: true,
                noise_suppression: true,
                agc: true,
                high_pass_filter: true,
            },
        )
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
                0, // apm_flags: all disabled
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
        let native = config.to_native()?;
        // libwebrtc reports why it rejected the configuration (an empty TURN
        // credential, for example); the glue copies that reason in here.
        let mut err = [0 as std::os::raw::c_char; 256];
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create(
                self.raw,
                &native.config(),
                &callbacks,
                err.as_mut_ptr(),
                err.len() as c_int,
            )
        };
        if raw.is_null() {
            let reason = unsafe { std::ffi::CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(Error::Webrtc(if reason.is_empty() {
                "peer connection creation returned null".into()
            } else {
                format!("peer connection creation failed: {reason}")
            }));
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

    /// Create a local audio track with a per-track audio source, independent of
    /// the factory ADM. Feed samples with [`Track::push_pcm`]. This allows
    /// different audio to be delivered to different peer connections, since each
    /// call returns a track backed by its own source.
    pub fn create_audio_track_with_local_source(&self, id: &str) -> Result<Track> {
        let cid = CString::new(id).map_err(|_| Error::Webrtc("id contains a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_audio_track_create_with_local_source(
                self.raw,
                cid.as_ptr(),
            )
        };
        if raw.is_null() {
            return Err(Error::Webrtc(
                "audio track with local source creation returned null".into(),
            ));
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
