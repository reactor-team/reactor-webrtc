//! Standalone video encoders and decoders: libwebrtc's codecs without a
//! [`PeerConnection`](crate::PeerConnection).
//!
//! [`VideoCodecs`] collects the backends this process can run, and creates a
//! [`VideoEncoder`] or a [`VideoDecoder`] for a [`VideoCodec`]. The codec is a
//! parameter, not a separate API. Today only [`VideoCodec::H264`] is
//! supported; [`VideoCodecs::capabilities`] lists what this process can run.
//!
//! ```rust,ignore
//! use reactor_webrtc::codec::{VideoCodecs, VideoEncoderConfig};
//! use reactor_webrtc::{PixelFormat, VideoCodec};
//!
//! let codecs = VideoCodecs::new()?;
//! let mut encoder = codecs.encoder(&VideoEncoderConfig::new(VideoCodec::H264, 1280, 720))?;
//! let mut decoder = codecs.decoder(VideoCodec::H264)?;
//! if let Some(encoded) = encoder.encode(&rgb, 1280, 720, PixelFormat::Rgb24, timestamp_us)? {
//!     let picture = decoder.decode(&encoded.data, encoded.timestamp_us, PixelFormat::Rgb24)?;
//! }
//! ```
//!
//! # Backends
//!
//! | Codec | Hardware | Software |
//! |-------|----------|----------|
//! | H.264 | VideoToolbox (macOS, iOS) | OpenH264, with the `openh264` feature ([`VideoCodecs::with_openh264`]) |
//!
//! A codec is created on the first backend its [`Backend`] allows that
//! accepts it, hardware first. Each encoder and decoder reports the backend it
//! got ([`VideoEncoder::implementation`], [`VideoEncoder::is_hardware`]).
//!
//! # Bitstream format
//!
//! One call to [`VideoEncoder::encode`] gives one encoded frame, and
//! [`VideoDecoder::decode`] takes one:
//!
//! | Codec | One encoded frame |
//! |-------|-------------------|
//! | H.264 | one access unit, Annex B (start codes), constrained baseline; a key frame carries its SPS and PPS |
//!
//! # Key frames
//!
//! The encoder makes a key frame for its first frame, for the first frame of a
//! new size, for the frame after a failed call, and for the frame after
//! [`VideoEncoder::request_key_frame`]. A backend may add key frames of its
//! own, so read [`EncodedFrame::key_frame`] rather than predicting it. Every
//! other frame references the frame before it, never a later one: there are
//! no B-frames, and frames come out in the order they went in.
//!
//! The decoder needs a key frame first: after it is created, after
//! [`VideoDecoder::reset`], and after a failed call. Until then it refuses
//! every other frame with [`CodecError::NeedKeyFrame`] and leaves its state
//! untouched.
//!
//! # Bitrate
//!
//! libwebrtc's encoders are real-time and rate-controlled: they aim at
//! [`VideoEncoderConfig::bitrate_bps`] over a second of frames at
//! [`VideoEncoderConfig::framerate`], whatever timestamps the frames carry.
//! Change both with [`VideoEncoder::set_rates`]. A frame far above the target
//! is still encoded, at a lower quality; the backends here do not skip frames
//! to keep the rate, but one that does makes [`VideoEncoder::encode`] return
//! `Ok(None)`.
//!
//! # Threading
//!
//! Every call blocks until its frame is done: the output of a frame is
//! returned by the call that took it, never by a later one. An encoder or a
//! decoder is [`Send`] and is used from one thread at a time (`&mut self`).
//! Different encoders and decoders run in parallel. [`VideoCodecs`] is
//! [`Send`] + [`Sync`] and cheap to clone.

mod capabilities;
mod decoder;
mod encoder;

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::Arc;

pub use crate::VideoCodec;
pub use capabilities::{CodecCapabilities, CodecSupport};
pub use decoder::{DecodedFrame, VideoDecoder};
pub use encoder::{EncodedFrame, VideoEncoder, VideoEncoderConfig};

/// Which backends a codec may be created on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Backend {
    /// The hardware backend when there is one that accepts the settings, a
    /// software one otherwise.
    #[default]
    Auto,
    /// Hardware only: creation fails when no hardware backend accepts them.
    Hardware,
    /// Software only.
    Software,
}

impl Backend {
    fn raw(self) -> c_int {
        match self {
            Self::Auto => reactor_webrtc_sys::CODEC_BACKEND_AUTO,
            Self::Hardware => reactor_webrtc_sys::CODEC_BACKEND_HARDWARE,
            Self::Software => reactor_webrtc_sys::CODEC_BACKEND_SOFTWARE,
        }
    }
}

/// Why a standalone codec call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecError {
    /// No backend here runs this codec with these settings.
    Unsupported(String),
    /// The arguments are wrong (a buffer of the wrong size, an odd frame
    /// size, …). Nothing was handed to the codec.
    InvalidInput(String),
    /// The decoder needs a key frame and was given another frame. Nothing was
    /// decoded, and the decoder still waits for a key frame.
    NeedKeyFrame,
    /// The backend failed. The encoder makes its next frame a key frame, and
    /// the decoder waits for one.
    Backend(String),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(m) => write!(f, "codec not supported: {m}"),
            Self::InvalidInput(m) => write!(f, "invalid codec input: {m}"),
            Self::NeedKeyFrame => write!(f, "the decoder needs a key frame"),
            Self::Backend(m) => write!(f, "codec backend failed: {m}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// The native codec context, shared by every encoder and decoder made from it.
struct ContextHandle(*mut reactor_webrtc_sys::CodecContext);

// SAFETY: the context is immutable after creation and the glue serialises the
// factory calls it makes on behalf of encoders and decoders.
unsafe impl Send for ContextHandle {}
unsafe impl Sync for ContextHandle {}

impl Drop for ContextHandle {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_codec_context_destroy(self.0) }
    }
}

/// The video codec backends this process can run, and the place to create
/// encoders and decoders on them. See the [module docs](self) for the contract.
#[derive(Clone)]
pub struct VideoCodecs {
    ctx: Arc<ContextHandle>,
}

impl VideoCodecs {
    /// The platform's own codecs: VideoToolbox on macOS and iOS, none
    /// elsewhere yet.
    pub fn new() -> Result<Self, CodecError> {
        Self::create(std::ptr::null())
    }

    /// The platform's codecs plus Cisco's OpenH264 loaded from `lib_path` (see
    /// [`crate::openh264::ensure_available`]), as the software H.264 backend.
    /// Fails when the library does not load.
    ///
    /// Cisco's binary license requires [`crate::openh264::OPENH264_ATTRIBUTION`]
    /// in your app's licensing surface.
    #[cfg(feature = "openh264")]
    pub fn with_openh264(lib_path: &std::path::Path) -> Result<Self, CodecError> {
        let path = lib_path
            .to_str()
            .ok_or_else(|| CodecError::InvalidInput("OpenH264 path is not UTF-8".into()))?;
        let c = std::ffi::CString::new(path)
            .map_err(|_| CodecError::InvalidInput("OpenH264 path contains a NUL byte".into()))?;
        Self::create(c.as_ptr())
    }

    fn create(openh264: *const c_char) -> Result<Self, CodecError> {
        let mut err = ErrorBuffer::new();
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_codec_context_create(openh264, err.ptr(), err.cap())
        };
        if raw.is_null() {
            return Err(CodecError::Unsupported(err.message("codec context")));
        }
        Ok(Self {
            ctx: Arc::new(ContextHandle(raw)),
        })
    }

    /// The codecs this process can encode and decode, and on which backends.
    pub fn capabilities(&self) -> CodecCapabilities {
        capabilities::read(self.ctx.0)
    }

    /// Create an encoder. Fails with [`CodecError::Unsupported`] when no
    /// backend the config allows accepts it.
    pub fn encoder(&self, config: &VideoEncoderConfig) -> Result<VideoEncoder, CodecError> {
        VideoEncoder::create(Arc::clone(&self.ctx), config)
    }

    /// Create a decoder for `codec`, hardware first.
    pub fn decoder(&self, codec: VideoCodec) -> Result<VideoDecoder, CodecError> {
        self.decoder_on(codec, Backend::Auto)
    }

    /// Create a decoder for `codec` on the backends `backend` allows.
    pub fn decoder_on(
        &self,
        codec: VideoCodec,
        backend: Backend,
    ) -> Result<VideoDecoder, CodecError> {
        VideoDecoder::create(Arc::clone(&self.ctx), codec, backend)
    }
}

/// Whether `codec` is one the standalone codecs run at all, on any platform.
fn check_codec(codec: VideoCodec) -> Result<(), CodecError> {
    match codec {
        VideoCodec::H264 => Ok(()),
        other => Err(CodecError::Unsupported(format!(
            "{} is not available as a standalone codec yet",
            other.name()
        ))),
    }
}

/// Whether `data` holds a frame a decoder can start from. For H.264, an Annex B
/// access unit with an IDR slice.
pub(crate) fn is_key_frame(codec: VideoCodec, data: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => h264_nal_types(data).any(|t| t == 5),
        _ => false,
    }
}

/// The `nal_unit_type` of each NAL unit in an Annex B buffer.
fn h264_nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    // A start code is 00 00 01 (a four-byte 00 00 00 01 ends the same way);
    // the NAL header is the byte after it.
    data.windows(4)
        .filter(|w| w[0] == 0 && w[1] == 0 && w[2] == 1)
        .map(|w| w[3] & 0x1f)
}

/// A buffer the glue writes an error message into.
struct ErrorBuffer([c_char; 256]);

impl ErrorBuffer {
    fn new() -> Self {
        Self([0; 256])
    }

    fn ptr(&mut self) -> *mut c_char {
        self.0.as_mut_ptr()
    }

    fn cap(&self) -> c_int {
        self.0.len() as c_int
    }

    /// The message, or a generic one naming `what` when the glue left none.
    fn message(&self, what: &str) -> String {
        let s = unsafe { CStr::from_ptr(self.0.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if s.is_empty() {
            format!("{what} failed")
        } else {
            s
        }
    }
}

/// Read a NUL-terminated name the glue wrote into `buf`.
fn c_name(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_an_idr_after_either_start_code() {
        // SPS, PPS, IDR with four-byte and three-byte start codes.
        let key = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88,
        ];
        assert!(is_key_frame(VideoCodec::H264, &key));
        // A non-IDR slice.
        let delta = [0, 0, 0, 1, 0x41, 0x9a, 0x00];
        assert!(!is_key_frame(VideoCodec::H264, &delta));
        assert!(!is_key_frame(VideoCodec::H264, &[]));
    }

    #[test]
    fn refuses_codecs_it_does_not_run() {
        assert!(check_codec(VideoCodec::H264).is_ok());
        for codec in [
            VideoCodec::Vp8,
            VideoCodec::Vp9,
            VideoCodec::Av1,
            VideoCodec::H265,
        ] {
            assert!(matches!(
                check_codec(codec),
                Err(CodecError::Unsupported(_))
            ));
        }
    }
}
