//! Standalone video encoders and decoders: libwebrtc's codecs without a
//! [`PeerConnection`](crate::PeerConnection).
//!
//! [`VideoCodecs`] collects the backends this process can run, and creates a
//! [`VideoEncoder`] or a [`VideoDecoder`] for a [`VideoCodec`]. The codec is a
//! parameter, not a separate API: [`VideoCodec::H264`], [`VideoCodec::Vp8`],
//! [`VideoCodec::Vp9`] and [`VideoCodec::Av1`] run standalone, and
//! [`VideoCodecs::capabilities`] lists which of them this process can run in
//! each direction.
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
//! | H.264 | VideoToolbox (macOS, iOS); NVENC to encode, NVDEC to decode | OpenH264, with the `openh264` feature ([`VideoCodecs::with_openh264`]) |
//! | VP8 | NVDEC to decode | libvpx |
//! | VP9 | NVDEC to decode | libvpx (profile 0) |
//! | AV1 | NVENC to encode, NVDEC to decode | libaom to encode, dav1d to decode (main profile) |
//!
//! NVDEC decodes and NVENC encodes on an NVIDIA GPU (Linux), each codec the
//! GPU runs, when asked for with [`VideoCodecsBuilder::nvdec`] and
//! [`VideoCodecsBuilder::nvenc`].
//!
//! The software VP8, VP9 and AV1 codecs are the ones built into libwebrtc. A
//! build without one of them (an AV1 encoder, say) leaves it out of
//! [`VideoCodecs::capabilities`] in that direction, and creating it fails
//! with [`CodecError::Unsupported`].
//!
//! A codec is created on the first backend its [`Backend`] allows that
//! accepts it, hardware first. Each encoder and decoder reports the backend it
//! got ([`VideoEncoder::implementation`], [`VideoEncoder::is_hardware`]).
//!
//! # Decoding into your own buffer
//!
//! [`VideoDecoder::decode_into`] writes the picture to a buffer the caller
//! hands over once the picture's size is known, converted once, in place,
//! with no copy behind it. [`VideoDecoder::decode`] is the same call into a
//! new `Vec`. Every backend takes it, hardware or software:
//!
//! ```no_run
//! # use reactor_webrtc::codec::{VideoCodecs, VideoDecoder};
//! # use reactor_webrtc::PixelFormat;
//! # fn f(decoder: &mut VideoDecoder, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
//! let mut picture = Vec::new(); // reused from one frame to the next
//! if let Some((width, height)) = decoder.decode_into(data, PixelFormat::Rgb24, |w, h| {
//!     picture.resize(w as usize * h as usize * 3, 0);
//!     Some(&mut picture[..])
//! })? {
//!     // `picture` holds the width x height RGB picture.
//! }
//! # Ok(()) }
//! ```
//!
//! # Bitstream format
//!
//! One call to [`VideoEncoder::encode`] gives one encoded frame, and
//! [`VideoDecoder::decode`] takes one:
//!
//! | Codec | One encoded frame |
//! |-------|-------------------|
//! | H.264 | one access unit, Annex B (start codes), constrained baseline; a key frame carries its SPS and PPS |
//! | VP8 | one frame, as the encoder emits it (RFC 6386) |
//! | VP9 | one frame, as the encoder emits it; one spatial layer, so no superframe |
//! | AV1 | one temporal unit of OBUs, each with its size field (low-overhead format); a key frame carries the sequence header |
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
    /// software one otherwise. A hardware decoder falls back to the codec's
    /// software decoder, from that frame on, on a stream the hardware does
    /// not decode (NVDEC below its minimum size, or a bit depth it does not
    /// take); it keeps reporting the hardware backend's name.
    #[default]
    Auto,
    /// Hardware only: creation fails when no hardware backend accepts them,
    /// and a decoder fails on a stream the hardware does not decode.
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

/// The optional backends of a [`VideoCodecs`], from [`VideoCodecs::builder`].
#[derive(Debug, Default, Clone)]
pub struct VideoCodecsBuilder {
    #[cfg(feature = "openh264")]
    openh264: Option<std::path::PathBuf>,
    nvdec: bool,
    nvenc: bool,
}

impl VideoCodecsBuilder {
    /// Add Cisco's OpenH264 loaded from `lib_path` (see
    /// [`crate::openh264::ensure_available`]) as the software H.264 backend.
    /// [`build`](Self::build) fails when the library does not load.
    ///
    /// Cisco's binary license requires [`crate::openh264::OPENH264_ATTRIBUTION`]
    /// in your app's licensing surface.
    #[cfg(feature = "openh264")]
    pub fn openh264(mut self, lib_path: &std::path::Path) -> Self {
        self.openh264 = Some(lib_path.to_path_buf());
        self
    }

    /// Add NVDEC as the hardware decoder of each of H.264, VP8, VP9 and AV1
    /// the first visible NVIDIA GPU decodes (`CUDA_VISIBLE_DEVICES` picks it).
    /// Linux only; it decodes, it does not encode. A codec the GPU does not
    /// decode, or every codec without a GPU, is left out rather than failing
    /// the build: [`VideoCodecs::capabilities`] lists an `NVDEC` hardware
    /// decoder for each codec it is there for.
    pub fn nvdec(mut self) -> Self {
        self.nvdec = true;
        self
    }

    /// Add NVENC as the hardware encoder of each of H.264 (constrained
    /// baseline) and AV1 (main) the first visible NVIDIA GPU encodes
    /// (`CUDA_VISIBLE_DEVICES` picks it); VP8 and VP9 have no NVENC encoder.
    /// Linux only. A codec the GPU does not encode, or every codec without a
    /// GPU, a driver or an encoder (an A100, H100 or B200 has none), is left
    /// out rather than failing the build: [`VideoCodecs::capabilities`] lists
    /// an `NVENC` hardware encoder for each codec it is there for. An encoder
    /// NVENC does not open (an odd size, one outside the GPU's limits, no
    /// session left) is created on the codec's software backend instead.
    pub fn nvenc(mut self) -> Self {
        self.nvenc = true;
        self
    }

    /// Collect the backends.
    pub fn build(self) -> Result<VideoCodecs, CodecError> {
        #[cfg(feature = "openh264")]
        let openh264 = match &self.openh264 {
            Some(path) => {
                let path = path
                    .to_str()
                    .ok_or_else(|| CodecError::InvalidInput("OpenH264 path is not UTF-8".into()))?;
                Some(std::ffi::CString::new(path).map_err(|_| {
                    CodecError::InvalidInput("OpenH264 path contains a NUL byte".into())
                })?)
            }
            None => None,
        };
        #[cfg(feature = "openh264")]
        let openh264_ptr = openh264.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        #[cfg(not(feature = "openh264"))]
        let openh264_ptr = std::ptr::null();
        VideoCodecs::create(openh264_ptr, self.nvdec, self.nvenc)
    }
}

impl VideoCodecs {
    /// The platform's own codecs: VideoToolbox on macOS and iOS, none
    /// elsewhere yet.
    pub fn new() -> Result<Self, CodecError> {
        Self::create(std::ptr::null(), false, false)
    }

    /// Choose the optional backends: OpenH264, NVDEC, NVENC. [`new`](Self::new) and
    /// `with_openh264` are shorthands for it.
    pub fn builder() -> VideoCodecsBuilder {
        VideoCodecsBuilder::default()
    }

    /// The platform's codecs plus Cisco's OpenH264 loaded from `lib_path` (see
    /// [`crate::openh264::ensure_available`]), as the software H.264 backend.
    /// Fails when the library does not load.
    ///
    /// Cisco's binary license requires [`crate::openh264::OPENH264_ATTRIBUTION`]
    /// in your app's licensing surface.
    #[cfg(feature = "openh264")]
    pub fn with_openh264(lib_path: &std::path::Path) -> Result<Self, CodecError> {
        Self::builder().openh264(lib_path).build()
    }

    fn create(openh264: *const c_char, nvdec: bool, nvenc: bool) -> Result<Self, CodecError> {
        let mut err = ErrorBuffer::new();
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_codec_context_create(
                openh264,
                nvdec as std::os::raw::c_int,
                nvenc as std::os::raw::c_int,
                err.ptr(),
                err.cap(),
            )
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
/// Whether this process has a backend for it is
/// [`VideoCodecs::capabilities`]'s answer.
fn check_codec(codec: VideoCodec) -> Result<(), CodecError> {
    match codec {
        VideoCodec::H264 | VideoCodec::Vp8 | VideoCodec::Vp9 | VideoCodec::Av1 => Ok(()),
        other => Err(CodecError::Unsupported(format!(
            "{} is not available as a standalone codec",
            other.name()
        ))),
    }
}

/// Whether `data` holds a frame a decoder can start from, in the bitstream
/// format of the [module docs](self#bitstream-format).
pub(crate) fn is_key_frame(codec: VideoCodec, data: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => h264_nal_types(data).any(|t| t == 5),
        VideoCodec::Vp8 => vp8_is_key_frame(data),
        VideoCodec::Vp9 => vp9_is_key_frame(data),
        VideoCodec::Av1 => av1_is_key_frame(data),
        VideoCodec::H265 => false,
    }
}

/// A VP8 key frame: the frame tag's type bit is 0 and the start code
/// `9d 01 2a` follows the three-byte tag (RFC 6386, 9.1).
fn vp8_is_key_frame(data: &[u8]) -> bool {
    data.len() >= 10 && data[0] & 1 == 0 && data[3..6] == [0x9d, 0x01, 0x2a]
}

/// A VP9 key frame: the uncompressed header's `frame_type` is `KEY_FRAME` and
/// the frame is not a `show_existing_frame` (VP9 spec, 6.2). A superframe
/// starts with its first frame, so the same bits are read.
fn vp9_is_key_frame(data: &[u8]) -> bool {
    let Some(&first) = data.first() else {
        return false;
    };
    let mut bits = (0..8).rev().map(|i| (first >> i) & 1);
    let mut next = || bits.next().unwrap_or(1);
    // frame_marker
    if (next(), next()) != (1, 0) {
        return false;
    }
    let profile = next() | (next() << 1);
    if profile == 3 && next() != 0 {
        return false; // reserved_zero
    }
    let show_existing_frame = next();
    let frame_type = next();
    show_existing_frame == 0 && frame_type == 0
}

const AV1_OBU_SEQUENCE_HEADER: u8 = 1;
const AV1_OBU_FRAME_HEADER: u8 = 3;
const AV1_OBU_FRAME: u8 = 6;

/// An AV1 temporal unit a decoder can start from: it carries a sequence
/// header, and its first frame header is a shown `KEY_FRAME` (AV1 spec, 5.9.2).
fn av1_is_key_frame(data: &[u8]) -> bool {
    // Set by the sequence header: whether frame headers are reduced, which
    // makes every frame a key frame.
    let mut reduced_still_picture = None;
    for (obu_type, payload) in av1_obus(data) {
        match obu_type {
            AV1_OBU_SEQUENCE_HEADER => {
                // seq_profile (3 bits), still_picture (1),
                // reduced_still_picture_header (1).
                let Some(&b) = payload.first() else {
                    return false;
                };
                reduced_still_picture = Some(b & 0x08 != 0);
            }
            AV1_OBU_FRAME_HEADER | AV1_OBU_FRAME => {
                let Some(reduced) = reduced_still_picture else {
                    return false;
                };
                if reduced {
                    return true;
                }
                // show_existing_frame (1 bit), frame_type (2 bits).
                let Some(&b) = payload.first() else {
                    return false;
                };
                return b & 0x80 == 0 && (b >> 5) & 0x03 == 0;
            }
            _ => {}
        }
    }
    false
}

/// The `(obu_type, payload)` of each OBU in a temporal unit in the
/// low-overhead format. Stops at the first malformed OBU. An OBU without a
/// size field runs to the end of the buffer.
fn av1_obus(mut data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        let (&header, rest) = data.split_first()?;
        if header & 0x80 != 0 {
            data = &[];
            return None; // obu_forbidden_bit
        }
        let obu_type = (header >> 3) & 0x0f;
        let has_extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let rest = if has_extension { rest.get(1..)? } else { rest };
        let (size, rest) = if has_size {
            leb128(rest)?
        } else {
            (rest.len(), rest)
        };
        let Some(payload) = rest.get(..size) else {
            data = &[];
            return None;
        };
        data = &rest[size..];
        Some((obu_type, payload))
    })
}

/// An AV1 `leb128()` value and the bytes after it.
fn leb128(data: &[u8]) -> Option<(usize, &[u8])> {
    let mut value: u64 = 0;
    for (i, &b) in data.iter().take(8).enumerate() {
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((usize::try_from(value).ok()?, &data[i + 1..]));
        }
    }
    None
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
    fn finds_a_vp8_key_frame_by_its_tag_and_start_code() {
        let key = [0x50, 0x2a, 0x00, 0x9d, 0x01, 0x2a, 0x80, 0x02, 0x68, 0x01];
        assert!(is_key_frame(VideoCodec::Vp8, &key));
        let mut delta = key;
        delta[0] |= 1;
        assert!(!is_key_frame(VideoCodec::Vp8, &delta));
        // A key frame tag without the start code is not one.
        assert!(!is_key_frame(
            VideoCodec::Vp8,
            &[0x50, 0x2a, 0x00, 0, 0, 0, 0, 0, 0, 0]
        ));
        assert!(!is_key_frame(VideoCodec::Vp8, &key[..9]));
    }

    #[test]
    fn finds_a_vp9_key_frame_in_the_uncompressed_header() {
        // Profile 0: marker 10, profile 00, show_existing 0, frame_type 0.
        assert!(is_key_frame(VideoCodec::Vp9, &[0x82, 0x49, 0x83, 0x42]));
        // frame_type 1: an inter frame.
        assert!(!is_key_frame(VideoCodec::Vp9, &[0x86, 0x00]));
        // show_existing_frame.
        assert!(!is_key_frame(VideoCodec::Vp9, &[0x88]));
        // Profile 3 has a reserved bit before show_existing_frame.
        assert!(is_key_frame(VideoCodec::Vp9, &[0xb0]));
        assert!(!is_key_frame(VideoCodec::Vp9, &[0xb2]));
        // A bad frame marker.
        assert!(!is_key_frame(VideoCodec::Vp9, &[0x02]));
        assert!(!is_key_frame(VideoCodec::Vp9, &[]));
    }

    /// An OBU of `obu_type` with a size field.
    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(obu_type << 3) | 0x02, payload.len() as u8];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn finds_an_av1_key_frame_after_a_sequence_header() {
        let td = obu(2, &[]);
        let seq = obu(AV1_OBU_SEQUENCE_HEADER, &[0x00, 0x00, 0x00]);
        // show_existing_frame 0, frame_type KEY_FRAME, show_frame 1.
        let key_frame = obu(AV1_OBU_FRAME, &[0x10, 0xaa]);
        // frame_type INTER_FRAME.
        let inter_frame = obu(AV1_OBU_FRAME, &[0x30, 0xaa]);

        let key = [td.clone(), seq.clone(), key_frame.clone()].concat();
        assert!(is_key_frame(VideoCodec::Av1, &key));
        // The same, through a FRAME_HEADER OBU.
        let header = obu(AV1_OBU_FRAME_HEADER, &[0x10]);
        assert!(is_key_frame(
            VideoCodec::Av1,
            &[td.clone(), seq.clone(), header].concat()
        ));
        // Without the sequence header a decoder cannot start there.
        assert!(!is_key_frame(
            VideoCodec::Av1,
            &[td.clone(), key_frame].concat()
        ));
        assert!(!is_key_frame(
            VideoCodec::Av1,
            &[td.clone(), seq.clone(), inter_frame].concat()
        ));
        // show_existing_frame.
        let existing = obu(AV1_OBU_FRAME_HEADER, &[0x80]);
        assert!(!is_key_frame(
            VideoCodec::Av1,
            &[td.clone(), seq, existing].concat()
        ));
        // Reduced still picture headers make every frame a key frame.
        let still = obu(AV1_OBU_SEQUENCE_HEADER, &[0x18]);
        assert!(is_key_frame(
            VideoCodec::Av1,
            &[still, obu(AV1_OBU_FRAME, &[0xff])].concat()
        ));
        // Truncated or malformed buffers are not key frames, and do not panic.
        for n in 0..key.len() {
            assert!(!is_key_frame(VideoCodec::Av1, &key[..n]), "{n} bytes");
        }
        assert!(!is_key_frame(VideoCodec::Av1, &[0x80, 0x00]));
        assert!(!is_key_frame(VideoCodec::Av1, &[0x0a, 0xff, 0xff]));
    }

    #[test]
    fn reads_multi_byte_obu_sizes() {
        let payload = vec![0u8; 200];
        let mut long = vec![(5 << 3) | 0x02, 0xc8, 0x01];
        long.extend_from_slice(&payload);
        let seq = obu(AV1_OBU_SEQUENCE_HEADER, &[0x00]);
        let frame = obu(AV1_OBU_FRAME, &[0x10]);
        assert!(is_key_frame(VideoCodec::Av1, &[long, seq, frame].concat()));
        assert_eq!(leb128(&[0xc8, 0x01, 0x07]), Some((200, &[0x07][..])));
        assert_eq!(leb128(&[0x80]), None);
    }

    #[test]
    fn refuses_codecs_it_does_not_run() {
        for codec in [
            VideoCodec::H264,
            VideoCodec::Vp8,
            VideoCodec::Vp9,
            VideoCodec::Av1,
        ] {
            assert!(check_codec(codec).is_ok(), "{codec:?}");
        }
        assert!(matches!(
            check_codec(VideoCodec::H265),
            Err(CodecError::Unsupported(_))
        ));
    }
}
