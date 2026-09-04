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

use std::ffi::CString;
use std::os::raw::c_int;
use std::sync::Arc;

pub use builder::PeerConnectionFactoryBuilder;
pub use config::{
    BundlePolicy, ContinualGatheringPolicy, IceServer, IceTransportsType, RtcConfiguration,
    TcpCandidatePolicy,
};
pub use encoded::{
    EncodedFrame, EncodedVideoFrame, EncodedVideoTrack, EncoderFeedback, FrameAction,
    FrameDirection, FrameTransform, H264Backend, InlineEncoderCallback, LocalVideoTrack,
    PreEncodedOptions, RawVideoFrame, TrackVideoEncoder, VideoCodec, VideoTrackOptions,
};

/// Whether this build targets Apple (H.264 VideoToolbox backend exists).
pub(crate) const HAVE_VIDEO_TOOLBOX: bool = cfg!(target_vendor = "apple");
pub use media::{
    AudioFrame, AudioTrack, AudioTrackOptions, AudioTrackSource, MediaKind, RemoteTrack, Track,
    VideoFrame, VideoTrack,
};
pub use metadata::{
    FrameMetadata, FrameMetadataGate, FRAME_METADATA_ATTRIBUTE, FRAME_METADATA_VERSION,
};
pub use observer::PeerConnectionObserver;
pub use peer_connection::{
    DataChannel, DataChannelState, IceCandidate, IceCandidatePairState, IceCandidatePairStats,
    IceCandidateType, IceGatheringState, InboundRtpStats, OutboundRtpStats, PeerConnection,
    PeerConnectionState, RelayProtocol, SdpType, SessionDescription, StatsReport, StreamKind,
    Transceiver, TransceiverDirection,
};
/// Runtime download/verification/caching of Cisco's OpenH264 shared library,
/// and the required attribution string — registered with
/// [`PeerConnectionFactoryBuilder::with_openh264`].
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
    pub(crate) fn to_flags(self) -> c_int {
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
/// module (synthetic by default, or the platform device). Construct it with
/// [`PeerConnectionFactory::builder`].
pub struct PeerConnectionFactory {
    handle: Arc<FactoryHandle>,
    /// Factory-level kill switch for per-frame metadata: when `false`, every
    /// [`PeerConnection`] from this factory behaves like one created with
    /// `RtcConfiguration::frame_metadata` off, whatever each config says.
    metadata_enabled: bool,
    /// Per-track encoder slots (pre-encoded / inline), wired into the native
    /// factory at creation. Every factory has one; tracks register slots via
    /// [`PeerConnectionFactory::create_video_track_with_options`].
    registry: Arc<crate::encoded::EncoderRegistry>,
    /// Whether an OpenH264 library path was registered at build
    /// ([`PeerConnectionFactoryBuilder::with_openh264`]). Gates the explicit
    /// [`H264Backend::OpenH264`] per-track selection. (Whether the library
    /// itself loaded is a native-side concern — a failed load degrades to
    /// "no OpenH264 backend", it never fails the factory.)
    #[cfg_attr(not(feature = "openh264"), allow(dead_code))]
    openh264_registered: bool,
}

impl PeerConnectionFactory {
    /// Clone the refcounted native handle, so a [`PeerConnection`] or
    /// [`Track`] created from this factory can keep its threads alive past
    /// this value's own lifetime.
    pub(crate) fn handle(&self) -> Arc<FactoryHandle> {
        Arc::clone(&self.handle)
    }

    /// Start composing a factory — the replacement for the old one-shot
    /// constructors. Chain the knobs you need, then [`build`](PeerConnectionFactoryBuilder::build):
    ///
    /// ```rust,ignore
    /// let factory = PeerConnectionFactory::builder()
    ///     .with_platform_adm()
    ///     .with_metadata(false)
    ///     .build()?;
    /// ```
    pub fn builder() -> PeerConnectionFactoryBuilder {
        PeerConnectionFactoryBuilder::new()
    }

    /// Shared create path: every constructor shapes a
    /// [`reactor_webrtc_sys::ReactorFactoryOptions`] and lands here. The glue
    /// writes a reason into `err` when it returns null; a silent null gets the
    /// generic message.
    pub(crate) fn create_from_options(
        opts: &reactor_webrtc_sys::ReactorFactoryOptions,
        metadata_enabled: bool,
        registry: Arc<crate::encoded::EncoderRegistry>,
        openh264_registered: bool,
    ) -> Result<Self> {
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
            metadata_enabled,
            registry,
            openh264_registered,
        })
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
            config.frame_metadata && self.metadata_enabled,
        ))
    }

    /// Create a local video track backed by a push-able source
    /// ([`Track::push_video_frame`]); libwebrtc's builtin encoder pipeline
    /// encodes it. For encoder plumbing (pre-encoded / inline) use
    /// [`create_video_track_with_options`](Self::create_video_track_with_options).
    ///
    /// Slot assignment between encoder instances and tracks is **positional**:
    /// create tracks before (or in the same order as) the transceivers that
    /// carry them. Slots are reserved only after native creation succeeds,
    /// so a failed creation (bad id, native error) never leaves an orphan
    /// slot misbinding the next track's encoder.
    pub fn create_video_track(&self, id: &str) -> Result<VideoTrack> {
        let track = self.create_video_track_no_slot(id, self.metadata_enabled)?;
        self.registry.add_raw_slot(track.native_id());
        Ok(VideoTrack::wrap(track))
    }

    /// Create a local video track with per-track [`VideoTrackOptions`] —
    /// encoder plumbing for this track alone, alongside any number of other
    /// raw or encoded tracks on the same factory.
    ///
    /// ```rust,ignore
    /// // Pre-encoded: push already-encoded bytes whenever you produce them.
    /// let screen = factory.create_video_track_with_options("screen", {
    ///     let mut o = VideoTrackOptions::default();
    ///     o.encoder = Some(TrackVideoEncoder::PreEncoded(PreEncodedOptions::new(1920, 1080)));
    ///     o
    /// })?;
    /// if let LocalVideoTrack::Encoded(enc) = screen {
    ///     enc.push_encoded_frame(frame);
    /// }
    /// ```
    ///
    /// The same positional slot-assignment rule as
    /// [`create_video_track`](Self::create_video_track) applies, and slots are
    /// likewise reserved only after a successful native creation.
    ///
    /// **One pre-encoded / inline track serves exactly one peer connection.**
    /// Each PeerConnection layers on its own encoder instance, and the
    /// registry binds them positionally by reservation — a second PC wired to
    /// the same track finds no reservation and falls back to the builtin
    /// encoder, which encodes the track's raw pushes as ordinary video
    /// (grey/dropped output, not a copy of your bitstream). Create one
    /// encoder-carrying track per PeerConnection instead of sharing.
    pub fn create_video_track_with_options(
        &self,
        id: &str,
        options: VideoTrackOptions,
    ) -> Result<LocalVideoTrack> {
        if options.encoder.is_some() && options.h264_backend.is_some() {
            return Err(Error::Webrtc(
                "h264_backend with a custom encoder: the track's bytes come \
                 from your own pipeline — there is no backend to route to"
                    .into(),
            ));
        }
        match options.encoder {
            None => {
                let pref = self.h264_backend_pref(options.h264_backend)?;
                let track = VideoTrack::wrap(self.create_video_track_no_slot(
                    id,
                    self.track_metadata_enabled(options.frame_metadata),
                )?);
                self.registry
                    .add_raw_slot_with_backend(track.native_id(), pref);
                Ok(LocalVideoTrack::Raw(track))
            }
            Some(TrackVideoEncoder::PreEncoded(o)) => {
                let track = VideoTrack::wrap(self.create_video_track_no_slot(
                    id,
                    self.track_metadata_enabled(options.frame_metadata),
                )?);
                let (queue, feedback) = self.registry.add_encoded_slot(track.native_id());
                Ok(LocalVideoTrack::Encoded(EncodedVideoTrack::new(
                    track, queue, feedback, o.width, o.height,
                )))
            }
            Some(TrackVideoEncoder::Inline(cb)) => {
                let track = VideoTrack::wrap(self.create_video_track_no_slot(
                    id,
                    self.track_metadata_enabled(options.frame_metadata),
                )?);
                let feedback = self.registry.add_inline_slot(track.native_id(), cb);
                crate::encoded::register_feedback_binding(track.native_id(), &feedback);
                Ok(LocalVideoTrack::Raw(track))
            }
        }
    }

    /// Validate an explicit [`H264Backend`] choice against what this build /
    /// this factory can actually serve, and map it to the slot preference.
    fn h264_backend_pref(
        &self,
        backend: Option<H264Backend>,
    ) -> Result<crate::encoded::H264BackendPref> {
        use crate::encoded::H264BackendPref as Pref;
        match backend {
            None => Ok(Pref::Auto),
            Some(H264Backend::VideoToolbox) if !HAVE_VIDEO_TOOLBOX => Err(Error::Webrtc(
                "H264Backend::VideoToolbox is only available on Apple platforms".into(),
            )),
            Some(H264Backend::VideoToolbox) => Ok(Pref::VideoToolbox),
            #[cfg(feature = "openh264")]
            Some(H264Backend::OpenH264) if !self.openh264_registered => Err(Error::Webrtc(
                "H264Backend::OpenH264 requires registering the library first \
                 (PeerConnectionFactory::builder().with_openh264(path))"
                    .into(),
            )),
            #[cfg(feature = "openh264")]
            Some(H264Backend::OpenH264) => Ok(Pref::OpenH264),
        }
    }

    /// Effective frame-metadata on a track: the factory kill switch is a
    /// process-wide **off** that a per-track `Some(true)` can never
    /// re-enable — `None` simply defers to the factory's setting.
    fn track_metadata_enabled(&self, track_flag: Option<bool>) -> bool {
        self.metadata_enabled && track_flag.unwrap_or(true)
    }

    /// Shared native side of [`create_video_track`](Self::create_video_track):
    /// the strict FFI call, **without** touching the registry — callers
    /// reserve their slot *after* this succeeds (so a failed create never
    /// leaves an orphan positional slot behind).
    fn create_video_track_no_slot(&self, id: &str, metadata_enabled: bool) -> Result<Track> {
        let cid = CString::new(id).map_err(|_| Error::Webrtc("id contains a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_track_create(self.handle.raw(), cid.as_ptr())
        };
        if raw.is_null() {
            return Err(Error::Webrtc("video track creation returned null".into()));
        }
        Ok(Track::from_raw_with_registry(
            raw,
            MediaKind::Video,
            self.handle(),
            false,
            metadata_enabled,
            Some(Arc::clone(&self.registry)),
        ))
    }

    /// Create a local audio track. Its samples come from this factory's ADM —
    /// feed it with [`PeerConnectionFactory::push_audio_frame`]. For per-track
    /// sources and processing constraints use
    /// [`create_audio_track_with_options`](Self::create_audio_track_with_options).
    pub fn create_audio_track(&self, id: &str) -> Result<AudioTrack> {
        self.create_audio_track_with_options(id, AudioTrackOptions::default())
    }

    /// Create a local audio track with a per-track audio source, independent of
    /// the factory ADM. Feed samples with [`Track::push_pcm`].
    ///
    /// **Deprecated:** retained for 0.12 source compatibility; use
    /// [`create_audio_track_with_options`](Self::create_audio_track_with_options)
    /// with [`AudioTrackSource::LocalPush`] instead. Accepting that the type
    /// is now `AudioTrack` — push helpers didn't change names, though; calls
    /// typed against [`Track`] adapt trivially (`let track: AudioTrack`).
    #[deprecated(note = "use create_audio_track_with_options with AudioTrackSource::LocalPush")]
    pub fn create_audio_track_with_local_source(&self, id: &str) -> Result<AudioTrack> {
        self.create_audio_track_with_options(
            id,
            AudioTrackOptions {
                source: AudioTrackSource::LocalPush,
                ..Default::default()
            },
        )
    }

    /// Create a local audio track with per-track [`AudioTrackOptions`] —
    /// choose the source (factory ADM vs independent push source) and the
    /// per-source processing constraints (AEC / noise suppression / AGC /
    /// high-pass), alongside any number of other audio tracks on the same
    /// factory — the mic + music scenario, or different audio per peer.
    ///
    /// For [`AudioTrackSource::LocalPush`] tracks, feed samples with
    /// [`Track::push_pcm`]; the returned [`Track`] handles both cases (push
    /// methods are documented no-ops where they don't apply).
    pub fn create_audio_track_with_options(
        &self,
        id: &str,
        options: AudioTrackOptions,
    ) -> Result<AudioTrack> {
        let cid = CString::new(id).map_err(|_| Error::Webrtc("id contains a NUL byte".into()))?;
        let tri = |v: Option<bool>| v.map_or(-1, |b| b as c_int);
        let sys_opts = reactor_webrtc_sys::ReactorAudioTrackOptions {
            source: match options.source {
                AudioTrackSource::Adm => 0,
                AudioTrackSource::LocalPush => 1,
            },
            echo_cancellation: tri(options.echo_cancellation),
            noise_suppression: tri(options.noise_suppression),
            auto_gain_control: tri(options.auto_gain_control),
            high_pass_filter: tri(options.high_pass_filter),
            ..Default::default()
        };
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_audio_track_create(
                self.handle.raw(),
                cid.as_ptr(),
                &sys_opts,
            )
        };
        if raw.is_null() {
            return Err(Error::Webrtc("audio track creation returned null".into()));
        }
        Ok(AudioTrack::wrap(Track::from_raw(
            raw,
            MediaKind::Audio,
            self.handle(),
            false,
            true, // metadata is a video feature; audio stays neutral
        )))
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
