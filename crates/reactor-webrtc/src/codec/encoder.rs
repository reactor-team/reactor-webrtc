//! [`VideoEncoder`]: raw frames in, one encoded frame out per call.

use std::os::raw::c_char;
use std::sync::Arc;

use super::{c_name, check_codec, Backend, CodecError, ContextHandle, ErrorBuffer, VideoCodec};
use crate::PixelFormat;

/// Settings for [`VideoCodecs::encoder`](super::VideoCodecs::encoder).
/// `#[non_exhaustive]`: start from [`VideoEncoderConfig::new`] and assign
/// fields.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct VideoEncoderConfig {
    pub codec: VideoCodec,
    /// Frame size the encoder starts with, in pixels; both even. A frame of
    /// another size starts a new sequence at that size, with a key frame.
    pub width: u32,
    pub height: u32,
    /// Target bitrate, in bits per second. Default 2 Mbps.
    pub bitrate_bps: u32,
    /// The frame rate the bitrate is spread over, from 0.01 to 1000.
    /// Default 30.
    pub framerate: f64,
    /// Which backends the encoder may run on. Default [`Backend::Auto`].
    pub backend: Backend,
}

impl VideoEncoderConfig {
    /// `codec` at `width` x `height`, with the default rates on any backend.
    pub fn new(codec: VideoCodec, width: u32, height: u32) -> Self {
        Self {
            codec,
            width,
            height,
            bitrate_bps: 2_000_000,
            framerate: 30.0,
            backend: Backend::Auto,
        }
    }
}

/// One encoded frame, in the bitstream format of the
/// [module docs](super#bitstream-format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    /// Whether a decoder can start from this frame.
    pub key_frame: bool,
    /// Size of the encoded picture, in pixels.
    pub width: u32,
    pub height: u32,
    /// The timestamp the raw frame was given, unchanged.
    pub timestamp_us: i64,
}

/// A video encoder that runs outside a peer connection. See the
/// [module docs](super) for the bitrate, key frame, bitstream and threading
/// contract.
pub struct VideoEncoder {
    raw: *mut reactor_webrtc_sys::StandaloneVideoEncoder,
    /// Keeps the backend (and a loaded OpenH264) alive.
    _ctx: Arc<ContextHandle>,
    codec: VideoCodec,
    implementation: String,
    hardware: bool,
    /// The next frame must be a key frame: set at creation, by
    /// [`request_key_frame`](Self::request_key_frame) and by a failed call,
    /// and cleared only by a key frame coming out.
    key_frame_pending: bool,
    bitrate_bps: u32,
    framerate: f64,
}

// SAFETY: the native encoder is used by one thread at a time (`&mut self`);
// the glue synchronises with the backend's own callback threads.
unsafe impl Send for VideoEncoder {}

impl VideoEncoder {
    pub(super) fn create(
        ctx: Arc<ContextHandle>,
        config: &VideoEncoderConfig,
    ) -> Result<Self, CodecError> {
        check_codec(config.codec)?;
        check_size(config.width, config.height)?;
        if config.bitrate_bps == 0 {
            return Err(CodecError::InvalidInput("bitrate_bps is 0".into()));
        }
        check_framerate(config.framerate)?;
        let native = reactor_webrtc_sys::ReactorVideoEncoderConfig {
            size: std::mem::size_of::<reactor_webrtc_sys::ReactorVideoEncoderConfig>() as u32,
            codec: config.codec as i32,
            width: config.width as i32,
            height: config.height as i32,
            bitrate_bps: config.bitrate_bps,
            framerate: config.framerate,
            backend: config.backend.raw(),
        };
        let mut err = ErrorBuffer::new();
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_encoder_create(
                ctx.0,
                &native,
                err.ptr(),
                err.cap(),
            )
        };
        if raw.is_null() {
            return Err(CodecError::Unsupported(err.message("encoder creation")));
        }
        let mut name = [0 as c_char; 64];
        let hardware = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_encoder_info(
                raw,
                name.as_mut_ptr(),
                name.len() as i32,
            )
        } != 0;
        Ok(Self {
            raw,
            _ctx: ctx,
            codec: config.codec,
            implementation: c_name(&name),
            hardware,
            key_frame_pending: true,
            bitrate_bps: config.bitrate_bps,
            framerate: config.framerate,
        })
    }

    /// The codec this encoder produces.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The backend this encoder runs on, e.g. `"VideoToolbox"` or `"libvpx"`.
    pub fn implementation(&self) -> &str {
        &self.implementation
    }

    /// Whether that backend is the platform's hardware codec.
    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Make the next frame a key frame. Asking again before it is encoded
    /// changes nothing.
    pub fn request_key_frame(&mut self) {
        self.key_frame_pending = true;
    }

    /// Change the target bitrate (bits per second) and the frame rate it is
    /// spread over, from the next frame on. Refuses a bitrate of 0 and a frame
    /// rate outside 0.01 to 1000.
    pub fn set_rates(&mut self, bitrate_bps: u32, framerate: f64) -> Result<(), CodecError> {
        if bitrate_bps == 0 {
            return Err(CodecError::InvalidInput(format!(
                "rates {bitrate_bps} bps at {framerate} fps"
            )));
        }
        check_framerate(framerate)?;
        self.bitrate_bps = bitrate_bps;
        self.framerate = framerate;
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_encoder_set_rates(
                self.raw,
                bitrate_bps,
                framerate,
            )
        }
        Ok(())
    }

    /// The target bitrate, in bits per second.
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// The frame rate the bitrate is spread over.
    pub fn framerate(&self) -> f64 {
        self.framerate
    }

    /// Encode one frame of `format` (`width * height * bytes_per_pixel`
    /// bytes, rows without padding) and return its encoded frame, carrying
    /// `timestamp_us` back. Blocks until the backend is done with it.
    ///
    /// `Ok(None)` means the backend dropped the frame: the next frame still
    /// references the last one that came out. An error leaves nothing
    /// encoded, and makes the next frame a key frame.
    pub fn encode(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        format: PixelFormat,
        timestamp_us: i64,
    ) -> Result<Option<EncodedFrame>, CodecError> {
        check_size(width, height)?;
        let expected = width as usize * height as usize * format.bytes_per_pixel();
        if pixels.len() != expected {
            return Err(CodecError::InvalidInput(format!(
                "{} bytes for a {width}x{height} frame, expected {expected}",
                pixels.len()
            )));
        }
        let mut out = reactor_webrtc_sys::ReactorCodecEncodedFrame {
            data: std::ptr::null(),
            len: 0,
            is_key_frame: 0,
            width: 0,
            height: 0,
        };
        let mut err = ErrorBuffer::new();
        let rc = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_encoder_encode(
                self.raw,
                pixels.as_ptr(),
                width as i32,
                height as i32,
                format.raw(),
                self.key_frame_pending as i32,
                &mut out,
                err.ptr(),
                err.cap(),
            )
        };
        match rc {
            1 => {
                // SAFETY: the glue keeps `data` valid until the next call on
                // this encoder, which `&mut self` rules out until we return.
                let data = if out.data.is_null() || out.len == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(out.data, out.len) }.to_vec()
                };
                let key_frame = out.is_key_frame != 0;
                if key_frame {
                    self.key_frame_pending = false;
                }
                Ok(Some(EncodedFrame {
                    data,
                    key_frame,
                    width: out.width.max(0) as u32,
                    height: out.height.max(0) as u32,
                    timestamp_us,
                }))
            }
            0 => Ok(None),
            _ => {
                self.key_frame_pending = true;
                Err(CodecError::Backend(err.message("encode")))
            }
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_video_encoder_destroy(self.raw) }
    }
}

/// The lowest and highest frame rates an encoder takes. The glue turns the
/// frame interval into whole RTP ticks and microseconds; inside this range
/// those conversions fit their integer types. Keep in step with the glue.
const MIN_FRAMERATE: f64 = 0.01;
const MAX_FRAMERATE: f64 = 1000.0;

fn check_framerate(framerate: f64) -> Result<(), CodecError> {
    // Also false for NaN.
    if (MIN_FRAMERATE..=MAX_FRAMERATE).contains(&framerate) {
        Ok(())
    } else {
        Err(CodecError::InvalidInput(format!(
            "framerate {framerate} is outside {MIN_FRAMERATE} to {MAX_FRAMERATE}"
        )))
    }
}

/// The frame sizes every backend takes: positive, even, and within 16 bits.
fn check_size(width: u32, height: u32) -> Result<(), CodecError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(CodecError::InvalidInput(format!(
            "frame size {width}x{height}: both sides must be positive and even"
        )));
    }
    if width > u16::MAX as u32 || height > u16::MAX as u32 {
        return Err(CodecError::InvalidInput(format!(
            "frame size {width}x{height} is too large"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_frame_rates_the_glue_cannot_convert() {
        for ok in [MIN_FRAMERATE, 1.0, 29.97, 30.0, 240.0, MAX_FRAMERATE] {
            assert!(check_framerate(ok).is_ok(), "{ok}");
        }
        for bad in [
            0.0,
            -1.0,
            1e-300,
            f64::MIN_POSITIVE,
            0.009,
            1000.5,
            1e300,
            f64::INFINITY,
            f64::NAN,
        ] {
            assert!(
                matches!(check_framerate(bad), Err(CodecError::InvalidInput(_))),
                "{bad}"
            );
        }
    }
}
