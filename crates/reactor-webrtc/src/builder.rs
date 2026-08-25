//! Builders: [`PeerConnectionFactoryBuilder`] — the composable entry point
//! for every [`PeerConnectionFactory`] — and [`EncodedVideoBuilder`] for a
//! factory plus a mix of raw and pre-encoded video tracks.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::encoded::{EncodedVideoFrame, EncodedVideoTrack, EncoderRegistry};
use crate::media::Track;
use crate::{AdmMode, ApmConfig, CustomVideoEncoder, PeerConnectionFactory, Result};

/// Builds a [`PeerConnectionFactory`] knob by knob — the single entry point
/// that replaced the old mutually-exclusive constructors (they could not
/// compose: custom encode used to force the synthetic ADM, OpenH264 could not
/// share a factory with a custom encoder, and so on).
///
/// Every knob is optional; the defaults are a headless, no-processing factory
/// — `PeerConnectionFactory::builder().build()?` is exactly what the removed
/// `PeerConnectionFactory::new()` produced. Chain only what you need:
///
/// ```rust,ignore
/// let factory = PeerConnectionFactory::builder()
///     .with_platform_adm()                    // real mic/speaker + full APM chain
///     .with_metadata(false)                   // kill frame metadata factory-wide
///     .build()?;
/// ```
///
/// The knobs are the process-physical singletons libwebrtc only accepts at
/// factory-creation time: the audio device (ADM), the audio-processing chain
/// (APM), and codec backends loaded once per process (OpenH264). Everything
/// else — track kinds, per-track encoder choices, per-track metadata —
/// belongs to track creation, not the builder.
pub struct PeerConnectionFactoryBuilder {
    adm: AdmMode,
    apm: ApmConfig,
    metadata: bool,
    custom_encoder: Option<CustomVideoEncoder>,
    #[cfg(feature = "openh264")]
    openh264: Option<std::path::PathBuf>,
}

impl PeerConnectionFactoryBuilder {
    pub(crate) fn new() -> Self {
        Self {
            adm: AdmMode::Synthetic,
            apm: ApmConfig::default(),
            metadata: true,
            custom_encoder: None,
            #[cfg(feature = "openh264")]
            openh264: None,
        }
    }

    /// Which audio device module the factory owns. Default:
    /// [`AdmMode::Synthetic`] (headless; feed audio with
    /// [`PeerConnectionFactory::push_audio_frame`]).
    pub fn with_adm(mut self, mode: AdmMode) -> Self {
        self.adm = mode;
        self
    }

    /// Explicitly select the **synthetic** ADM — the default; exists for
    /// readable chains.
    pub fn with_synthetic_adm(self) -> Self {
        self.with_adm(AdmMode::Synthetic)
    }

    /// Use the **platform** audio device module (real mic/speaker, e.g.
    /// CoreAudio on macOS) and enable the full AEC3 + noise suppression +
    /// AGC + high-pass chain — the sensible default for real hardware
    /// capture. [`with_apm`](Self::with_apm) afterwards overrides the chain
    /// piece by piece.
    pub fn with_platform_adm(mut self) -> Self {
        self.adm = AdmMode::Platform;
        self.apm = ApmConfig {
            echo_canceller: true,
            noise_suppression: true,
            agc: true,
            high_pass_filter: true,
        };
        self
    }

    /// Configure the audio-processing chain (all stages default to off;
    /// `None`-valued per-track options later inherit from here).
    pub fn with_apm(mut self, apm: ApmConfig) -> Self {
        self.apm = apm;
        self
    }

    /// Factory-level kill switch for per-frame metadata (default `true`).
    /// With `false`, every peer connection from this factory behaves like one
    /// created with `RtcConfiguration::frame_metadata` off — offers do not
    /// advertise the capability, answers stay silent, and `user_data` passed
    /// to a push is dropped — whatever each connection's config says.
    pub fn with_metadata(mut self, enabled: bool) -> Self {
        self.metadata = enabled;
        self
    }

    /// Register the OpenH264 backend for real H.264 encode/decode (see
    /// [`crate::openh264::ensure_available`] to obtain `lib_path`). This
    /// never fails because the library itself couldn't be loaded: a
    /// non-loadable path degrades to "no OpenH264 backend" — H264 is simply
    /// not offered by it, and peers negotiate VP8/VP9/AV1 as usual.
    ///
    /// Cisco's binary license conditions the royalty carve-out on showing
    /// [`crate::openh264::OPENH264_ATTRIBUTION`] in your app's
    /// licensing/EULA surface — see that constant's doc comment.
    #[cfg(feature = "openh264")]
    pub fn with_openh264(mut self, lib_path: &std::path::Path) -> Self {
        self.openh264 = Some(lib_path.to_path_buf());
        self
    }

    /// Transitional (post-constructor) hook for the still-public
    /// [`PeerConnectionFactory::with_custom_video_encoder`] /
    /// [`EncodedVideoBuilder`] path — replaced by per-track encoder options
    /// in an upcoming change.
    pub(crate) fn with_custom_video_encoder(mut self, encoder: CustomVideoEncoder) -> Self {
        self.custom_encoder = Some(encoder);
        self
    }

    /// Finalise the factory. Fails on malformed options (an OpenH264 path
    /// with a NUL byte) or on factory/thread construction — the error carries
    /// the reason the glue reported.
    pub fn build(self) -> Result<PeerConnectionFactory> {
        #[cfg(feature = "openh264")]
        let openh264_c = match &self.openh264 {
            Some(p) => Some(
                std::ffi::CString::new(p.to_string_lossy().into_owned()).map_err(|_| {
                    crate::Error::Webrtc("openh264 lib_path contains a NUL byte".into())
                })?,
            ),
            None => None,
        };
        let encoder = self.custom_encoder.as_ref();
        let opts = reactor_webrtc_sys::ReactorFactoryOptions {
            use_platform_adm: matches!(self.adm, AdmMode::Platform) as std::os::raw::c_int,
            apm_flags: self.apm.to_flags(),
            #[cfg(feature = "openh264")]
            openh264_lib_path: openh264_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            encode_cb: encoder.map(|e| e.encode_fn),
            encode_userdata: encoder.map_or(std::ptr::null_mut(), |e| e.userdata),
            encode_free_ud: encoder.and_then(|e| e.free_ud),
            encode_use_builtin: encoder.and_then(|e| e.use_builtin),
            ..Default::default()
        };
        let result = PeerConnectionFactory::create_from_options(&opts, self.metadata);
        if result.is_err() {
            // Factory did not take ownership — free the encoder state.
            if let Some(e) = self.custom_encoder {
                if let Some(f) = e.free_ud {
                    f(e.userdata);
                }
            }
        }
        result
    }
}

/// One video track produced by [`EncodedVideoBuilder::build`].
///
/// - [`Raw`](MixedVideoTrack::Raw) — push BGRA frames with
///   [`Track::push_video_frame`]; libwebrtc encodes to VP8/VP9/AV1.
/// - [`Encoded`](MixedVideoTrack::Encoded) — push pre-encoded bytes with
///   [`EncodedVideoTrack::push_encoded_frame`]; libwebrtc only packetises.
pub enum MixedVideoTrack {
    Raw(Track),
    Encoded(EncodedVideoTrack),
}

impl MixedVideoTrack {
    /// The underlying [`Track`] handle — attach this to a transceiver with
    /// [`Transceiver::set_track`](crate::Transceiver::set_track).
    pub fn track(&self) -> &Track {
        match self {
            Self::Raw(t) => t,
            Self::Encoded(e) => e.track(),
        }
    }

    /// Returns the inner [`EncodedVideoTrack`], or `None` if this is a raw track.
    pub fn as_encoded(&self) -> Option<&EncodedVideoTrack> {
        match self {
            Self::Encoded(e) => Some(e),
            Self::Raw(_) => None,
        }
    }

    /// Returns the inner raw [`Track`], or `None` if this is a pre-encoded track.
    pub fn as_raw(&self) -> Option<&Track> {
        match self {
            Self::Raw(t) => Some(t),
            Self::Encoded(_) => None,
        }
    }
}

enum SlotConfig {
    Raw {
        id: String,
    },
    Encoded {
        id: String,
        width: u32,
        height: u32,
        queue: Arc<Mutex<VecDeque<EncodedVideoFrame>>>,
    },
}

/// Builds a [`PeerConnectionFactory`] wired for **multiple** video tracks that
/// may be a mix of raw (builtin encoder) and pre-encoded (custom encoder).
///
/// ```rust,ignore
/// let mut b = PeerConnectionFactory::encoded_video_builder();
///
/// // Raw track: push BGRA → libwebrtc encodes → RTP
/// let cam_idx = b.add_raw_track("camera", 1280, 720);
///
/// // Pre-encoded track: push your own bytes → RTP
/// let scr_idx = b.add_encoded_track("screen", 1920, 1080);
///
/// let (factory, tracks) = b.build()?;
///
/// if let MixedVideoTrack::Raw(cam) = &tracks[cam_idx] {
///     cam.push_video_frame(&bgra, 1280, 720);
/// }
/// if let MixedVideoTrack::Encoded(scr) = &tracks[scr_idx] {
///     scr.push_encoded_frame(screen_frame);
/// }
/// ```
///
/// For a single pre-encoded track the convenience method
/// [`PeerConnectionFactory::with_encoded_video_track`] is simpler.
pub struct EncodedVideoBuilder {
    registry: Arc<EncoderRegistry>,
    slots: Vec<SlotConfig>,
}

impl EncodedVideoBuilder {
    pub(crate) fn new() -> Self {
        Self {
            registry: EncoderRegistry::new(),
            slots: Vec::new(),
        }
    }

    /// Add a **raw** video track. Push BGRA frames with
    /// [`Track::push_video_frame`]; libwebrtc's builtin VP8/VP9/AV1 encoder
    /// handles compression. No `width`/`height` needed here — set them per frame.
    ///
    /// Returns the index into the `Vec<MixedVideoTrack>` that [`build`] produces.
    pub fn add_raw_track(&mut self, id: &str) -> usize {
        self.registry.add_raw_slot();
        let idx = self.slots.len();
        self.slots.push(SlotConfig::Raw { id: id.to_owned() });
        idx
    }

    /// Add a **pre-encoded** video track. Push encoded bytes with
    /// [`EncodedVideoTrack::push_encoded_frame`]; libwebrtc only packetises.
    ///
    /// Returns the index into the `Vec<MixedVideoTrack>` that [`build`] produces.
    pub fn add_encoded_track(&mut self, id: &str, width: u32, height: u32) -> usize {
        let queue = self.registry.add_encoded_slot();
        let idx = self.slots.len();
        self.slots.push(SlotConfig::Encoded {
            id: id.to_owned(),
            width,
            height,
            queue,
        });
        idx
    }

    /// Convenience alias for [`add_encoded_track`] (backward compat).
    pub fn add_track(&mut self, id: &str, width: u32, height: u32) -> usize {
        self.add_encoded_track(id, width, height)
    }

    /// Finalise the factory and create all registered video tracks.
    ///
    /// Returns `(factory, tracks)` where `tracks[i]` corresponds to the i-th
    /// `add_*_track` call.
    pub fn build(self) -> Result<(PeerConnectionFactory, Vec<MixedVideoTrack>)> {
        let encoder = CustomVideoEncoder::from_registry(self.registry);
        let factory = PeerConnectionFactory::builder()
            .with_custom_video_encoder(encoder)
            .build()?;
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in self.slots {
            match slot {
                SlotConfig::Raw { id } => {
                    let track = factory.create_video_track(&id)?;
                    out.push(MixedVideoTrack::Raw(track));
                }
                SlotConfig::Encoded {
                    id,
                    width,
                    height,
                    queue,
                } => {
                    let track = factory.create_video_track(&id)?;
                    out.push(MixedVideoTrack::Encoded(EncodedVideoTrack::new(
                        track, queue, width, height,
                    )));
                }
            }
        }
        Ok((factory, out))
    }
}
