//! [`VideoDecoder`]: encoded frames in decode order, raw pictures out.

use std::os::raw::{c_char, c_void};
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

    /// The backend this decoder runs on, e.g. `"VideoToolbox"` or `"dav1d"`.
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
        let mut pixels = Vec::new();
        let size = self.decode_into(data, format, |width, height| {
            // Zeroed rather than uninitialised: a picture this size comes
            // from the allocator as fresh zero pages, with no fill to pay.
            pixels = vec![0; width as usize * height as usize * format.bytes_per_pixel()];
            Some(&mut pixels[..])
        })?;
        Ok(size.map(|(width, height)| DecodedFrame {
            data: pixels,
            width,
            height,
            format,
            timestamp_us,
        }))
    }

    /// Decode one encoded frame and write its picture in `format` to the
    /// buffer `buffer` returns for its size, converted once, in place, with
    /// no copy behind it. Blocks until the backend is done with the frame.
    ///
    /// `buffer(width, height)` runs at most once, during the call, on the
    /// calling thread, once the picture's size is known. It returns
    /// `width * height * format.bytes_per_pixel()` bytes for rows without
    /// padding, or `None` to give up. `Ok(Some((width, height)))` once the
    /// buffer holds the picture; `Ok(None)` when the frame gave no picture,
    /// and `buffer` is not called.
    ///
    /// This is [`decode`](Self::decode) without its `Vec`: a binding hands
    /// over memory it already owns (a numpy array, a pooled buffer), and a
    /// caller that keeps one buffer reuses it across frames. Every backend
    /// takes it, hardware or software. The key frame contract is
    /// [`decode`](Self::decode)'s; a buffer of the wrong size, or none, fails
    /// the call with [`CodecError::InvalidInput`] after the frame has been
    /// decoded, so the decoder then waits for a key frame. A panic in
    /// `buffer` resumes once the backend is done with the frame.
    pub fn decode_into<'b>(
        &mut self,
        data: &[u8],
        format: PixelFormat,
        buffer: impl FnOnce(u32, u32) -> Option<&'b mut [u8]>,
    ) -> Result<Option<(u32, u32)>, CodecError> {
        if data.is_empty() {
            return Err(CodecError::InvalidInput("empty frame".into()));
        }
        let key_frame = is_key_frame(self.codec, data);
        if self.needs_key_frame && !key_frame {
            return Err(CodecError::NeedKeyFrame);
        }
        // The buffer as a raw pointer and length: the glue writes to it during
        // the call, inside the borrow `'b` the caller gave.
        let mut state = BufferRequest {
            buffer: Some(move |width, height| {
                buffer(width, height).map(|b: &'b mut [u8]| (b.as_mut_ptr(), b.len()))
            }),
            refused: None,
            panic: None,
        };
        let (mut width, mut height) = (0i32, 0i32);
        let mut err = ErrorBuffer::new();
        let rc = unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_decoder_decode_into(
                self.raw,
                data.as_ptr(),
                data.len(),
                key_frame as i32,
                format.raw(),
                provider_for(&state),
                &mut state as *mut BufferRequest<_> as *mut c_void,
                &mut width,
                &mut height,
                err.ptr(),
                err.cap(),
            )
        };
        if let Some(panic) = state.panic {
            self.needs_key_frame = true;
            std::panic::resume_unwind(panic);
        }
        match rc {
            1 => {
                if key_frame {
                    self.needs_key_frame = false;
                }
                Ok(Some((width.max(0) as u32, height.max(0) as u32)))
            }
            0 => {
                if key_frame {
                    self.needs_key_frame = false;
                }
                Ok(None)
            }
            _ => {
                self.needs_key_frame = true;
                Err(match state.refused {
                    Some(why) => CodecError::InvalidInput(why),
                    None => CodecError::Backend(err.message("decode")),
                })
            }
        }
    }
}

/// What [`VideoDecoder::decode_into`] lends [`provide_buffer`]: the caller's
/// buffer function, and why it gave no buffer, or how it panicked.
struct BufferRequest<F> {
    buffer: Option<F>,
    refused: Option<String>,
    panic: Option<Box<dyn std::any::Any + Send>>,
}

/// [`provide_buffer`] for `request`'s function type, which has no name to
/// spell.
fn provider_for<F>(
    _request: &BufferRequest<F>,
) -> unsafe extern "C" fn(*mut c_void, i32, i32, usize) -> *mut u8
where
    F: FnOnce(u32, u32) -> Option<(*mut u8, usize)>,
{
    provide_buffer::<F>
}

/// The glue's `alloc`: the buffer `request`'s function gives for a `width` x
/// `height` picture of `bytes`, or null when it gives none, one of another
/// size, or panics. A panic is kept for the caller to resume, so none unwinds
/// through the glue.
unsafe extern "C" fn provide_buffer<F>(
    request: *mut c_void,
    width: i32,
    height: i32,
    bytes: usize,
) -> *mut u8
where
    F: FnOnce(u32, u32) -> Option<(*mut u8, usize)>,
{
    // SAFETY: `decode_into` passes its own `BufferRequest<F>`, alive for the
    // call this runs in.
    let request = unsafe { &mut *(request as *mut BufferRequest<F>) };
    let Some(buffer) = request.buffer.take() else {
        return std::ptr::null_mut();
    };
    let (width, height) = (width.max(0) as u32, height.max(0) as u32);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| buffer(width, height))) {
        Ok(Some((ptr, len))) if len == bytes && !ptr.is_null() => ptr,
        Ok(Some((_, len))) => {
            request.refused = Some(format!(
                "a buffer of {len} bytes for a {width}x{height} picture, which takes {bytes}"
            ));
            std::ptr::null_mut()
        }
        Ok(None) => {
            request.refused = Some(format!("no buffer for the {width}x{height} picture"));
            std::ptr::null_mut()
        }
        Err(panic) => {
            request.panic = Some(panic);
            std::ptr::null_mut()
        }
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_video_decoder_destroy(self.raw) }
    }
}
