//! The peer connection and its associated signaling/data types.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Mutex;
use std::time::Duration;

use crate::encoded::FrameTransform;
use crate::media::{MediaKind, Track};
use crate::observer::ObserverState;
use crate::{Error, Result};

/// How long to wait for an async native op (create offer/answer, set
/// description, add ICE candidate) to complete before giving up.
const OP_TIMEOUT: Duration = Duration::from_secs(10);

/// SDP description kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpType {
    Offer,
    PrAnswer,
    Answer,
    Rollback,
}

impl SdpType {
    fn as_str(self) -> &'static str {
        match self {
            SdpType::Offer => "offer",
            SdpType::PrAnswer => "pranswer",
            SdpType::Answer => "answer",
            SdpType::Rollback => "rollback",
        }
    }
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "offer" => Some(SdpType::Offer),
            "pranswer" => Some(SdpType::PrAnswer),
            "answer" => Some(SdpType::Answer),
            "rollback" => Some(SdpType::Rollback),
            _ => None,
        }
    }
}

/// A session description (offer/answer).
#[derive(Debug, Clone)]
pub struct SessionDescription {
    pub kind: SdpType,
    pub sdp: String,
}

/// A trickled ICE candidate.
#[derive(Debug, Clone)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

/// Direction of a transceiver / track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransceiverDirection {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl TransceiverDirection {
    fn to_raw(self) -> c_int {
        match self {
            TransceiverDirection::SendRecv => 0,
            TransceiverDirection::SendOnly => 1,
            TransceiverDirection::RecvOnly => 2,
            TransceiverDirection::Inactive => 3,
        }
    }
}

/// ICE gathering state (delivered to
/// [`PeerConnectionObserver::on_ice_gathering_change`](crate::PeerConnectionObserver)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceGatheringState {
    New,
    Gathering,
    Complete,
}

impl IceGatheringState {
    pub(crate) fn from_raw(state: c_int) -> Self {
        match state {
            1 => IceGatheringState::Gathering,
            2 => IceGatheringState::Complete,
            _ => IceGatheringState::New,
        }
    }
}

/// A transceiver: one bidirectional media "slot" in the peer connection. Its
/// `mid` (available after `set_local_description`) maps it to an SDP m-section.
pub struct Transceiver {
    raw: *mut reactor_webrtc_sys::RtpTransceiver,
}

// SAFETY: the native transceiver is internally thread-safe.
unsafe impl Send for Transceiver {}
unsafe impl Sync for Transceiver {}

impl Transceiver {
    pub(crate) fn from_raw(raw: *mut reactor_webrtc_sys::RtpTransceiver) -> Self {
        Self { raw }
    }

    /// The transceiver's media kind (audio/video).
    pub fn kind(&self) -> MediaKind {
        let k =
            unsafe { reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_media_kind(self.raw) };
        MediaKind::from_raw(k)
    }

    /// The transceiver's mid, once assigned (after `set_local_description`).
    pub fn mid(&self) -> Option<String> {
        let mut buf = [0u8; 256];
        let n = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_mid(
                self.raw,
                buf.as_mut_ptr() as *mut c_char,
                buf.len() as c_int,
            )
        };
        if n < 0 {
            None
        } else {
            Some(
                unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    /// Attach a local track to this transceiver's sender (for sendonly/sendrecv).
    pub fn set_track(&self, track: &Track) -> Result<()> {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_track(self.raw, track.raw())
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc("transceiver set_track failed".into()))
        }
    }

    /// Attach an encoded-frame transform to this transceiver's **sender**
    /// (encoder → packetizer): observe/replace/drop each encoded frame before
    /// it is sent. See [`crate::FrameTransform`]. The transform must outlive
    /// this transceiver's use of it.
    pub fn set_sender_transform(&self, transform: &FrameTransform) -> Result<()> {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_sender_transform(
                self.raw,
                transform.raw(),
            )
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc("transceiver set_sender_transform failed".into()))
        }
    }

    /// Attach an encoded-frame transform to this transceiver's **receiver**
    /// (depacketizer → decoder): observe each encoded frame before decode, and
    /// [`FrameAction::Drop`](crate::FrameAction) to bypass the decoder. See
    /// [`crate::FrameTransform`]. The transform must outlive this transceiver's
    /// use of it.
    pub fn set_receiver_transform(&self, transform: &FrameTransform) -> Result<()> {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_receiver_transform(
                self.raw,
                transform.raw(),
            )
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc(
                "transceiver set_receiver_transform failed".into(),
            ))
        }
    }
}

impl Drop for Transceiver {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_destroy(self.raw) }
    }
}

/// Aggregate connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

impl PeerConnectionState {
    pub(crate) fn from_raw(state: c_int) -> Self {
        match state {
            0 => PeerConnectionState::New,
            1 => PeerConnectionState::Connecting,
            2 => PeerConnectionState::Connected,
            3 => PeerConnectionState::Disconnected,
            4 => PeerConnectionState::Failed,
            _ => PeerConnectionState::Closed,
        }
    }
}

type MessageCb = Box<dyn for<'a> FnMut(&'a [u8], bool) + Send>;
type EventCb = Box<dyn FnMut() + Send>;

// Heap-pinned data-channel callback state addressed by the sys `userdata`.
#[derive(Default)]
struct DcObserverState {
    on_message: Option<Mutex<MessageCb>>,
    on_open: Option<Mutex<EventCb>>,
    on_close: Option<Mutex<EventCb>>,
}

extern "C" fn dc_on_message(ud: *mut c_void, data: *const u8, len: usize, binary: c_int) {
    let st = unsafe { &*(ud as *const DcObserverState) };
    if let Some(m) = &st.on_message {
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        if let Ok(mut cb) = m.lock() {
            cb(bytes, binary != 0);
        }
    }
}
extern "C" fn dc_on_open(ud: *mut c_void) {
    let st = unsafe { &*(ud as *const DcObserverState) };
    if let Some(m) = &st.on_open {
        if let Ok(mut cb) = m.lock() {
            cb();
        }
    }
}
extern "C" fn dc_on_close(ud: *mut c_void) {
    let st = unsafe { &*(ud as *const DcObserverState) };
    if let Some(m) = &st.on_close {
        if let Ok(mut cb) = m.lock() {
            cb();
        }
    }
}

/// A negotiated data channel. Dropping it releases the native handle.
pub struct DataChannel {
    raw: *mut reactor_webrtc_sys::DataChannel,
    // Keeps the callback closures alive while the native observer is registered.
    observer: Option<Box<DcObserverState>>,
}

// SAFETY: the native data channel is internally thread-safe; callbacks are
// serialized on the signaling thread and guarded by mutexes.
unsafe impl Send for DataChannel {}
unsafe impl Sync for DataChannel {}

impl DataChannel {
    pub(crate) fn from_raw(raw: *mut reactor_webrtc_sys::DataChannel) -> Self {
        Self {
            raw,
            observer: None,
        }
    }

    /// Send bytes over the channel (`binary` selects the message type).
    pub fn send(&self, data: &[u8], binary: bool) -> Result<()> {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_data_channel_send(
                self.raw,
                data.as_ptr(),
                data.len(),
                binary as c_int,
            )
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc("data channel send failed".into()))
        }
    }

    /// Set the message handler. The closure runs on a WebRTC thread.
    pub fn on_message(&mut self, cb: impl for<'a> FnMut(&'a [u8], bool) + Send + 'static) {
        self.observer
            .get_or_insert_with(Default::default)
            .on_message = Some(Mutex::new(Box::new(cb)));
        self.reregister();
    }
    /// Set the open handler (fires when the channel becomes open).
    pub fn on_open(&mut self, cb: impl FnMut() + Send + 'static) {
        self.observer.get_or_insert_with(Default::default).on_open = Some(Mutex::new(Box::new(cb)));
        self.reregister();
    }
    /// Set the close handler.
    pub fn on_close(&mut self, cb: impl FnMut() + Send + 'static) {
        self.observer.get_or_insert_with(Default::default).on_close =
            Some(Mutex::new(Box::new(cb)));
        self.reregister();
    }

    fn reregister(&mut self) {
        if let Some(state) = &self.observer {
            let ud = &**state as *const DcObserverState as *mut c_void;
            unsafe {
                reactor_webrtc_sys::reactor_webrtc_data_channel_register_observer(
                    self.raw,
                    ud,
                    dc_on_message,
                    dc_on_open,
                    dc_on_close,
                );
            }
        }
    }
}

impl Drop for DataChannel {
    fn drop(&mut self) {
        // Unregisters the native observer before the closure box is freed.
        unsafe { reactor_webrtc_sys::reactor_webrtc_data_channel_destroy(self.raw) }
    }
}

// ── async-op bridges (block on a one-shot callback) ──────────────────────────

type SdpTx = SyncSender<Result<SessionDescription>>;
type CompleteTx = SyncSender<Result<()>>;

extern "C" fn sdp_ok(ud: *mut c_void, ty: *const c_char, sdp: *const c_char) {
    let tx = unsafe { &*(ud as *const SdpTx) };
    let kind = unsafe { CStr::from_ptr(ty) }.to_string_lossy();
    let sdp = unsafe { CStr::from_ptr(sdp) }
        .to_string_lossy()
        .into_owned();
    let result = match SdpType::from_str(&kind) {
        Some(kind) => Ok(SessionDescription { kind, sdp }),
        None => Err(Error::Webrtc(format!("unknown sdp type: {kind}"))),
    };
    let _ = tx.try_send(result);
}
extern "C" fn sdp_err(ud: *mut c_void, message: *const c_char) {
    let tx = unsafe { &*(ud as *const SdpTx) };
    let msg = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    let _ = tx.try_send(Err(Error::Webrtc(msg)));
}
extern "C" fn complete_cb(ud: *mut c_void, error: *const c_char) {
    let tx = unsafe { &*(ud as *const CompleteTx) };
    let r = if error.is_null() {
        Ok(())
    } else {
        Err(Error::Webrtc(
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned(),
        ))
    };
    let _ = tx.try_send(r);
}

fn run_sdp(call: impl FnOnce(*mut c_void)) -> Result<SessionDescription> {
    let (tx, rx) = sync_channel::<Result<SessionDescription>>(1);
    let p = Box::into_raw(Box::new(tx));
    call(p as *mut c_void);
    let r = rx.recv_timeout(OP_TIMEOUT);
    drop(unsafe { Box::from_raw(p) });
    r.map_err(|_| Error::Webrtc("sdp operation timed out".into()))?
}

fn run_complete(call: impl FnOnce(*mut c_void)) -> Result<()> {
    let (tx, rx) = sync_channel::<Result<()>>(1);
    let p = Box::into_raw(Box::new(tx));
    call(p as *mut c_void);
    let r = rx.recv_timeout(OP_TIMEOUT);
    drop(unsafe { Box::from_raw(p) });
    r.map_err(|_| Error::Webrtc("operation timed out".into()))?
}

/// An `RTCPeerConnection`.
pub struct PeerConnection {
    raw: *mut reactor_webrtc_sys::PeerConnection,
    // Keeps the observer closures alive for the connection's lifetime. The
    // native side holds a pointer into this box.
    _observer: Box<ObserverState>,
}

// SAFETY: the native peer connection is internally thread-safe; observer
// callbacks are serialized on the signaling thread and guarded by mutexes.
unsafe impl Send for PeerConnection {}
unsafe impl Sync for PeerConnection {}

impl PeerConnection {
    pub(crate) fn new(
        raw: *mut reactor_webrtc_sys::PeerConnection,
        observer: Box<ObserverState>,
    ) -> Self {
        Self {
            raw,
            _observer: observer,
        }
    }

    // ── Signaling (blocking on the native callback) ──────────────────────────
    pub fn create_offer(&self) -> Result<SessionDescription> {
        run_sdp(|ud| unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create_offer(
                self.raw, ud, sdp_ok, sdp_err,
            )
        })
    }
    pub fn create_answer(&self) -> Result<SessionDescription> {
        run_sdp(|ud| unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create_answer(
                self.raw, ud, sdp_ok, sdp_err,
            )
        })
    }
    pub fn set_local_description(&self, sdp: &SessionDescription) -> Result<()> {
        self.set_description(sdp, true)
    }
    pub fn set_remote_description(&self, sdp: &SessionDescription) -> Result<()> {
        self.set_description(sdp, false)
    }
    fn set_description(&self, sdp: &SessionDescription, local: bool) -> Result<()> {
        let ty = CString::new(sdp.kind.as_str()).unwrap();
        let body = CString::new(sdp.sdp.as_str())
            .map_err(|_| Error::Webrtc("sdp contains a NUL byte".into()))?;
        run_complete(|ud| unsafe {
            if local {
                reactor_webrtc_sys::reactor_webrtc_peer_connection_set_local_description(
                    self.raw,
                    ty.as_ptr(),
                    body.as_ptr(),
                    ud,
                    complete_cb,
                )
            } else {
                reactor_webrtc_sys::reactor_webrtc_peer_connection_set_remote_description(
                    self.raw,
                    ty.as_ptr(),
                    body.as_ptr(),
                    ud,
                    complete_cb,
                )
            }
        })
    }
    pub fn add_ice_candidate(&self, candidate: &IceCandidate) -> Result<()> {
        let mid = CString::new(candidate.sdp_mid.clone().unwrap_or_default()).unwrap_or_default();
        let cand = CString::new(candidate.candidate.as_str())
            .map_err(|_| Error::Webrtc("candidate contains a NUL byte".into()))?;
        let idx = candidate.sdp_mline_index.unwrap_or(0) as c_int;
        run_complete(|ud| unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_add_ice_candidate(
                self.raw,
                mid.as_ptr(),
                idx,
                cand.as_ptr(),
                ud,
                complete_cb,
            )
        })
    }

    // ── Tracks / data channels ───────────────────────────────────────────────
    /// Add a local track (creates a sendrecv transceiver).
    pub fn add_track(&self, track: &Track) -> Result<()> {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_add_track(self.raw, track.raw())
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc("add_track failed".into()))
        }
    }
    /// Add a transceiver of `kind` with an explicit `direction` (e.g. recvonly
    /// to receive a remote track, sendonly to publish). Returns the transceiver
    /// so its `mid` can be read after `set_local_description`.
    pub fn add_transceiver(
        &self,
        kind: MediaKind,
        direction: TransceiverDirection,
    ) -> Result<Transceiver> {
        let media_kind = match kind {
            MediaKind::Audio => 0,
            MediaKind::Video => 1,
            MediaKind::Unknown => {
                return Err(Error::Webrtc("add_transceiver needs audio or video".into()))
            }
        };
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_add_transceiver(
                self.raw,
                media_kind,
                direction.to_raw(),
            )
        };
        if raw.is_null() {
            Err(Error::Webrtc("add_transceiver failed".into()))
        } else {
            Ok(Transceiver::from_raw(raw))
        }
    }

    /// All transceivers on this peer connection. After negotiation this includes
    /// transceivers auto-created from the remote description — use this to reach
    /// a receiving transceiver (e.g. to attach an encoded-frame transform to its
    /// receiver: match on [`Transceiver::kind`] and call
    /// [`Transceiver::set_receiver_transform`]).
    pub fn transceivers(&self) -> Vec<Transceiver> {
        let n = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_transceiver_count(self.raw)
        };
        (0..n)
            .filter_map(|i| {
                let raw = unsafe {
                    reactor_webrtc_sys::reactor_webrtc_peer_connection_get_transceiver(self.raw, i)
                };
                (!raw.is_null()).then(|| Transceiver::from_raw(raw))
            })
            .collect()
    }

    /// Create an SDP-negotiated data channel.
    pub fn create_data_channel(&self, label: &str) -> Result<DataChannel> {
        let label =
            CString::new(label).map_err(|_| Error::Webrtc("label has a NUL byte".into()))?;
        let raw = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create_data_channel(
                self.raw,
                label.as_ptr(),
            )
        };
        if raw.is_null() {
            Err(Error::Webrtc("create_data_channel returned null".into()))
        } else {
            Ok(DataChannel::from_raw(raw))
        }
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        // Destroy the native PC (stops callbacks) before the observer box drops.
        unsafe { reactor_webrtc_sys::reactor_webrtc_peer_connection_destroy(self.raw) }
    }
}
