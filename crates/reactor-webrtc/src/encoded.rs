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

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ffi::CStr;
use std::os::raw::c_int;
use std::sync::{Arc, Mutex};

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
        unsafe { CStr::from_ptr(f.mime_type) }
            .to_str()
            .unwrap_or("")
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

// ── Custom video encoder ─────────────────────────────────────────────────────

/// Which video codec was negotiated for the session.
///
/// The value mirrors `webrtc::VideoCodecType` so it round-trips through the
/// FFI as a plain `u32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Vp8 = 1,
    Vp9 = 2,
    Av1 = 3,
    H264 = 4,
    H265 = 5,
}

impl VideoCodec {
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::Vp8),
            2 => Some(Self::Vp9),
            3 => Some(Self::Av1),
            4 => Some(Self::H264),
            5 => Some(Self::H265),
            _ => None,
        }
    }
}

/// Raw I420 video frame delivered to a [`CustomVideoEncoder`] callback.
///
/// Planes are slices into the native frame buffer and are **only valid for the
/// duration of the callback**. Copy the data if your encoder is asynchronous.
pub struct RawVideoFrame<'a> {
    /// Which codec was negotiated — produce a matching bitstream.
    pub codec: VideoCodec,
    /// Luma (Y) plane.
    pub y: &'a [u8],
    pub y_stride: u32,
    /// Chroma (U / Cb) plane.
    pub u: &'a [u8],
    pub u_stride: u32,
    /// Chroma (V / Cr) plane.
    pub v: &'a [u8],
    pub v_stride: u32,
    pub width: u32,
    pub height: u32,
    pub rtp_timestamp: u32,
    /// `true` if the media engine is requesting a key frame (IDR / intra).
    pub request_key_frame: bool,
}

/// An encoded H.264 frame produced by a [`CustomVideoEncoder`] callback.
pub struct EncodedVideoFrame {
    /// Raw H.264 Annex-B or AVCC bitstream bytes.
    pub data: Vec<u8>,
    /// `true` for IDR (key) frames.
    pub is_key_frame: bool,
    /// Width in pixels (0 = inherit from the raw frame).
    pub width: u32,
    /// Height in pixels (0 = inherit from the raw frame).
    pub height: u32,
    /// RTP timestamp (0 = inherit from the raw frame).
    pub rtp_timestamp: u32,
}

type EncodeCallbackBox = Box<dyn FnMut(&RawVideoFrame<'_>) -> Option<EncodedVideoFrame> + Send>;

struct CustomEncoderState {
    cb: Mutex<EncodeCallbackBox>,
}

extern "C" fn encode_tramp(
    ud: *mut c_void,
    raw: *const reactor_webrtc_sys::ReactorRawVideoFrame,
    out: *mut reactor_webrtc_sys::ReactorEncodedVideoOutput,
) -> c_int {
    let Some(r) = (unsafe { raw.as_ref() }) else {
        return 1;
    };
    let st = unsafe { &*(ud as *const CustomEncoderState) };

    let y_len = (r.y_stride.max(0) as usize) * r.height as usize;
    let uv_len = (r.u_stride.max(0) as usize) * (r.height as usize).div_ceil(2);

    let frame = RawVideoFrame {
        codec: VideoCodec::from_u32(r.codec).unwrap_or(VideoCodec::H264),
        y: if r.y.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(r.y, y_len) }
        },
        y_stride: r.y_stride as u32,
        u: if r.u.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(r.u, uv_len) }
        },
        u_stride: r.u_stride as u32,
        v: if r.v.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(r.v, uv_len) }
        },
        v_stride: r.v_stride as u32,
        width: r.width,
        height: r.height,
        rtp_timestamp: r.rtp_timestamp,
        request_key_frame: r.request_key_frame != 0,
    };

    let result = match st.cb.lock() {
        Ok(mut cb) => cb(&frame),
        Err(_) => return 1,
    };

    match result {
        None => 1,
        Some(encoded) => fill_output(encoded, out),
    }
}

/// Called by C++ after `EncodedImageBuffer::Create` has copied the bytes.
extern "C" fn free_encoded_data(data: *const u8, len: usize) {
    // Reconstruct the Vec we leaked in encode_tramp and drop it.
    // SAFETY: this pointer+len was produced by a Vec with capacity==len
    // (we called shrink_to_fit before forgetting it).
    unsafe { drop(Vec::from_raw_parts(data as *mut u8, len, len)) };
}

extern "C" fn free_encoder_state_tramp(ud: *mut c_void) {
    drop(unsafe { Box::from_raw(ud as *mut CustomEncoderState) });
}

// ── Multi-track encoder registry ─────────────────────────────────────────────

/// A pending slot for one video transceiver in an [`EncoderRegistry`].
///
/// - `Custom` — the custom Rust encoder handles this slot; frames are read from
///   the associated queue (push via [`EncodedVideoTrack`]).
/// - `Builtin` — the factory delegates to libwebrtc's builtin VP8/VP9/AV1
///   encoder; push raw BGRA frames via the returned [`Track`](crate::media::Track).
#[derive(Clone)]
pub(crate) enum RegistrySlot {
    Custom(Arc<Mutex<VecDeque<EncodedVideoFrame>>>),
    Builtin,
}

/// Routes encoder instances to per-track slots using the per-encoder-instance ID
/// stamped by the C++ factory.
///
/// Slots are assigned lazily: when a given `encoder_id` appears for the first
/// time (either in `use_builtin_for` or `pop_for`), the next pending slot is
/// consumed and bound to that ID. The assignment order matches the order
/// libwebrtc calls `VideoEncoderFactory::Create()` — one call per video
/// transceiver, in negotiation order — which in turn matches the order
/// [`add_encoded_slot`] / [`add_raw_slot`] were called on the builder.
pub(crate) struct EncoderRegistry {
    pending: Mutex<VecDeque<RegistrySlot>>,
    assigned: Mutex<HashMap<u64, RegistrySlot>>,
}

impl EncoderRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(VecDeque::new()),
            assigned: Mutex::new(HashMap::new()),
        })
    }

    /// Reserve a custom (pre-encoded) slot. Returns the queue the
    /// [`EncodedVideoTrack`] will push frames into.
    pub(crate) fn add_encoded_slot(&self) -> Arc<Mutex<VecDeque<EncodedVideoFrame>>> {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        self.pending
            .lock()
            .unwrap()
            .push_back(RegistrySlot::Custom(q.clone()));
        q
    }

    /// Reserve a builtin (raw BGRA) slot. The C++ factory will delegate to
    /// the builtin VP8/VP9/AV1 encoder for this transceiver.
    pub(crate) fn add_raw_slot(&self) {
        self.pending
            .lock()
            .unwrap()
            .push_back(RegistrySlot::Builtin);
    }

    /// Called by `registry_use_builtin_tramp`. Assigns the next pending slot to
    /// `encoder_id` if it has not been seen before, then returns whether the
    /// C++ factory should delegate to the builtin encoder.
    pub(crate) fn use_builtin_for(&self, encoder_id: u64) -> bool {
        let mut assigned = self.assigned.lock().unwrap();
        if let Some(slot) = assigned.get(&encoder_id) {
            return matches!(slot, RegistrySlot::Builtin);
        }
        let slot = self
            .pending
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(RegistrySlot::Builtin);
        let is_builtin = matches!(slot, RegistrySlot::Builtin);
        assigned.insert(encoder_id, slot);
        is_builtin
    }

    /// Called by `registry_encode_tramp`. Pops the next pre-encoded frame for
    /// this encoder instance, assigning its slot on first call if needed.
    fn pop_for(&self, encoder_id: u64) -> Option<EncodedVideoFrame> {
        let mut assigned = self.assigned.lock().unwrap();
        let slot = assigned.entry(encoder_id).or_insert_with(|| {
            self.pending
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(RegistrySlot::Builtin)
        });
        match slot {
            RegistrySlot::Custom(q) => {
                let q = q.clone();
                drop(assigned);
                let frame = q.lock().unwrap().pop_front();
                frame
            }
            _ => None,
        }
    }
}

struct RegistryState {
    registry: Arc<EncoderRegistry>,
}

/// Fills `out` from an `EncodedVideoFrame`. Returns 0 (forward) on success.
///
/// Extracted so both trampolines share the same output-filling logic.
fn fill_output(
    encoded: EncodedVideoFrame,
    out: *mut reactor_webrtc_sys::ReactorEncodedVideoOutput,
) -> c_int {
    let mut v = encoded.data;
    v.shrink_to_fit();
    let ptr = v.as_ptr();
    let len = v.len();
    std::mem::forget(v);
    unsafe {
        let o = &mut *out;
        o.data = ptr;
        o.len = len;
        o.is_key_frame = encoded.is_key_frame as c_int;
        o.width = encoded.width;
        o.height = encoded.height;
        o.rtp_timestamp = encoded.rtp_timestamp;
        o.free_data = Some(free_encoded_data);
    }
    0
}

extern "C" fn registry_encode_tramp(
    ud: *mut c_void,
    raw: *const reactor_webrtc_sys::ReactorRawVideoFrame,
    out: *mut reactor_webrtc_sys::ReactorEncodedVideoOutput,
) -> c_int {
    let Some(r) = (unsafe { raw.as_ref() }) else {
        return 1;
    };
    let st = unsafe { &*(ud as *const RegistryState) };
    match st.registry.pop_for(r.encoder_id) {
        None => 1,
        Some(encoded) => fill_output(encoded, out),
    }
}

/// Called by C++ before creating each encoder instance. Returns 1 if the
/// builtin VP8/VP9/AV1 encoder should be used for this slot, 0 for custom.
extern "C" fn registry_use_builtin_tramp(ud: *mut c_void, encoder_id: u64) -> c_int {
    let st = unsafe { &*(ud as *const RegistryState) };
    st.registry.use_builtin_for(encoder_id) as c_int
}

extern "C" fn free_registry_state_tramp(ud: *mut c_void) {
    drop(unsafe { Box::from_raw(ud as *mut RegistryState) });
}

/// A factory-level custom video encoder. Pass to
/// [`PeerConnectionFactory::with_custom_video_encoder`](crate::PeerConnectionFactory::with_custom_video_encoder).
///
/// The closure is called **synchronously** on the WebRTC encoder thread for every
/// raw I420 frame. Return `Some(encoded)` to inject H.264 bytes into the RTP
/// stack, or `None` to drop the frame.
///
/// For asynchronous hardware encoders (VideoToolbox, GStreamer, etc.), copy the
/// I420 planes into your pipeline and block until output is ready. The closure
/// must be `Send` because it is called from a WebRTC-internal thread.
pub struct CustomVideoEncoder {
    pub(crate) encode_fn: extern "C" fn(
        *mut c_void,
        *const reactor_webrtc_sys::ReactorRawVideoFrame,
        *mut reactor_webrtc_sys::ReactorEncodedVideoOutput,
    ) -> c_int,
    pub(crate) userdata: *mut c_void,
    pub(crate) free_ud: Option<extern "C" fn(*mut c_void)>,
    /// Optional: called by the C++ factory before creating each encoder instance.
    /// Non-null only when the registry contains a mix of custom and builtin slots.
    pub(crate) use_builtin: Option<extern "C" fn(*mut c_void, u64) -> c_int>,
}

// SAFETY: the callback is Mutex-guarded; userdata is a heap-pinned Box that
// lives until the native factory calls free_ud.
unsafe impl Send for CustomVideoEncoder {}
unsafe impl Sync for CustomVideoEncoder {}

impl CustomVideoEncoder {
    /// Create a custom encoder that calls `cb` for every frame to be encoded.
    pub fn new(
        cb: impl FnMut(&RawVideoFrame<'_>) -> Option<EncodedVideoFrame> + Send + 'static,
    ) -> Self {
        // Leak the state: the factory holds it and frees via free_encoder_state_tramp.
        let state = Box::into_raw(Box::new(CustomEncoderState {
            cb: Mutex::new(Box::new(cb)),
        }));
        Self {
            encode_fn: encode_tramp,
            userdata: state as *mut c_void,
            free_ud: Some(free_encoder_state_tramp),
            use_builtin: None,
        }
    }

    /// Create a custom encoder driven by an [`EncodedVideoTrack`] queue.
    ///
    /// Internal — called by [`PeerConnectionFactory::with_encoded_video_track`].
    pub(crate) fn from_queue(queue: Arc<Mutex<VecDeque<EncodedVideoFrame>>>) -> Self {
        Self::new(move |_raw| queue.lock().unwrap().pop_front())
    }

    /// Create a custom encoder backed by a shared [`EncoderRegistry`].
    ///
    /// Internal — used by [`EncodedVideoBuilder`](crate::EncodedVideoBuilder)
    /// to route frames across multiple encoded and/or raw video tracks.
    /// The `use_builtin` trampoline is passed to the C++ factory so it can
    /// delegate individual encoder instances to the builtin VP8/VP9/AV1 pipeline.
    pub(crate) fn from_registry(registry: Arc<EncoderRegistry>) -> Self {
        let state = Box::into_raw(Box::new(RegistryState { registry }));
        Self {
            encode_fn: registry_encode_tramp,
            userdata: state as *mut c_void,
            free_ud: Some(free_registry_state_tramp),
            use_builtin: Some(registry_use_builtin_tramp),
        }
    }
}

// ── Push-based encoded video track ───────────────────────────────────────────

/// A video track that accepts **pre-encoded** frames directly, bypassing the
/// libwebrtc software encoder pipeline entirely.
///
/// Obtain one via [`PeerConnectionFactory::with_encoded_video_track`], then:
///
/// 1. Attach `EncodedVideoTrack::track()` to a send-only transceiver.
/// 2. Call `push_encoded_frame` whenever your encoder (VideoToolbox, NVENC,
///    GStreamer, libvpx, …) produces a frame — at your rate, on any thread.
///
/// # Timing
///
/// The WebRTC encoder thread is triggered internally each time you call
/// `push_encoded_frame`, so you do **not** need to call
/// `push_video_frame` separately. The dummy raw frame used to trigger it
/// is cheap (pre-allocated; the I420 data is discarded by the encoder
/// callback before it ever touches your encoded bytes).
pub struct EncodedVideoTrack {
    pub(crate) track: crate::media::Track,
    pub(crate) queue: Arc<Mutex<VecDeque<EncodedVideoFrame>>>,
    // Pre-allocated BGRA buffer used to trigger the WebRTC encoder thread.
    // The dimensions are kept in sync with the track's configured resolution
    // so the pipeline doesn't reject the frame due to a size mismatch.
    dummy: Vec<u8>,
    width: u32,
    height: u32,
}

// SAFETY: the queue is Mutex-guarded and the dummy buffer is owned; both are
// safe to move across threads.
unsafe impl Send for EncodedVideoTrack {}
unsafe impl Sync for EncodedVideoTrack {}

impl EncodedVideoTrack {
    pub(crate) fn new(
        track: crate::media::Track,
        queue: Arc<Mutex<VecDeque<EncodedVideoFrame>>>,
        width: u32,
        height: u32,
    ) -> Self {
        let dummy = vec![0u8; (width * height * 4) as usize];
        Self {
            track,
            queue,
            dummy,
            width,
            height,
        }
    }

    /// The underlying video [`Track`](crate::Track). Pass this to
    /// [`Transceiver::set_track`](crate::Transceiver::set_track) after creating
    /// the send-only transceiver.
    pub fn track(&self) -> &crate::media::Track {
        &self.track
    }

    /// Inject a pre-encoded frame into the WebRTC RTP stack.
    ///
    /// The call returns immediately; the frame is queued and forwarded to the
    /// RTP packetizer on the WebRTC encoder thread. Thread-safe — call from
    /// any thread, including a hardware encoder callback.
    ///
    /// Set `frame.width` / `frame.height` to 0 to inherit from the track's
    /// configured resolution (the value passed to
    /// [`with_encoded_video_track`](crate::PeerConnectionFactory::with_encoded_video_track)).
    pub fn push_encoded_frame(&self, frame: EncodedVideoFrame) {
        // Queue first so the frame is always present when the encoder thread
        // dequeues it (the two operations are not atomic, but the encoder
        // thread is asynchronous, so the queue push always wins the race).
        self.queue.lock().unwrap().push_back(frame);
        // Push a dummy raw frame to wake the WebRTC encoder thread.
        // The I420 data is thrown away in the encoder callback — the actual
        // encoded bytes come from the queue above.
        self.track
            .push_video_frame(&self.dummy, self.width, self.height);
    }
}
