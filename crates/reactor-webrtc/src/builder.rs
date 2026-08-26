//! [`PeerConnectionFactoryBuilder`] — the composable entry point for every
//! [`PeerConnectionFactory`].

use crate::{AdmMode, ApmConfig, PeerConnectionFactory, Result};

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
    #[cfg(feature = "openh264")]
    openh264: Option<std::path::PathBuf>,
}

impl PeerConnectionFactoryBuilder {
    pub(crate) fn new() -> Self {
        Self {
            adm: AdmMode::Synthetic,
            apm: ApmConfig::default(),
            metadata: true,
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
    ///
    /// **Playout is automatic and unconditional here**: every inbound audio
    /// track's decoded frames reach the speaker by default (there is no
    /// per-track "played on speaker" switch, and `on_frame` sinks are taps
    /// they don't divert). To keep an inbound audio silent in that setup you
    /// must neutralize its transceiver (e.g. `set_direction` → Inactive) or
    /// use the synthetic ADM, where nothing plays on its own.
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
        // Every factory wires the encoder registry: per-track encoder options
        // (pre-encoded / inline) need no factory-level heads-up. The
        // `has_custom_slots` predicate keeps factories without custom slots
        // out of the H264/H265 advertisement.
        let registry = crate::encoded::EncoderRegistry::new();
        let mut opts = reactor_webrtc_sys::ReactorFactoryOptions {
            use_platform_adm: matches!(self.adm, AdmMode::Platform) as std::os::raw::c_int,
            apm_flags: self.apm.to_flags(),
            #[cfg(feature = "openh264")]
            openh264_lib_path: openh264_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            ..Default::default()
        };
        registry.install_into(&mut opts);
        // Ownership of the registry state transfers to the glue at
        // encoder-state construction, which the glue only performs after
        // every fallible step — any earlier failure keeps the binding's
        // ownership, and a binding-allocated state then leaks nowhere
        // (there is simply nothing cleaning it up here: the glue's own
        // destructor path never runs for those failures).
        PeerConnectionFactory::create_from_options(&opts, self.metadata, registry.clone())
    }
}
