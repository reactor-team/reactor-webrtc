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
//!
//! libwebrtc allows one transformer per sender and one per receiver, so the crate
//! owns those slots and composes: your callback runs alongside the per-frame
//! metadata step ([`crate::metadata`]) rather than displacing it. Your callback goes
//! first in both directions, which means it always sees exactly the bytes that
//! traverse the network — before a trailer is appended on send, before one is
//! stripped on receive. Returning [`FrameAction::Drop`] skips the metadata step too.

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
    /// Capture timestamp in milliseconds (monotonic, same epoch as
    /// `webrtc::TimeMicros`). Zero when unavailable.
    pub capture_time_ms: i64,
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

pub(crate) type EncodedCb = Box<dyn for<'a> FnMut(&EncodedFrame<'a>) -> FrameAction + Send>;

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
        capture_time_ms: f.capture_time_ms,
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

/// A callback over encoded frames, to be attached to a transceiver's
/// sender/receiver; see the [module docs](self).
///
/// This is a *registration*, not a native object. Attaching it does not take
/// libwebrtc's single `SetFrameTransformer` slot — the crate owns one transformer
/// per sender and per receiver, and composes this callback with its own
/// frame-metadata step (see [`crate::metadata`]). That is what lets a caller use
/// encoded-frame access and per-frame metadata on the same transceiver, which a
/// single slot cannot express.
///
/// Attaching the same `FrameTransform` to more than one sender/receiver shares one
/// callback, serialised by its mutex.
#[derive(Clone)]
pub struct FrameTransform {
    cb: Arc<Mutex<EncodedCb>>,
}

impl FrameTransform {
    /// Create a transform running `cb` per encoded frame. The closure runs on a
    /// WebRTC thread and must not block it.
    pub fn new(cb: impl for<'a> FnMut(&EncodedFrame<'a>) -> FrameAction + Send + 'static) -> Self {
        Self {
            cb: Arc::new(Mutex::new(Box::new(cb))),
        }
    }

    pub(crate) fn callback(&self) -> Arc<Mutex<EncodedCb>> {
        Arc::clone(&self.cb)
    }
}

/// The native transformer: one per sender/receiver, owned by the crate.
///
/// The composed root that [`FrameTransform`] callbacks and the frame-metadata step
/// both run under. Dropping this handle releases the binding's reference; the
/// native object and its callback state live until every sender/receiver it is
/// attached to also releases it, which is why the install path can attach and drop.
pub(crate) struct NativeTransform {
    raw: *mut reactor_webrtc_sys::FrameTransformer,
}

// SAFETY: the callback is Mutex-guarded and the native transformer is
// internally thread-safe; the handle only owns a ref-counted pointer.
unsafe impl Send for NativeTransform {}
unsafe impl Sync for NativeTransform {}

impl NativeTransform {
    pub(crate) fn new(
        cb: impl for<'a> FnMut(&EncodedFrame<'a>) -> FrameAction + Send + 'static,
    ) -> Self {
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

impl Drop for NativeTransform {
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

    /// The name libwebrtc's codec capabilities carry for this codec (e.g.
    /// `RtpCodecCapability::name`) — what
    /// [`Transceiver::set_codec_preferences`](crate::Transceiver::set_codec_preferences)
    /// matches against, as a plain string rather than a wire-format integer
    /// that both sides of the FFI boundary have to keep in sync by hand.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Vp8 => "VP8",
            Self::Vp9 => "VP9",
            Self::Av1 => "AV1",
            Self::H264 => "H264",
            Self::H265 => "H265",
        }
    }
}

/// Raw I420 video frame delivered to an inline encoder callback
/// ([`TrackVideoEncoder::Inline`]).
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

/// An encoded video frame, produced by an inline encoder callback or pushed
/// to an [`EncodedVideoTrack`].
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

/// The boxed encoder callback a [`TrackVideoEncoder::Inline`] track hands
/// the factory — called synchronously with every raw I420 frame on the
/// encoder thread; return `Some(encoded)` to forward, `None` to drop.
pub type InlineEncoderCallback =
    Box<dyn FnMut(&RawVideoFrame<'_>) -> Option<EncodedVideoFrame> + Send + 'static>;

pub(crate) struct CustomEncoderState {
    cb: Mutex<InlineEncoderCallback>,
}

/// Borrowed view of a glue raw frame, for the inline-encoder callback.
/// Planes are valid only for the duration of the native Encode() call.
fn raw_view(r: &reactor_webrtc_sys::ReactorRawVideoFrame) -> RawVideoFrame<'_> {
    let y_len = (r.y_stride.max(0) as usize) * r.height as usize;
    let uv_len = (r.u_stride.max(0) as usize) * (r.height as usize).div_ceil(2);

    RawVideoFrame {
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
    }
}

/// Called by C++ after `EncodedImageBuffer::Create` has copied the bytes.
extern "C" fn free_encoded_data(data: *const u8, len: usize) {
    // Reconstruct the Vec we leaked in encode_tramp and drop it.
    // SAFETY: this pointer+len was produced by a Vec with capacity==len
    // (we called shrink_to_fit before forgetting it).
    unsafe { drop(Vec::from_raw_parts(data as *mut u8, len, len)) };
}

// ── Multi-track encoder registry ─────────────────────────────────────────────

/// A pending slot for one video transceiver in an [`EncoderRegistry`].
///
/// - `Custom` — frames are read from the associated queue (push via
///   [`EncodedVideoTrack`]); the queue drain itself is the "encoder".
/// - `Inline` — the registry calls the user callback synchronously with every
///   raw I420 frame ([`TrackVideoEncoder::Inline`] tracks).
/// - `Builtin` — the factory delegates to a backend encoder (builtin
///   VP8/VP9/AV1, or H264 via the slot's backend preference); push raw BGRA
///   frames via the returned [`Track`](crate::media::Track).
pub(crate) enum RegistrySlot {
    Custom(Arc<Mutex<VecDeque<EncodedVideoFrame>>>),
    Inline(Arc<CustomEncoderState>),
    Builtin(H264BackendPref),
}

impl Clone for RegistrySlot {
    fn clone(&self) -> Self {
        match self {
            Self::Custom(q) => Self::Custom(q.clone()),
            Self::Inline(s) => Self::Inline(s.clone()),
            Self::Builtin(pref) => Self::Builtin(*pref),
        }
    }
}

/// Which H.264 backend a builtin-routed slot prefers, mirroring the public
/// [`H264Backend`] (None → `Auto`). `Auto` resolves at encoder creation:
/// VideoToolbox on Apple, registered OpenH264 elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum H264BackendPref {
    Auto,
    VideoToolbox,
    #[cfg(feature = "openh264")]
    OpenH264,
}

impl H264BackendPref {
    /// The `reactor_video_backend_cb` wire value (0 = auto, 1 = VT, 2 = OH).
    fn as_c_int(self) -> c_int {
        match self {
            Self::Auto => 0,
            Self::VideoToolbox => 1,
            #[cfg(feature = "openh264")]
            Self::OpenH264 => 2,
        }
    }
}

/// Routes encoder instances to per-track slots using the per-encoder-instance ID
/// stamped by the C++ factory.
///
/// Slots are assigned lazily: when a given `encoder_id` appears for the first
/// time (either in `use_builtin_for` or `encode_for`), the next pending slot is
/// consumed and bound to that ID. The assignment order matches the order
/// libwebrtc calls `VideoEncoderFactory::Create()` — one call per video
/// transceiver, in negotiation order — which in turn matches the order
/// [`add_encoded_slot`] / [`add_raw_slot`] were called on the builder.
///
/// Every reservation carries the owning track's native id so the registry
/// can retract a dead track's slots ([`EncoderRegistry::retract`]): an
/// unreserved-pending slot for a dead track gets dropped, and an assigned
/// slot degrades to Builtin with a loud fallback, never silently routing
/// another track's frames.
pub(crate) struct EncoderRegistry {
    pending: Mutex<VecDeque<RegistryReservation>>,
    assigned: Mutex<HashMap<u64, AssignedSlot>>,
}

struct RegistryReservation {
    native_id: usize,
    slot: RegistrySlot,
}

struct AssignedSlot {
    native_id: usize,
    slot: RegistrySlot,
}

impl EncoderRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(VecDeque::new()),
            assigned: Mutex::new(HashMap::new()),
        })
    }

    /// Wire this registry into factory-create options: leaks a
    /// [`RegistryState`] the native side frees via `free_registry_state_tramp`
    /// when the last encoder goes away (or the caller frees manually after a
    /// failed create), and points every encode/`use_builtin`/`has_custom`
    /// trampoline at it.
    pub(crate) fn install_into(
        self: &Arc<Self>,
        opts: &mut reactor_webrtc_sys::ReactorFactoryOptions,
    ) {
        let state = Box::into_raw(Box::new(RegistryState {
            registry: Arc::clone(self),
        }));
        opts.encode_cb = Some(registry_encode_tramp);
        opts.encode_userdata = state as *mut c_void;
        opts.encode_free_ud = Some(free_registry_state_tramp);
        opts.encode_use_builtin = Some(registry_use_builtin_tramp);
        opts.encode_has_custom_slots = Some(registry_has_custom_tramp);
        opts.encode_video_backend_for = Some(registry_backend_for_tramp);
    }

    /// Reserve a custom (pre-encoded) slot for the track `native_id`.
    /// Returns the queue the [`EncodedVideoTrack`] will push frames into.
    pub(crate) fn add_encoded_slot(
        &self,
        native_id: usize,
    ) -> Arc<Mutex<VecDeque<EncodedVideoFrame>>> {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        self.pending.lock().unwrap().push_back(RegistryReservation {
            native_id,
            slot: RegistrySlot::Custom(q.clone()),
        });
        q
    }

    /// Reserve a builtin (raw BGRA) slot for the track `native_id`. The C++
    /// factory delegates to the builtin VP8/VP9/AV1 encoder for this
    /// transceiver.
    pub(crate) fn add_raw_slot(&self, native_id: usize) {
        self.add_raw_slot_with_backend(native_id, H264BackendPref::Auto);
    }

    /// Reserve a builtin (raw BGRA) slot with an explicit H264 backend
    /// preference, for tracks created with `h264_backend` set.
    pub(crate) fn add_raw_slot_with_backend(&self, native_id: usize, backend: H264BackendPref) {
        self.pending.lock().unwrap().push_back(RegistryReservation {
            native_id,
            slot: RegistrySlot::Builtin(backend),
        });
    }

    /// Reserve an inline-encoder slot for the track `native_id`. The
    /// registry calls `cb` synchronously with every raw I420 frame for the
    /// encoder instance that binds here.
    pub(crate) fn add_inline_slot(&self, native_id: usize, cb: InlineEncoderCallback) {
        let state = Arc::new(CustomEncoderState { cb: Mutex::new(cb) });
        self.pending.lock().unwrap().push_back(RegistryReservation {
            native_id,
            slot: RegistrySlot::Inline(state),
        });
    }

    /// Drop everything reserved for a track that no longer exists.
    /// Pending reservations are simply removed; an already-assigned slot is
    /// degraded to Builtin with a loud fallback — libwebrtc may legitimately
    /// recreate an encoder for a dead stream, and that must never consume
    /// another track's slot (the old grey-silence failure).
    pub(crate) fn retract(&self, native_id: usize) {
        self.pending
            .lock()
            .unwrap()
            .retain(|r| r.native_id != native_id);
        let mut assigned = self.assigned.lock().unwrap();
        for a in assigned.values_mut() {
            if a.native_id == native_id && !matches!(a.slot, RegistrySlot::Builtin(_)) {
                eprintln!(
                    "[reactor-webrtc] retracting encoder slot for dropped track {native_id:#x}: \
                     degrading to builtin (loud fallback — never reroutes another track's slot)"
                );
                a.slot = RegistrySlot::Builtin(H264BackendPref::Auto);
            }
        }
    }

    /// Whether any custom (pre-encoded or inline) slot exists — pending or
    /// already assigned. Drives the `has_custom_slots` predicate the native
    /// factory consults when advertising codecs.
    pub(crate) fn has_custom(&self) -> bool {
        let is_custom = |s: &RegistrySlot| !matches!(s, RegistrySlot::Builtin(_));
        // Drop pending's guard before touching `assigned`: holding both is an
        // ABBA deadlock against use_builtin_for/encode_for under concurrent
        // negotiation + encode.
        let pending_has = self
            .pending
            .lock()
            .unwrap()
            .iter()
            .any(|r| is_custom(&r.slot));
        pending_has
            || self
                .assigned
                .lock()
                .unwrap()
                .values()
                .any(|a| is_custom(&a.slot))
    }

    /// Called by `registry_use_builtin_tramp`. Assigns the next pending slot to
    /// `encoder_id` if it has not been seen before, then returns whether the
    /// C++ factory should delegate to the builtin encoder.
    pub(crate) fn use_builtin_for(&self, encoder_id: u64) -> bool {
        let mut assigned = self.assigned.lock().unwrap();
        if let Some(a) = assigned.get(&encoder_id) {
            return matches!(a.slot, RegistrySlot::Builtin(_));
        }
        let next = {
            let mut pending = self.pending.lock().unwrap();
            pending.pop_front()
        };
        let Some(reservation) = next else {
            // No reservation left: the positional fallback, loud — previously
            // this degraded a custom-encoded stream to builtin silently.
            eprintln!(
                "[reactor-webrtc] no encoder slot reservation left for encoder {encoder_id}: delegating to builtin — custom-encoded tracks after this point may not produce video"
            );
            return true;
        };
        let is_builtin = matches!(reservation.slot, RegistrySlot::Builtin(_));
        assigned.insert(
            encoder_id,
            AssignedSlot {
                native_id: reservation.native_id,
                slot: reservation.slot,
            },
        );
        is_builtin
    }

    /// Called by `registry_encode_tramp`. Produces the next encoded frame for
    /// this encoder instance — draining the queue for pre-encoded slots,
    /// invoking the user callback for inline slots — assigning its slot on
    /// first call if needed.
    fn encode_for(&self, encoder_id: u64, raw: &RawVideoFrame) -> Option<EncodedVideoFrame> {
        let mut assigned = self.assigned.lock().unwrap();
        let a = assigned.entry(encoder_id).or_insert_with(|| {
            let reservation = self.pending.lock().unwrap().pop_front();
            match reservation {
                Some(r) => AssignedSlot {
                    native_id: r.native_id,
                    slot: r.slot,
                },
                None => {
                    eprintln!(
                        "[reactor-webrtc] no encoder slot reservation left for encoder {encoder_id}: delegating to builtin"
                    );
                    AssignedSlot {
                        native_id: 0,
                        slot: RegistrySlot::Builtin(H264BackendPref::Auto),
                    }
                }
            }
        });
        match &a.slot {
            RegistrySlot::Custom(q) => {
                let q = q.clone();
                drop(assigned);
                let frame = q.lock().unwrap().pop_front();
                frame
            }
            RegistrySlot::Inline(state) => {
                let state = state.clone();
                drop(assigned);
                let Ok(mut cb) = state.cb.lock() else {
                    return None;
                };
                cb(raw)
            }
            RegistrySlot::Builtin(_) => None,
        }
    }

    /// Called by the native composite when building an H264 encoder for a
    /// backend-routed instance — the slot's stored preference.
    pub(crate) fn backend_for(&self, encoder_id: u64) -> H264BackendPref {
        match self.assigned.lock().unwrap().get(&encoder_id) {
            Some(a) => match &a.slot {
                RegistrySlot::Builtin(pref) => *pref,
                _ => H264BackendPref::Auto,
            },
            None => H264BackendPref::Auto,
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

pub(crate) extern "C" fn registry_encode_tramp(
    ud: *mut c_void,
    raw: *const reactor_webrtc_sys::ReactorRawVideoFrame,
    out: *mut reactor_webrtc_sys::ReactorEncodedVideoOutput,
) -> c_int {
    let Some(r) = (unsafe { raw.as_ref() }) else {
        return 1;
    };
    let st = unsafe { &*(ud as *const RegistryState) };
    let frame = raw_view(r);
    match st.registry.encode_for(r.encoder_id, &frame) {
        None => 1,
        Some(encoded) => fill_output(encoded, out),
    }
}

/// Called by C++ before creating each encoder instance. Returns 1 if the
/// builtin VP8/VP9/AV1 encoder should be used for this slot, 0 for custom.
pub(crate) extern "C" fn registry_use_builtin_tramp(ud: *mut c_void, encoder_id: u64) -> c_int {
    let st = unsafe { &*(ud as *const RegistryState) };
    st.registry.use_builtin_for(encoder_id) as c_int
}

pub(crate) extern "C" fn free_registry_state_tramp(ud: *mut c_void) {
    drop(unsafe { Box::from_raw(ud as *mut RegistryState) });
}

/// Called by the native factory when enumerating advertised video codecs.
/// Returns nonzero while any custom (pre-encoded or inline) slot exists.
pub(crate) extern "C" fn registry_has_custom_tramp(ud: *mut c_void) -> c_int {
    let st = unsafe { &*(ud as *const RegistryState) };
    st.registry.has_custom() as c_int
}

/// Called by the native composite when it builds an H264 encoder for a
/// backend-routed instance — returns the slot's stored preference as the
/// wire code (0 = auto, 1 = VideoToolbox, 2 = OpenH264).
pub(crate) extern "C" fn registry_backend_for_tramp(ud: *mut c_void, encoder_id: u64) -> c_int {
    let st = unsafe { &*(ud as *const RegistryState) };
    st.registry.backend_for(encoder_id).as_c_int()
}

// ── Per-track encoder selection ─────────────────────────────────────────────

/// Geometry for a pre-encoded video track
/// ([`TrackVideoEncoder::PreEncoded`]).
///
/// `width`/`height` set the resolution libwebrtc's encoder pipeline is
/// configured for. They must match what your encoder actually produces. Pass
/// 0 in [`EncodedVideoFrame`] fields to inherit them per frame.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct PreEncodedOptions {
    /// Encoded width in pixels.
    pub width: u32,
    /// Encoded height in pixels.
    pub height: u32,
}

impl PreEncodedOptions {
    /// Pre-encoded track encoded at `width`×`height`.
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// How a video track produces its encoded bitstream, replacing libwebrtc's
/// builtin encoder **for that track only**.
///
/// One mechanism, two feeding styles:
///
/// - [`PreEncoded`](TrackVideoEncoder::PreEncoded) — **asynchronous**: you
///   push already-encoded bytes at your own pace, from any thread (a
///   hardware encoder callback, a relay, a file dumper). A queue decouples
///   your producer from the encoder thread. The track comes back as
///   [`LocalVideoTrack::Encoded`] and you feed it with
///   [`EncodedVideoTrack::push_encoded_frame`].
/// - [`Inline`](TrackVideoEncoder::Inline) — **synchronous**: libwebrtc calls
///   your closure on the encoder thread with every raw I420 frame; return
///   `Some(encoded)` to inject bytes into the RTP stack or `None` to drop the
///   frame. Right when your pipeline is raw-in/encoded-out and you want
///   libwebrtc to drive the cadence. The track comes back as
///   [`LocalVideoTrack::Raw`] and you feed it like any raw track.
///
/// Either way, the bytes **you** produce must match the codec negotiated for
/// the track's transceiver — read [`RawVideoFrame::codec`] (inline) or set
/// codec preferences to pin it.
///
/// # Slot assignment order — positional, with fresh upgrades
///
/// Slots route encoder instances ↔ tracks **positionally**: reservation order
/// (track creation) must match the negotiation order of the transceivers
/// carrying those tracks. Create encoder-carrying tracks **before** the
/// peer connection they're attached to negotiates (before `create_offer`),
/// or the registry may not yet see a reservation to bind to; ordering across
/// **multiple** peer connections sharing one factory follows whichever
/// negotiates first, not track creation order.
///
/// Dropping an encoder-carrying track retracts its pending slots and degrades
/// any surviving assigned slot to Builtin (loud fallback, never another
/// track's). libwebrtc may recreate encoder instances on internal errors or
/// renegotiation past a dead track; that path degrades the same way rather
/// than re-routing another live track's pipeline.
#[non_exhaustive]
pub enum TrackVideoEncoder {
    /// You push already-encoded bytes at your own pace.
    PreEncoded(PreEncodedOptions),
    /// libwebrtc calls your encoder synchronously with every raw I420 frame.
    Inline(InlineEncoderCallback),
}

/// Which H.264 backend a **raw** video track encodes with.
///
/// `None` (Auto, the default) picks the platform default: **VideoToolbox**
/// on Apple, **OpenH264** — when registered via
/// [`PeerConnectionFactoryBuilder::with_openh264`] — elsewhere. Without any
/// usable backend, H264 is simply not negotiated (VP8/VP9/AV1 take over).
///
/// Choose explicitly to force a backend per track — a camera on VideoToolbox
/// (hardware, battery-friendly) beside a screen share on OpenH264
/// (bitrate/intra control, cross-platform parity). An explicit-but-unusable
/// backend (VideoToolbox off Apple, OpenH264 unregistered) fails track
/// creation with an error.
///
/// Meaningless — and rejected — on tracks with
/// [`TrackVideoEncoder`] set, since the track's bytes come from your own
/// pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum H264Backend {
    /// Apple's hardware VideoToolbox (macOS/iOS).
    VideoToolbox,
    /// Cisco's software OpenH264 — requires registering the shared library
    /// with [`PeerConnectionFactoryBuilder::with_openh264`].
    #[cfg(feature = "openh264")]
    OpenH264,
    // Future: MediaFoundation (Windows hw), NvEnc (NVIDIA), …
}

/// Options for [`PeerConnectionFactory::create_video_track_with_options`].
///
/// All fields optional; `Default::default()` produces exactly what
/// [`PeerConnectionFactory::create_video_track`] produces. The struct is
/// `#[non_exhaustive]` — construct via `Default` and assign fields:
///
/// ```rust,ignore
/// let mut opts = VideoTrackOptions::default();
/// opts.encoder = Some(TrackVideoEncoder::PreEncoded(PreEncodedOptions::new(1280, 720)));
/// ```
#[derive(Default)]
#[non_exhaustive]
pub struct VideoTrackOptions {
    /// Encoder override for this track. `None` (default) = libwebrtc's
    /// builtin/software pipeline encodes.
    pub encoder: Option<TrackVideoEncoder>,
    /// Which H.264 backend encodes this track when it is raw and H264 wins
    /// negotiation. Errors when set together with `encoder`.
    pub h264_backend: Option<H264Backend>,
    /// Per-track frame-metadata switch. `None` (default) inherits the
    /// factory's kill switch
    /// ([`PeerConnectionFactoryBuilder::with_metadata`]); `Some(false)`
    /// disables trailers for this track only — pushes with `user_data` drop
    /// it silently (the frame itself still flows) and the peer sees no
    /// trailer whatever the connection negotiated. The SDP stays
    /// session-level either way.
    pub frame_metadata: Option<bool>,
}

/// One video track created by
/// [`PeerConnectionFactory::create_video_track_with_options`].
///
/// - [`Raw`](LocalVideoTrack::Raw) — push BGRA frames; either libwebrtc
///   encodes them itself (default) or they go through an
///   [`Inline`](TrackVideoEncoder::Inline) callback.
/// - [`Encoded`](LocalVideoTrack::Encoded) — push pre-encoded bytes;
///   libwebrtc only packetises.
pub enum LocalVideoTrack {
    /// A raw (BGRA-in) video track.
    Raw(crate::media::VideoTrack),
    /// A pre-encoded (bytes-in) video track.
    Encoded(EncodedVideoTrack),
}

impl LocalVideoTrack {
    /// The underlying video track handle — attach this to a transceiver with
    /// [`Transceiver::set_track`](crate::Transceiver::set_track).
    pub fn track(&self) -> &crate::media::VideoTrack {
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

    /// Returns the inner raw [`VideoTrack`](crate::media::VideoTrack), or
    /// `None` if this is a pre-encoded track.
    pub fn as_raw(&self) -> Option<&crate::media::VideoTrack> {
        match self {
            Self::Raw(t) => Some(t),
            Self::Encoded(_) => None,
        }
    }
}

// ── Push-based encoded video track ───────────────────────────────────────────

/// A video track that accepts **pre-encoded** frames directly, bypassing the
/// libwebrtc software encoder pipeline entirely.
///
/// Obtain one via
/// [`PeerConnectionFactory::create_video_track_with_options`] with
/// [`TrackVideoEncoder::PreEncoded`], then:
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
    pub(crate) track: crate::media::VideoTrack,
    pub(crate) queue: Arc<Mutex<VecDeque<EncodedVideoFrame>>>,
    // Pre-allocated BGRA buffer used to trigger the WebRTC encoder thread.
    // The dimensions are kept in sync with the track's configured resolution
    // so the pipeline doesn't reject the frame due to a size mismatch.
    dummy: Vec<u8>,
    width: u32,
    height: u32,
    // FIFO metadata queue for push_encoded_frame_with_metadata: the sender
    // FrameTransform pops one entry per encoded frame in push order. A FIFO rather
    // than timestamp correlation because capture_time_ms is unreliable here —
    // VideoStreamEncoder clamps future timestamps back to post_time, which can
    // collide when two pushes land in the same millisecond.
    sender_meta_fifo: Arc<FifoMeta>,
}

/// An [`EncodedVideoTrack`]'s outgoing metadata, in push order.
///
/// Registered in [`crate::sender_meta`] under the inner track's native identity,
/// *replacing* the timestamp-keyed source that [`crate::Track`] registered for it:
/// for a pre-encoded track, push order is the correlation that holds.
#[derive(Default)]
pub(crate) struct FifoMeta(Mutex<VecDeque<crate::metadata::FrameMetadata>>);

impl crate::sender_meta::SenderMetaSource for FifoMeta {
    fn take(&self, _frame: &EncodedFrame) -> Option<crate::metadata::FrameMetadata> {
        // Pops whether or not the caller will use the result. The queue is filled
        // by push_encoded_frame_with_metadata regardless of what the peer declared,
        // so leaving entries in place would grow it for as long as the peer
        // declines and then emit stale metadata if it ever accepted.
        self.0.lock().ok()?.pop_front()
    }
}

// SAFETY: the queue is Mutex-guarded and the dummy buffer is owned; both are
// safe to move across threads.
unsafe impl Send for EncodedVideoTrack {}
unsafe impl Sync for EncodedVideoTrack {}

impl EncodedVideoTrack {
    pub(crate) fn new(
        track: crate::media::VideoTrack,
        queue: Arc<Mutex<VecDeque<EncodedVideoFrame>>>,
        width: u32,
        height: u32,
    ) -> Self {
        let dummy = vec![0u8; (width * height * 4) as usize];
        let sender_meta_fifo = Arc::new(FifoMeta::default());
        // Overwrite the inner Track's timestamp-keyed registration: both describe
        // the same native track, and for pre-encoded frames the FIFO is the one
        // that correlates correctly.
        let source: Arc<dyn crate::sender_meta::SenderMetaSource> = sender_meta_fifo.clone();
        crate::sender_meta::register(track.native_id(), &source);
        Self {
            track,
            queue,
            dummy,
            width,
            height,
            sender_meta_fifo,
        }
    }

    /// The underlying video track handle — pass `track()` (or the
    /// [`VideoTrack`](crate::VideoTrack) itself) to
    /// [`Transceiver::set_track`](crate::Transceiver::set_track); it derefs to
    /// [`&Track`](crate::Track).
    pub fn track(&self) -> &crate::media::VideoTrack {
        &self.track
    }

    /// Inject a pre-encoded frame into the WebRTC RTP stack.
    ///
    /// The call returns immediately; the frame is queued and forwarded to the
    /// RTP packetizer on the WebRTC encoder thread. Thread-safe — call from
    /// any thread, including a hardware encoder callback.
    ///
    /// Set `frame.width` / `frame.height` to 0 to inherit from the track's
    /// configured resolution — the
    /// [`PreEncodedOptions`] the track was created with.
    pub fn push_frame(&self, frame: EncodedVideoFrame) -> crate::Result<()> {
        // Queue first so the frame is always present when the encoder thread
        // dequeues it (the two operations are not atomic, but the encoder
        // thread is asynchronous, so the queue push always wins the race).
        self.queue.lock().unwrap().push_back(frame);
        // Push a dummy raw frame to wake the WebRTC encoder thread.
        // The I420 data is thrown away in the encoder callback — the actual
        // encoded bytes come from the queue above.
        self.track.push_frame(crate::media::VideoFrame::new(
            &self.dummy,
            self.width,
            self.height,
        ))?;
        Ok(())
    }

    /// Inject a pre-encoded frame with per-frame metadata embedded as a
    /// protobuf trailer.
    ///
    /// `frame_id` and `capture_time_us` are filled in internally, the latter from
    /// [`time_micros`](crate::time_micros); the caller supplies only `user_data`.
    /// A capture time cannot be declared here the way it can on a raw
    /// [`Track`](crate::Track): these frames arrive already encoded and the
    /// encoder clamps future capture timestamps, which is why this path
    /// correlates its metadata by FIFO order instead. Requires
    /// [`sender_metadata_transform`](Self::sender_metadata_transform) to be
    /// called and the returned [`FrameTransform`](crate::FrameTransform) to
    /// be attached to the sender transceiver before the first SDP exchange.
    pub fn push_frame_with_metadata(
        &self,
        frame: EncodedVideoFrame,
        user_data: &[u8],
    ) -> crate::Result<()> {
        let meta = crate::metadata::FrameMetadata {
            frame_id: self.track.next_frame_id(),
            capture_time_us: crate::time_micros().max(0) as u64,
            user_data: user_data.to_vec(),
        };
        // Enqueue metadata first so the FIFO entry is always present when the
        // sender FrameTransform fires on the encoder thread. Off-track
        // (frame_metadata off) drops user_data here, mirroring the raw path.
        if self.track.metadata_enabled() {
            if let Ok(mut fifo) = self.sender_meta_fifo.0.lock() {
                fifo.push_back(meta);
            }
        }
        self.queue.lock().unwrap().push_back(frame);
        self.track.push_frame(crate::media::VideoFrame::new(
            &self.dummy,
            self.width,
            self.height,
        ))?;
        Ok(())
    }
}
