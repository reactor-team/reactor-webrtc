//! [`VideoDecoder`]: encoded frames in decode order, raw pictures out.

use std::os::raw::c_char;
use std::sync::Arc;

use super::{
    c_name, check_codec, is_key_frame, Backend, CodecError, ContextHandle, ErrorBuffer, VideoCodec,
};
use crate::PixelFormat;

/// One decoded picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// `width * height * format.bytes_per_pixel()` bytes, rows without
    /// padding.
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// The timestamp the encoded frame was given, unchanged.
    pub timestamp_us: i64,
}

/// A video decoder that runs outside a peer connection. It keeps its
/// reference frames between calls, so it takes one stream's frames in the
/// order they were encoded. See the [module docs](super) for the key frame
/// and threading contract.
pub struct VideoDecoder {
    raw: *mut reactor_webrtc_sys::StandaloneVideoDecoder,
    /// Keeps the backend (and a loaded OpenH264) alive.
    _ctx: Arc<ContextHandle>,
    codec: VideoCodec,
    implementation: String,
    hardware: bool,
    /// Only a key frame may come next: set at creation, by
    /// [`reset`](Self::reset) and by a failed call, and cleared by a key
    /// frame that decodes.
    needs_key_frame: bool,
}

// SAFETY: the native decoder is used by one thread at a time (`&mut self`);
// the glue synchronises with the backend's own callback threads.
unsafe impl Send for VideoDecoder {}

impl VideoDecoder {
    pub(super) fn create(
        ctx: Arc<ContextHandle>,
        codec: VideoCodec,
        backend: Backend,
    ) -> Result<Self, CodecError> {
        check_codec(codec)?;
        let mut err = ErrorBuffer::new();
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_decoder_create(
                ctx.0,
                codec as i32,
                backend.raw(),
                err.ptr(),
                err.cap(),
            )
        };
        if raw.is_null() {
            return Err(CodecError::Unsupported(err.message("decoder creation")));
        }
        let mut name = [0 as c_char; 64];
        let hardware = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_decoder_info(
                raw,
                name.as_mut_ptr(),
                name.len() as i32,
            )
        } != 0;
        Ok(Self {
            raw,
            _ctx: ctx,
            codec,
            implementation: c_name(&name),
            hardware,
            needs_key_frame: true,
        })
    }

    /// The codec this decoder reads.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The backend this decoder runs on, e.g. `"VideoToolbox"`.
    pub fn implementation(&self) -> &str {
        &self.implementation
    }

    /// Whether that backend is the platform's hardware codec.
    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Whether the next frame must be a key frame.
    pub fn needs_key_frame(&self) -> bool {
        self.needs_key_frame
    }

    /// Forget every frame decoded so far, as when a stream starts over: the
    /// next frame must be a key frame. On error the decoder is unusable
    /// until a later `reset` succeeds.
    pub fn reset(&mut self) -> Result<(), CodecError> {
        self.needs_key_frame = true;
        let ok = unsafe { reactor_webrtc_sys::reactor_webrtc_video_decoder_reset(self.raw) } != 0;
        if ok {
            Ok(())
        } else {
            Err(CodecError::Backend(
                "the decoder could not be configured again".into(),
            ))
        }
    }

    /// Decode one encoded frame and return its picture in `format`, carrying
    /// `timestamp_us` back. Blocks until the backend is done with it.
    ///
    /// Refuses anything but a key frame while
    /// [`needs_key_frame`](Self::needs_key_frame), with
    /// [`CodecError::NeedKeyFrame`]. `Ok(None)` means the frame gave no
    /// picture, which a whole frame from [`VideoEncoder`](super::VideoEncoder)
    /// does not. An error makes the decoder wait for a key frame.
    pub fn decode(
        &mut self,
        data: &[u8],
        timestamp_us: i64,
        format: PixelFormat,
    ) -> Result<Option<DecodedFrame>, CodecError> {
        if data.is_empty() {
            return Err(CodecError::InvalidInput("empty frame".into()));
        }
        let key_frame = is_key_frame(self.codec, data);
        if self.needs_key_frame && !key_frame {
            return Err(CodecError::NeedKeyFrame);
        }
        let mut out = reactor_webrtc_sys::ReactorCodecDecodedFrame {
            pixels: std::ptr::null(),
            len: 0,
            width: 0,
            height: 0,
        };
        let mut err = ErrorBuffer::new();
        let rc = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_decoder_decode(
                self.raw,
                data.as_ptr(),
                data.len(),
                key_frame as i32,
                format.raw(),
                &mut out,
                err.ptr(),
                err.cap(),
            )
        };
        match rc {
            1 => {
                if key_frame {
                    self.needs_key_frame = false;
                }
                let width = out.width.max(0) as u32;
                let height = out.height.max(0) as u32;
                let expected = width as usize * height as usize * format.bytes_per_pixel();
                if out.pixels.is_null() || out.len != expected {
                    self.needs_key_frame = true;
                    return Err(CodecError::Backend(format!(
                        "decoded {} bytes for a {width}x{height} picture",
                        out.len
                    )));
                }
                // SAFETY: the glue keeps `pixels` valid until the next call on
                // this decoder, which `&mut self` rules out until we return.
                let pixels = unsafe { std::slice::from_raw_parts(out.pixels, out.len) }.to_vec();
                Ok(Some(DecodedFrame {
                    data: pixels,
                    width,
                    height,
                    format,
                    timestamp_us,
                }))
            }
            0 => {
                if key_frame {
                    self.needs_key_frame = false;
                }
                Ok(None)
            }
            _ => {
                self.needs_key_frame = true;
                Err(CodecError::Backend(err.message("decode")))
            }
        }
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_video_decoder_destroy(self.raw) }
    }
}
