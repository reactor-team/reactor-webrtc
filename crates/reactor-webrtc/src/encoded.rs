//! Encoded-frame transform — bypass the codec to forward/receive **encoded**
//! media (Insertable Streams / Encoded Transform).
//!
//! A [`FrameTransform`] attached to a transceiver's sender or receiver
//! ([`Transceiver::set_sender_transform`](crate::Transceiver::set_sender_transform)
//! / [`set_receiver_transform`](crate::Transceiver::set_receiver_transform))
//! runs a closure per **encoded** frame:
//!
//! - **Sender** (encoder → packetizer): observe the encoded payload before it's
//!   packetized, and optionally [`replace it`](EncodedFrame::replace_data) with
//!   your own encoded bytes (forwarding). Returning [`FrameAction::Drop`] sends
//!   nothing.
//! - **Receiver** (depacketizer → decoder): observe the encoded payload before
//!   it's decoded. Returning [`FrameAction::Drop`] **bypasses the decoder**
//!   entirely (right for a forwarding server that never renders locally);
//!   [`FrameAction::Forward`] lets the local decoder run as usual.
//!
//! The closure runs on a WebRTC thread and must not block it.

use std::ffi::CStr;
use std::os::raw::c_int;
use std::ffi::c_void;
use std::sync::Mutex;

use crate::media::MediaKind;

/// Which side of the pipeline an [`EncodedFrame`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDirection {
    /// Egress: after the encoder, before packetization.
    Send,
    /// Ingress: after depacketization, before the decoder.
    Receive,
}

/// What to do with a frame after the callback returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    /// Emit the frame downstream (send it / hand it to the decoder), including
    /// any [`EncodedFrame::replace_data`] applied.
    Forward,
    /// Drop the frame: on receive this bypasses the decoder; on send nothing is
    /// transmitted.
    Drop,
}

/// A borrowed encoded media frame. `data` and `mime_type` are valid only for
/// the duration of the callback — copy them to retain.
pub struct EncodedFrame<'a> {
    pub direction: FrameDirection,
    pub kind: MediaKind,
    /// Video only: whether this is a key frame (always `false` for audio).
    pub is_key_frame: bool,
    pub payload_type: u8,
    pub ssrc: u32,
    /// The frame's RTP timestamp.
    pub timestamp: u32,
    /// e.g. `"video/VP8"`, `"audio/opus"`.
    pub mime_type: &'a str,
    /// The encoded payload.
    pub data: &'a [u8],
    // Opaque native frame handle, for replace_data.
    frame: *mut c_void,
}

impl EncodedFrame<'_> {
    /// Replace this frame's encoded payload (copied). Combine with
    /// [`FrameAction::Forward`] to send/forward your own encoded bytes in place
    /// of the original. No effect if `FrameAction::Drop` is returned.
    pub fn replace_data(&self, data: &[u8]) {
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_encoded_frame_set_data(
                self.frame,
                data.as_ptr(),
                data.len(),
            );
        }
    }
}

type EncodedCb = Box<dyn for<'a> FnMut(&EncodedFrame<'a>) -> FrameAction + Send>;

// Heap-pinned callback state; owned by the *native* transformer (freed via
// `free_state_tramp` when its last ref drops), so it outlives every callback
// even if the `FrameTransform` handle is dropped while still attached.
struct TransformState {
    cb: Mutex<EncodedCb>,
}

extern "C" fn encoded_tramp(
    ud: *mut c_void,
    frame: *const reactor_webrtc_sys::ReactorEncodedFrame,
) -> c_int {
    // Default to Forward (0) on any error so we never silently break media.
    let f = match unsafe { frame.as_ref() } {
        Some(f) => f,
        None => return 0,
    };
    let st = unsafe { &*(ud as *const TransformState) };
    let data = if f.data.is_null() || f.data_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(f.data, f.data_len) }
    };
    let mime = if f.mime_type.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(f.mime_type) }.to_str().unwrap_or("")
    };
    let ef = EncodedFrame {
        direction: if f.direction == 1 {
            FrameDirection::Receive
        } else {
            FrameDirection::Send
        },
        kind: if f.is_audio == 1 {
            MediaKind::Audio
        } else {
            MediaKind::Video
        },
        is_key_frame: f.is_key_frame == 1,
        payload_type: f.payload_type,
        ssrc: f.ssrc,
        timestamp: f.timestamp,
        mime_type: mime,
        data,
        frame: f.frame,
    };
    let action = match st.cb.lock() {
        Ok(mut cb) => cb(&ef),
        Err(_) => FrameAction::Forward,
    };
    match action {
        FrameAction::Forward => 0,
        FrameAction::Drop => 1,
    }
}

extern "C" fn free_state_tramp(ud: *mut c_void) {
    // Reclaim the Box leaked in `FrameTransform::new`.
    drop(unsafe { Box::from_raw(ud as *mut TransformState) });
}

/// An encoded-frame transformer. Attach it to a transceiver's sender/receiver;
/// see the [module docs](self). Dropping the handle releases the binding's
/// reference — the native transformer (and the callback) live until every
/// sender/receiver it's attached to also releases it.
pub struct FrameTransform {
    raw: *mut reactor_webrtc_sys::FrameTransformer,
}

// SAFETY: the callback is Mutex-guarded and the native transformer is
// internally thread-safe; the handle only owns a ref-counted pointer.
unsafe impl Send for FrameTransform {}
unsafe impl Sync for FrameTransform {}

impl FrameTransform {
    /// Create a transformer running `cb` per encoded frame. The closure runs on
    /// a WebRTC thread and must not block it.
    pub fn new(cb: impl for<'a> FnMut(&EncodedFrame<'a>) -> FrameAction + Send + 'static) -> Self {
        // Leak the state; the native transformer owns it and frees it via
        // free_state_tramp when its last ref drops.
        let state = Box::into_raw(Box::new(TransformState {
            cb: Mutex::new(Box::new(cb)),
        }));
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_frame_transformer_create(
                encoded_tramp,
                state as *mut c_void,
                free_state_tramp,
            )
        };
        if raw.is_null() {
            // Creation failed: reclaim the leaked state so we don't leak it.
            drop(unsafe { Box::from_raw(state) });
        }
        Self { raw }
    }

    pub(crate) fn raw(&self) -> *mut reactor_webrtc_sys::FrameTransformer {
        self.raw
    }
}

impl Drop for FrameTransform {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { reactor_webrtc_sys::reactor_webrtc_frame_transformer_destroy(self.raw) }
        }
    }
}
