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
//! ICE credentials are libwebrtc's to generate, and it exposes no setter. When an
//! application needs to *choose* its ufrag — so that a fronting layer can route on
//! it — [`SessionDescription::with_ice_credentials`] substitutes them in the
//! description before it is set locally, which is where libwebrtc reads the
//! transport's ICE parameters from.
//!
//! Per-frame metadata ([`metadata`]) rides in a trailer appended to the encoded
//! payload, which only works if the peer strips it again. That is negotiated for
//! you: every [`PeerConnection::create_offer`] advertises the capability as a
//! session-level `a=x-reactor-frame-metadata`,
//! [`PeerConnection::create_answer`] mirrors an offer that asked for
//! it, and [`PeerConnection::set_remote_description`] arms a
//! [`FrameMetadataGate`] from what the remote declared, and installs the metadata
//! steps on the video transceivers. Callers pass `user_data` when it is meaningful
//! and never have to ask what the far end supports — a trailer reaches the wire only
//! when the peer said it strips them. Set
//! [`RtcConfiguration::frame_metadata`] to `false` to keep the whole thing out of a
//! connection.
//!
//! A [`FrameTransform`] of your own composes with that rather than displacing it:
//! the crate owns libwebrtc's single transformer slot per sender/receiver and runs
//! both, so encoded-frame access and per-frame metadata work on one transceiver.
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
mod sender_meta;

use std::collections::VecDeque;
use std::ffi::CString;
use std::os::raw::c_int;
use std::sync::{Arc, Mutex};

pub use builder::{EncodedVideoBuilder, MixedVideoTrack};
pub use config::{
    BundlePolicy, ContinualGatheringPolicy, IceServer, IceTransportsType, RtcConfiguration,
    TcpCandidatePolicy,
};
pub use encoded::{
    CustomVideoEncoder, EncodedFrame, EncodedVideoFrame, EncodedVideoTrack, FrameAction,
    FrameDirection, FrameTransform, RawVideoFrame, VideoCodec,
};
pub use media::{AudioFrame, MediaKind, Track, VideoFrame};
pub use metadata::{
    FrameMetadata, FrameMetadataGate, FRAME_METADATA_ATTRIBUTE, FRAME_METADATA_VERSION,
};
pub use observer::PeerConnectionObserver;
pub use peer_connection::{
    DataChannel, DataChannelState, IceCandidate, IceCandidatePairState, IceCandidatePairStats,
    IceGatheringState, InboundRtpStats, OutboundRtpStats, PeerConnection, PeerConnectionState,
    SdpType, SessionDescription, StatsReport, Transceiver, TransceiverDirection,
};
/// Runtime download/verification/caching of Cisco's OpenH264 shared library,
/// and the required attribution string — see [`PeerConnectionFactory::with_openh264`].
#[cfg(feature = "openh264")]
pub use reactor_webrtc_sys::openh264;

/// The ABI version of the linked native build. Used to assert that the safe
/// crate and the prebuilt `libwebrtc` agree.
pub fn native_abi_version() -> u32 {
    // Safe: a pure version getter with no arguments.
    unsafe { reactor_webrtc_sys::reactor_webrtc_abi_version() }
}

/// The engine's monotonic clock, in microseconds.
///
/// The epoch [`Track::push_video_frame_at`], [`Track::push_video_frame_with_metadata_at`],
/// and [`Track::push_pcm_at`] read their capture timestamps in. Read it once per
/// unit of produced media and stamp every track with that one value: audio and
/// video are synchronised by sharing a capture time, not by reaching the encoder
/// at the same moment.
pub fn time_micros() -> i64 {
    // Safe: a pure clock read with no arguments.
    unsafe { reactor_webrtc_sys::reactor_webrtc_time_micros() }
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
/// All fields default to `false` (no processing). Enable selectively:
/// ```
/// use reactor_webrtc::ApmConfig;
///
/// let apm = ApmConfig { echo_canceller: true, noise_suppression: true, ..Default::default() };
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ApmConfig {
    /// AEC3 acoustic echo cancellation.
    pub echo_canceller: bool,
    /// Noise suppression (level = kHigh when enabled).
    pub noise_suppression: bool,
    /// Automatic gain control (gain_controller1).
    pub agc: bool,
    /// High-pass filter.
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

/// The native factory pointer, refcounted so every object the factory produces
/// (a [`PeerConnection`], a [`Track`](crate::media::Track)) can hold a clone and
/// keep the factory's signaling/worker/network threads alive for as long as it
/// lives — including past the point where the [`PeerConnectionFactory`] value
/// itself is dropped. Without this, the factory's `Drop` tears those threads
/// down as soon as its own handle goes out of scope, and any object it produced
/// that outlives it is left holding a pointer into what those threads used to
/// back.
pub(crate) struct FactoryHandle(*mut reactor_webrtc_sys::PeerConnectionFactory);

// SAFETY: the native factory is internally thread-safe (it owns the WebRTC
// signaling/worker/network threads).
unsafe impl Send for FactoryHandle {}
unsafe impl Sync for FactoryHandle {}

impl FactoryHandle {
    pub(crate) fn raw(&self) -> *mut reactor_webrtc_sys::PeerConnectionFactory {
        self.0
    }
}

impl Drop for FactoryHandle {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_factory_destroy(self.0) }
    }
}

/// Entry point: creates peer connections and tracks, and owns the audio device
/// module (synthetic by default, or the platform device).
pub struct PeerConnectionFactory {
    handle: Arc<FactoryHandle>,
}

impl PeerConnectionFactory {
    /// Clone the refcounted native handle, so a [`PeerConnection`] or
    /// [`Track`] created from this factory can keep its threads alive past
    /// this value's own lifetime.
    pub(crate) fn handle(&self) -> Arc<FactoryHandle> {
        Arc::clone(&self.handle)
    }

    /// Create a factory with the given [`AdmMode`] and no APM processing.
    pub fn with_adm(mode: AdmMode) -> Result<Self> {
        Self::with_adm_apm(mode, ApmConfig::default())
    }

    /// Create a factory with full control over the audio device and APM chain.
    pub fn with_adm_apm(mode: AdmMode, apm: ApmConfig) -> Result<Self> {
        Self::create_from_options(&reactor_webrtc_sys::ReactorFactoryOptions {
            use_platform_adm: matches!(mode, AdmMode::Platform) as c_int,
            apm_flags: apm.to_flags(),
            ..Default::default()
        })
    }

    /// Shared create path: every constructor shapes a
    /// [`reactor_webrtc_sys::ReactorFactoryOptions`] and lands here. The glue
    /// writes a reason into `err` when it returns null; a silent null gets the
    /// generic message.
    fn create_from_options(opts: &reactor_webrtc_sys::ReactorFactoryOptions) -> Result<Self> {
        let mut err = [0 as std::os::raw::c_char; 256];
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_factory_create(
                opts,
                err.as_mut_ptr(),
                err.len() as c_int,
            )
        };
        if raw.is_null() {
            let reason = unsafe { std::ffi::CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(Error::Webrtc(if reason.is_empty() {
                "factory creation returned null".into()
            } else {
                format!("factory creation failed: {reason}")
            }));
        }
        Ok(Self {
            handle: Arc::new(FactoryHandle(raw)),
        })
    }

    /// Create a factory using the **synthetic** audio device module — no audio
    /// hardware; feed audio with [`PeerConnectionFactory::push_audio_frame`].
    pub fn new() -> Result<Self> {
        Self::with_adm(AdmMode::Synthetic)
    }

    /// Create a factory using the **platform** audio device module (real
    /// mic/speaker, e.g. CoreAudio on macOS) with the full AEC3 + noise
    /// suppression + AGC + high-pass chain enabled — the sensible default for
    /// real hardware capture.
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
        let result = Self::create_from_options(&reactor_webrtc_sys::ReactorFactoryOptions {
            encode_cb: Some(encoder.encode_fn),
            encode_userdata: encoder.userdata,
            encode_free_ud: encoder.free_ud,
            encode_use_builtin: encoder.use_builtin,
            use_platform_adm: 0, // synthetic ADM
            apm_flags: 0,        // all processing disabled
            ..Default::default()
        });
        if result.is_err() {
            // Factory did not take ownership — free the state ourselves.
            if let Some(f) = encoder.free_ud {
                f(encoder.userdata);
            }
        }
        result
    }

    /// Create a factory with real H.264 encode/decode backed by a
    /// dynamically loaded OpenH264 shared library — see
    /// [`crate::openh264::ensure_available`] to obtain `lib_path`. VP8/VP9/AV1
    /// remain builtin; only H264 is affected.
    ///
    /// This never fails because OpenH264 itself couldn't be loaded: if
    /// `lib_path` doesn't `dlopen`/`LoadLibraryW`, the factory still
    /// constructs, H264 is simply not advertised in SDP (peers negotiate
    /// VP8/VP9/AV1 as usual). Errors here mean factory/thread construction
    /// itself failed, same as [`Self::with_adm_apm`].
    ///
    /// Requires the `openh264` crate feature. Cisco's binary license
    /// conditions the royalty carve-out on showing
    /// [`crate::openh264::OPENH264_ATTRIBUTION`] in your app's
    /// licensing/EULA surface — see that constant's doc comment.
    #[cfg(feature = "openh264")]
    pub fn with_openh264(
        lib_path: &std::path::Path,
        mode: AdmMode,
        apm: ApmConfig,
    ) -> Result<Self> {
        let lib_path = CString::new(lib_path.to_string_lossy().into_owned())
            .map_err(|_| Error::Webrtc("lib_path contains a NUL byte".into()))?;
        Self::create_from_options(&reactor_webrtc_sys::ReactorFactoryOptions {
            openh264_lib_path: lib_path.as_ptr(),
            use_platform_adm: matches!(mode, AdmMode::Platform) as c_int,
            apm_flags: apm.to_flags(),
            ..Default::default()
        })
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
        let state = observer.into_state(self.handle());
        let callbacks = state.callbacks();
        let native = config.to_native()?;
        // libwebrtc reports why it rejected the configuration (an empty TURN
        // credential, for example); the glue copies that reason in here.
        let mut err = [0 as std::os::raw::c_char; 256];
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create(
                self.handle.raw(),
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
        Ok(PeerConnection::new(
            raw,
            state,
            self.handle(),
            config.frame_metadata,
        ))
    }

    /// Create a local video track backed by a push-able source
    /// ([`Track::push_video_frame`]).
    pub fn create_video_track(&self, id: &str) -> Result<Track> {
        let cid = CString::new(id).map_err(|_| Error::Webrtc("id contains a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_track_create(self.handle.raw(), cid.as_ptr())
        };
        if raw.is_null() {
            return Err(Error::Webrtc("video track creation returned null".into()));
        }
        Ok(Track::from_raw(raw, MediaKind::Video, self.handle()))
    }

    /// Create a local audio track. Its samples come from this factory's ADM —
    /// feed it with [`PeerConnectionFactory::push_audio_frame`].
    pub fn create_audio_track(&self, id: &str) -> Result<Track> {
        let cid = CString::new(id).map_err(|_| Error::Webrtc("id contains a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_audio_track_create(self.handle.raw(), cid.as_ptr())
        };
        if raw.is_null() {
            return Err(Error::Webrtc("audio track creation returned null".into()));
        }
        Ok(Track::from_raw(raw, MediaKind::Audio, self.handle()))
    }

    /// Create a local audio track with a per-track audio source, independent of
    /// the factory ADM. Feed samples with [`Track::push_pcm`]. This allows
    /// different audio to be delivered to different peer connections, since each
    /// call returns a track backed by its own source.
    pub fn create_audio_track_with_local_source(&self, id: &str) -> Result<Track> {
        let cid = CString::new(id).map_err(|_| Error::Webrtc("id contains a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_audio_track_create_with_local_source(
                self.handle.raw(),
                cid.as_ptr(),
            )
        };
        if raw.is_null() {
            return Err(Error::Webrtc(
                "audio track with local source creation returned null".into(),
            ));
        }
        Ok(Track::from_raw(raw, MediaKind::Audio, self.handle()))
    }

    /// Feed interleaved i16 PCM to the (synthetic) ADM, shared by all local
    /// audio tracks. Typically called with ~10ms blocks (e.g. 480 frames @
    /// 48kHz). No-op with the platform ADM.
    pub fn push_audio_frame(&self, pcm: &[i16], sample_rate: u32, channels: u32) {
        let channels = channels.max(1);
        let samples_per_channel = (pcm.len() / channels as usize) as c_int;
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_factory_push_audio_frame(
                self.handle.raw(),
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
                self.handle.raw(),
                enabled as c_int,
            )
        }
    }
}
