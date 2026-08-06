//! The peer connection and its associated signaling/data types.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reactor_webrtc_sys::ReactorStatEntry;

use crate::encoded::FrameTransform;
use crate::media::{MediaKind, Track};
use crate::observer::ObserverState;
use crate::{Error, FactoryHandle, Result};

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

/// RFC 8445 §5.3: an ice-ufrag is 4..=256 characters.
const ICE_UFRAG_LEN: std::ops::RangeInclusive<usize> = 4..=256;
/// RFC 8445 §5.3: an ice-pwd is 22..=256 characters.
const ICE_PWD_LEN: std::ops::RangeInclusive<usize> = 22..=256;

/// Highest `a=extmap` id expressible in RFC 8285's one-byte header form.
/// `RtpHeaderExtensionId::kOneByteHeaderExtensionMaxId` upstream.
const EXTMAP_ONE_BYTE_MAX_ID: u16 = 14;
/// Highest `a=extmap` id libwebrtc accepts at all (two-byte form).
/// `RtpHeaderExtensionId::kMaxId` upstream.
const EXTMAP_MAX_ID: u16 = 255;

/// Split an `a=extmap` line into its id and URI.
///
/// `a=extmap:<value>["/"<direction>] <URI> <extensionattributes>` (RFC 8285 §6).
/// Ids outside 1..=255 are rejected rather than returned: RFC 8285 has no such
/// id, libwebrtc's `VerifyExtensionIds` refuses one, and callers here use the
/// result to index a table.
fn parse_extmap(line: &str) -> Option<(u16, &str)> {
    let (id_direction, rest) = line.strip_prefix("a=extmap:")?.split_once(' ')?;
    let id = id_direction.split('/').next()?.parse::<u16>().ok()?;
    if !(1..=EXTMAP_MAX_ID).contains(&id) {
        return None;
    }
    Some((id, rest.split_whitespace().next()?))
}

/// The lowest one-byte `a=extmap` id unused anywhere in `sdp`.
///
/// Scans the whole description rather than one m-section, because RFC 8843
/// requires an id to identify the same extension across every bundled
/// m-section — audio included — and it is the *peer* that enforces this
/// (`ValidateBundledRtpHeaderExtensions`) when it applies what we signalled. A
/// per-section search could pick an id that is free there and taken next door.
///
/// Restricted to the one-byte range so the result never depends on
/// `a=extmap-allow-mixed` having been negotiated. Since the frame-metadata
/// declaration emits no header bytes, the id's only job is not to collide.
fn free_extmap_id(sdp: &str) -> Option<u16> {
    let mut used = [false; EXTMAP_MAX_ID as usize + 1];
    for (id, _) in sdp.lines().filter_map(parse_extmap) {
        used[id as usize] = true;
    }
    (1..=EXTMAP_ONE_BYTE_MAX_ID).find(|&id| !used[id as usize])
}

/// RFC 8445 `ice-char = ALPHA / DIGIT / "+" / "/"`.
fn is_ice_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '/'
}

fn check_ice_value(
    what: &str,
    value: &str,
    len: std::ops::RangeInclusive<usize>,
) -> crate::Result<()> {
    if !len.contains(&value.len()) {
        return Err(crate::Error::Webrtc(format!(
            "{what} must be {}..={} characters, got {}",
            len.start(),
            len.end(),
            value.len()
        )));
    }
    if let Some(bad) = value.chars().find(|c| !is_ice_char(*c)) {
        return Err(crate::Error::Webrtc(format!(
            "{what} contains {bad:?}, which is not an RFC 8445 ice-char"
        )));
    }
    Ok(())
}

impl SessionDescription {
    /// The `ice-ufrag` values this description carries, in document order.
    ///
    /// One per m-section. Bundled sections repeat the same value; a non-BUNDLE
    /// description has a distinct ufrag per transport.
    pub fn ice_ufrags(&self) -> Vec<&str> {
        self.sdp
            .lines()
            .filter_map(|l| l.strip_prefix("a=ice-ufrag:").map(str::trim_end))
            .collect()
    }

    /// Return a copy with every `ice-ufrag` and `ice-pwd` replaced.
    ///
    /// # Why this exists
    ///
    /// libwebrtc generates ICE credentials itself and exposes no setter — the
    /// `SetIceParameters` entry point lives on `IceTransportInternal`, below the
    /// public API, and calling it out of band would desync the transport from the
    /// description that was signalled. An application that needs to *choose* its
    /// ufrag — routing through an edge relay that demultiplexes on it, for
    /// instance — has to do it here instead.
    ///
    /// It works because the local description is the source of truth:
    /// `JsepTransport::SetLocalJsepTransportDescription` reads `IceParameters`
    /// straight out of the description's transport description, and the only guard
    /// libwebrtc applies to a local description checks that credentials are
    /// *present*, not that it generated them. `tests/ice_credentials.rs` verifies
    /// this by observation rather than by reading: a loopback whose offerer has
    /// substituted credentials connects, which it could not if the transport had
    /// kept its own.
    ///
    /// # Ordering
    ///
    /// Call this on the description returned by `create_offer`/`create_answer` and
    /// before [`PeerConnection::set_local_description`]. Setting the local
    /// description is what creates the transport and starts gathering, so
    /// substituting afterwards has nothing to act on.
    ///
    /// ```no_run
    /// # use reactor_webrtc::PeerConnection;
    /// # fn f(pc: &PeerConnection) -> reactor_webrtc::Result<()> {
    /// let answer = pc.create_answer()?;
    /// let answer = answer.with_ice_credentials("MyRelayIssuedUfrag00", "aPasswordOfAtLeast22Chars")?;
    /// pc.set_local_description(&answer)?;
    /// # Ok(()) }
    /// ```
    ///
    /// # Renegotiation
    ///
    /// Changing `ice-ufrag`/`ice-pwd` between generations *is* an ICE restart
    /// (RFC 8445 §9): the transport discards its checklist and revalidates. So on a
    /// renegotiation that is not meant to restart ICE, re-apply the **same** values
    /// the session already uses. Substituting a fresh pair out of habit — rotating
    /// a routing token, say — restarts connectivity checks and can interrupt media.
    ///
    /// A renegotiation-time description also differs from a first one in carrying
    /// the candidates gathered so far. Those are left untouched, which is correct
    /// for this build: it emits `a=candidate` lines without the optional trailing
    /// `ufrag` token, so no candidate-level value can fall out of step with the
    /// substituted media-level one. `tests/ice_credentials.rs` pins that, since it
    /// is an upstream behaviour rather than a guarantee.
    ///
    /// # Errors
    ///
    /// If either value is outside RFC 8445's length range or contains a character
    /// outside `ice-char` (`ALPHA / DIGIT / "+" / "/"`). Rejecting here rather
    /// than at `set_local_description` keeps the failure attributable: libwebrtc
    /// reports the same problem as a generic invalid-parameters error much later.
    ///
    /// Note that RFC 8839 §5.4 asks a *sender* to keep the ufrag to 32 characters
    /// even though a receiver must accept 256. That is not enforced here, because
    /// the range libwebrtc itself accepts is the one that governs interoperation,
    /// but staying inside 32 is the safer choice.
    pub fn with_ice_credentials(&self, ufrag: &str, pwd: &str) -> crate::Result<Self> {
        check_ice_value("ice-ufrag", ufrag, ICE_UFRAG_LEN)?;
        check_ice_value("ice-pwd", pwd, ICE_PWD_LEN)?;

        if self.ice_ufrags().is_empty() {
            return Err(crate::Error::Webrtc(
                "session description carries no ice-ufrag to replace".into(),
            ));
        }

        // Every occurrence, not the first: bundled m-sections each carry the
        // attribute and they must agree, so replacing one would leave an SDP that
        // is inconsistent rather than substituted.
        let mut out = String::with_capacity(self.sdp.len() + 64);
        for line in self.sdp.lines() {
            if line.starts_with("a=ice-ufrag:") {
                out.push_str("a=ice-ufrag:");
                out.push_str(ufrag);
            } else if line.starts_with("a=ice-pwd:") {
                out.push_str("a=ice-pwd:");
                out.push_str(pwd);
            } else {
                out.push_str(line);
            }
            // SDP lines are CRLF-terminated (RFC 4566 §5); `lines` has already
            // stripped whatever the input used.
            out.push_str("\r\n");
        }

        Ok(Self {
            kind: self.kind,
            sdp: out,
        })
    }

    /// The `a=extmap` id this description declares
    /// [`FRAME_METADATA_URI`](crate::metadata::FRAME_METADATA_URI) with, if any.
    ///
    /// Read it on an offer to echo the same id back in the answer, the way
    /// libwebrtc does for the extensions it knows.
    pub fn frame_metadata_id(&self) -> Option<u16> {
        self.sdp
            .lines()
            .filter_map(parse_extmap)
            .find(|&(_, uri)| uri == crate::metadata::FRAME_METADATA_URI)
            .map(|(id, _)| id)
    }

    /// Whether this description declares support for frame-metadata trailers.
    ///
    /// This is what [`PeerConnection::set_remote_description`] arms the
    /// connection's [`FrameMetadataGate`](crate::FrameMetadataGate) from.
    pub fn declares_frame_metadata(&self) -> bool {
        self.frame_metadata_id().is_some()
    }

    /// Return a copy declaring frame-metadata support on every video m-section,
    /// allocating the lowest free one-byte `a=extmap` id.
    ///
    /// [`create_offer`](PeerConnection::create_offer) already applies this to
    /// every offer, and [`create_answer`](PeerConnection::create_answer) mirrors
    /// the offer — so a caller using this crate's signalling path never needs to
    /// call it. It is public for callers that assemble or rewrite descriptions
    /// themselves.
    ///
    /// # Why an extmap
    ///
    /// The capability has to survive the trip. libwebrtc drops `a=` attributes it
    /// does not recognise when it parses a description, so a bespoke attribute
    /// would arrive as nothing; an `a=extmap` arrives as a first-class
    /// `RtpExtension` because `ParseExtmap` applies no URI whitelist. It also
    /// costs nothing on the wire:
    /// `RtpHeaderExtensionMap::RegisterByUri` refuses to map an unknown URI, so
    /// no header extension is ever written to a packet.
    ///
    /// What it does *not* buy is automatic negotiation.
    /// `NegotiateRtpHeaderExtensions` builds an answer by walking the extensions
    /// the local engine supports and looking each up in the offer, so a URI it
    /// has never heard of is never echoed. Both peers inject the line themselves
    /// — the answerer with
    /// [`with_frame_metadata_id`](Self::with_frame_metadata_id), reusing the
    /// offer's id.
    ///
    /// # The description this produces is set locally too
    ///
    /// `create_offer` returns an already-declared description, so the same bytes
    /// go to `set_local_description` and to the peer. libwebrtc therefore parses
    /// our unknown URI on both sides, which is fine — `RegisterByUri` declines it
    /// with a log line rather than an error — but it does mean the id has to
    /// satisfy `VerifyExtensionIds` and the RFC 8843 bundle rule locally as well
    /// as remotely. That is what the bundle-wide id search is for.
    ///
    /// # Errors
    ///
    /// If the description has no video m-section carrying an `a=mid:`, or if
    /// every id in 1..=14 is already spoken for.
    ///
    /// Already declaring the capability is not an error — the description comes
    /// back unchanged.
    pub fn with_frame_metadata(&self) -> crate::Result<Self> {
        // Route the already-declared case through the id path so idempotence is
        // decided in exactly one place.
        let id = match self.frame_metadata_id() {
            Some(id) => id,
            None => free_extmap_id(&self.sdp).ok_or_else(|| {
                crate::Error::Webrtc(format!(
                    "every extmap id in 1..={EXTMAP_ONE_BYTE_MAX_ID} is taken, \
                     so frame-metadata support cannot be declared"
                ))
            })?,
        };
        self.with_frame_metadata_id(id)
    }

    /// Return a copy declaring frame-metadata support with a specific
    /// `a=extmap` id.
    ///
    /// This is what [`create_answer`](PeerConnection::create_answer) uses to echo
    /// the offer's id, and it is public for the same reason
    /// [`with_frame_metadata`](Self::with_frame_metadata) is.
    ///
    /// A divergent id would in fact be harmless — both ends ignore the URI, so
    /// nothing reads the number — but matching costs nothing and keeps the
    /// description something a strict middlebox has no opinion about.
    ///
    /// # Errors
    ///
    /// If `id` is outside 1..=255, if it is already bound to a *different* URI in
    /// this description (which would fail the peer's `VerifyExtensionIds`), if
    /// there is no video m-section with an `a=mid:`, or if the description
    /// already declares the capability under some other id.
    pub fn with_frame_metadata_id(&self, id: u16) -> crate::Result<Self> {
        if !(1..=EXTMAP_MAX_ID).contains(&id) {
            return Err(crate::Error::Webrtc(format!(
                "extmap id must be 1..={EXTMAP_MAX_ID}, got {id}"
            )));
        }
        match self.frame_metadata_id() {
            Some(existing) if existing == id => return Ok(self.clone()),
            Some(existing) => {
                return Err(crate::Error::Webrtc(format!(
                    "description already declares frame-metadata support with id \
                     {existing}, refusing to add a second declaration as {id}"
                )))
            }
            None => {}
        }
        // Rejecting here keeps the failure ours. Signalling a colliding id would
        // instead surface as the *peer's* set_remote_description failing, which
        // is a much harder thing to attribute.
        if let Some(uri) = self
            .sdp
            .lines()
            .filter_map(parse_extmap)
            .find(|&(n, _)| n == id)
            .map(|(_, uri)| uri)
        {
            return Err(crate::Error::Webrtc(format!(
                "extmap id {id} is already bound to {uri} in this description"
            )));
        }

        let mut out = String::with_capacity(self.sdp.len() + 96);
        let mut in_video = false;
        let mut declared = 0usize;
        for line in self.sdp.lines() {
            out.push_str(line);
            out.push_str("\r\n");
            if let Some(rest) = line.strip_prefix("m=") {
                in_video = rest.starts_with("video");
            } else if in_video && line.starts_with("a=mid:") {
                // Anchored on a=mid: rather than on the m= line, because RFC 8866
                // §5 puts the attribute block after c= and b=. Inserting straight
                // after m= would emit an a= line ahead of them.
                out.push_str("a=extmap:");
                out.push_str(&id.to_string());
                out.push(' ');
                out.push_str(crate::metadata::FRAME_METADATA_URI);
                out.push_str("\r\n");
                declared += 1;
            }
        }
        if declared == 0 {
            return Err(crate::Error::Webrtc(
                "no video m-section with an a=mid: to declare frame-metadata \
                 support on"
                    .into(),
            ));
        }

        Ok(Self {
            kind: self.kind,
            sdp: out,
        })
    }
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
        let k = unsafe { reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_media_kind(self.raw) };
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

    /// Set the transceiver's direction — controls what appears in the next
    /// `create_answer()` / `create_offer()` for this m-section.
    pub fn set_direction(&self, direction: TransceiverDirection) -> Result<()> {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_direction(
                self.raw,
                direction.to_raw() as c_int,
            )
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc("transceiver set_direction failed".into()))
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
            Err(Error::Webrtc(
                "transceiver set_sender_transform failed".into(),
            ))
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

/// Data channel readiness state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataChannelState {
    Connecting,
    Open,
    Closing,
    Closed,
}

impl DataChannelState {
    fn from_raw(v: c_int) -> Self {
        match v {
            0 => DataChannelState::Connecting,
            1 => DataChannelState::Open,
            2 => DataChannelState::Closing,
            _ => DataChannelState::Closed,
        }
    }
}

// ── Stats types ──────────────────────────────────────────────────────────────

/// State of an ICE candidate pair (`RTCIceCandidatePairStats::state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceCandidatePairState {
    Waiting,
    InProgress,
    Failed,
    Succeeded,
    Cancelled,
}

impl IceCandidatePairState {
    fn from_raw(v: c_int) -> Self {
        match v {
            1 => IceCandidatePairState::InProgress,
            2 => IceCandidatePairState::Failed,
            3 => IceCandidatePairState::Succeeded,
            4 => IceCandidatePairState::Cancelled,
            _ => IceCandidatePairState::Waiting,
        }
    }
}

/// `RTCInboundRtpStreamStats` subset.
#[derive(Debug, Clone)]
pub struct InboundRtpStats {
    pub ssrc: u32,
    pub packets_received: u32,
    pub bytes_received: u64,
    /// Jitter in seconds.
    pub jitter_s: f64,
    pub packets_lost: i32,
    pub nack_count: u32,
    /// Cumulative decode time in seconds.
    pub total_decode_time_s: f64,
}

/// `RTCOutboundRtpStreamStats` subset.
#[derive(Debug, Clone)]
pub struct OutboundRtpStats {
    pub ssrc: u32,
    pub packets_sent: u32,
    pub bytes_sent: u64,
    /// Target encoder bitrate in bps.
    pub target_bitrate_bps: f64,
    /// Round-trip time in seconds; `0.0` if not yet measured.
    pub round_trip_time_s: f64,
    pub retransmitted_packets_sent: u32,
}

/// `RTCIceCandidatePairStats` subset.
#[derive(Debug, Clone)]
pub struct IceCandidatePairStats {
    /// Current RTT in seconds; `0.0` if not yet measured.
    pub current_round_trip_time_s: f64,
    pub priority: u64,
    pub state: IceCandidatePairState,
}

/// A snapshot of the stats delivered by [`PeerConnection::get_stats`].
#[derive(Debug, Clone, Default)]
pub struct StatsReport {
    pub inbound_rtp: Vec<InboundRtpStats>,
    pub outbound_rtp: Vec<OutboundRtpStats>,
    pub candidate_pairs: Vec<IceCandidatePairStats>,
}

// ── Data channel callbacks ────────────────────────────────────────────────────

type MessageCb = Box<dyn for<'a> FnMut(&'a [u8], bool) + Send>;
type EventCb = Box<dyn FnMut() + Send>;
type StateCb = Box<dyn FnMut(DataChannelState) + Send>;

// Heap-pinned data-channel callback state addressed by the sys `userdata`.
#[derive(Default)]
struct DcObserverState {
    on_message: Option<Mutex<MessageCb>>,
    on_state_change: Option<Mutex<StateCb>>,
    on_buffered_amount_low: Option<Mutex<EventCb>>,
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
extern "C" fn dc_on_state_change(ud: *mut c_void, state: c_int) {
    let st = unsafe { &*(ud as *const DcObserverState) };
    if let Some(m) = &st.on_state_change {
        if let Ok(mut cb) = m.lock() {
            cb(DataChannelState::from_raw(state));
        }
    }
}
extern "C" fn dc_on_buffered_amount_low(ud: *mut c_void) {
    let st = unsafe { &*(ud as *const DcObserverState) };
    if let Some(m) = &st.on_buffered_amount_low {
        if let Ok(mut cb) = m.lock() {
            cb();
        }
    }
}

/// A data channel — either locally created or handed to `on_data_channel` by
/// the remote peer. Dropping releases the native handle.
pub struct DataChannel {
    raw: *mut reactor_webrtc_sys::DataChannel,
    // Keeps the callback closures alive while the native observer is registered.
    observer: Option<Box<DcObserverState>>,
    // Keeps the factory's signaling/network threads alive for as long as this
    // channel exists — a caller can detach it and outlive both the connection
    // that created it and the factory that ultimately owns those threads.
    _factory: Arc<FactoryHandle>,
}

// SAFETY: the native data channel is internally thread-safe; callbacks are
// serialized on the network/signaling thread and guarded by mutexes on the
// Rust side.
unsafe impl Send for DataChannel {}
unsafe impl Sync for DataChannel {}

impl DataChannel {
    pub(crate) fn from_raw(
        raw: *mut reactor_webrtc_sys::DataChannel,
        factory: Arc<FactoryHandle>,
    ) -> Self {
        Self {
            raw,
            observer: None,
            _factory: factory,
        }
    }

    /// The label this channel was created with.
    pub fn label(&self) -> String {
        let mut buf = [0u8; 256];
        let n = unsafe {
            reactor_webrtc_sys::reactor_webrtc_data_channel_label(
                self.raw,
                buf.as_mut_ptr() as *mut c_char,
                buf.len() as c_int,
            )
        };
        if n < 0 {
            String::new()
        } else {
            unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Current readiness state.
    pub fn state(&self) -> DataChannelState {
        DataChannelState::from_raw(unsafe {
            reactor_webrtc_sys::reactor_webrtc_data_channel_state(self.raw)
        })
    }

    /// Bytes currently queued for sending (backpressure signal).
    pub fn buffered_amount(&self) -> u64 {
        unsafe { reactor_webrtc_sys::reactor_webrtc_data_channel_buffered_amount(self.raw) }
    }

    /// Set the threshold below which `on_buffered_amount_low` fires.
    pub fn set_buffered_amount_low_threshold(&self, threshold: u64) {
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_data_channel_set_low_threshold(self.raw, threshold)
        }
    }

    /// Send bytes over the channel. `binary` selects the SCTP message type.
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

    /// Receive handler — fires on every incoming message. The closure runs on
    /// a WebRTC network thread; return quickly or offload heavy work.
    pub fn on_message(&mut self, cb: impl for<'a> FnMut(&'a [u8], bool) + Send + 'static) {
        self.observer
            .get_or_insert_with(Default::default)
            .on_message = Some(Mutex::new(Box::new(cb)));
        self.reregister();
    }

    /// State-change handler — fires for every transition including
    /// Connecting → Open → Closing → Closed.
    pub fn on_state_change(&mut self, cb: impl FnMut(DataChannelState) + Send + 'static) {
        self.observer
            .get_or_insert_with(Default::default)
            .on_state_change = Some(Mutex::new(Box::new(cb)));
        self.reregister();
    }

    /// Convenience: fires once when the channel becomes `Open`.
    pub fn on_open(&mut self, cb: impl FnMut() + Send + 'static) {
        let mut cb = cb;
        self.on_state_change(move |s| {
            if s == DataChannelState::Open {
                cb();
            }
        });
    }

    /// Convenience: fires once when the channel reaches `Closed`.
    pub fn on_close(&mut self, cb: impl FnMut() + Send + 'static) {
        let mut cb = cb;
        self.on_state_change(move |s| {
            if s == DataChannelState::Closed {
                cb();
            }
        });
    }

    /// Flow-control handler — fires when `buffered_amount` drops at or below
    /// the threshold set by [`set_buffered_amount_low_threshold`](Self::set_buffered_amount_low_threshold).
    pub fn on_buffered_amount_low(&mut self, cb: impl FnMut() + Send + 'static) {
        self.observer
            .get_or_insert_with(Default::default)
            .on_buffered_amount_low = Some(Mutex::new(Box::new(cb)));
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
                    dc_on_state_change,
                    dc_on_buffered_amount_low,
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

type StatsTx = SyncSender<StatsReport>;

extern "C" fn stats_cb(ud: *mut c_void, entries: *const ReactorStatEntry, count: c_int) {
    let tx = unsafe { &*(ud as *const StatsTx) };
    let slice = if entries.is_null() || count <= 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(entries, count as usize) }
    };
    let mut report = StatsReport::default();
    for e in slice {
        match e.kind {
            0 => report.inbound_rtp.push(InboundRtpStats {
                ssrc: e.ssrc,
                packets_received: e.packets_received,
                bytes_received: e.bytes_received,
                jitter_s: e.jitter,
                packets_lost: e.packets_lost,
                nack_count: e.nack_count,
                total_decode_time_s: e.total_decode_time,
            }),
            1 => report.outbound_rtp.push(OutboundRtpStats {
                ssrc: e.ssrc,
                packets_sent: e.packets_sent,
                bytes_sent: e.bytes_sent,
                target_bitrate_bps: e.target_bitrate,
                round_trip_time_s: e.round_trip_time,
                retransmitted_packets_sent: e.retransmitted_packets_sent,
            }),
            2 => report.candidate_pairs.push(IceCandidatePairStats {
                current_round_trip_time_s: e.current_round_trip_time,
                priority: e.priority,
                state: IceCandidatePairState::from_raw(e.pair_state),
            }),
            _ => {}
        }
    }
    let _ = tx.try_send(report);
}

fn run_stats(call: impl FnOnce(*mut c_void)) -> Result<StatsReport> {
    let (tx, rx) = sync_channel::<StatsReport>(1);
    let p = Box::into_raw(Box::new(tx));
    call(p as *mut c_void);
    let r = rx.recv_timeout(OP_TIMEOUT);
    drop(unsafe { Box::from_raw(p) });
    r.map_err(|_| Error::Webrtc("get_stats timed out".into()))
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
    // Keeps the factory's signaling/worker/network threads alive for as long
    // as this connection exists — destroying it dispatches onto them, so it
    // must not be destroyed after they are.
    _factory: Arc<FactoryHandle>,
    // Opened by set_remote_description when the remote declares
    // FRAME_METADATA_URI; read per frame by the sender metadata transforms.
    frame_metadata_gate: crate::metadata::FrameMetadataGate,
}

// SAFETY: the native peer connection is internally thread-safe; observer
// callbacks are serialized on the signaling thread and guarded by mutexes.
unsafe impl Send for PeerConnection {}
unsafe impl Sync for PeerConnection {}

impl PeerConnection {
    pub(crate) fn new(
        raw: *mut reactor_webrtc_sys::PeerConnection,
        observer: Box<ObserverState>,
        factory: Arc<FactoryHandle>,
    ) -> Self {
        Self {
            raw,
            _observer: observer,
            _factory: factory,
            frame_metadata_gate: crate::metadata::FrameMetadataGate::new(),
        }
    }

    // ── Signaling (blocking on the native callback) ──────────────────────────

    /// Create an offer.
    ///
    /// Every offer advertises frame-metadata support
    /// ([`FRAME_METADATA_URI`](crate::metadata::FRAME_METADATA_URI)) on each video
    /// m-section, because this crate does support it. A peer that does not
    /// understand the URI ignores the line — RFC 8866 requires unrecognised
    /// attributes to be ignored — and libwebrtc in particular parses it, declines
    /// to map it to any header extension, and emits no bytes for it.
    ///
    /// The declaration is what lets the answerer tell us it strips trailers, which
    /// is what opens this connection's
    /// [`FrameMetadataGate`](crate::FrameMetadataGate).
    pub fn create_offer(&self) -> Result<SessionDescription> {
        let offer = run_sdp(|ud| unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create_offer(
                self.raw, ud, sdp_ok, sdp_err,
            )
        })?;
        Ok(Self::advertise_frame_metadata(offer, None))
    }

    /// Create an answer.
    ///
    /// Mirrors the offer on frame metadata: the capability is declared, under the
    /// offer's own extmap id, only when the offer declared it. Introducing it in
    /// an answer that was not offered it is not something offer/answer can
    /// express, so a silent offer produces a silent answer and the gate stays
    /// closed in both directions.
    ///
    /// Requires [`set_remote_description`](Self::set_remote_description) to have
    /// been called with the offer first, which is already the only valid order.
    pub fn create_answer(&self) -> Result<SessionDescription> {
        let answer = run_sdp(|ud| unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create_answer(
                self.raw, ud, sdp_ok, sdp_err,
            )
        })?;
        match self.frame_metadata_gate.extmap_id() {
            Some(id) => Ok(Self::advertise_frame_metadata(answer, Some(id))),
            None => Ok(answer),
        }
    }

    /// Declare frame-metadata support on `sdp`, or return it untouched.
    ///
    /// Failure is never propagated. `with_frame_metadata` errors on a description
    /// with no video m-section, or one where every one-byte extmap id is already
    /// spoken for — neither is a reason to fail the caller's offer or answer. The
    /// capability simply goes unadvertised and the gate stays closed, which is the
    /// same outcome as talking to a peer that never supported it.
    fn advertise_frame_metadata(sdp: SessionDescription, id: Option<u16>) -> SessionDescription {
        let declared = match id {
            Some(id) => sdp.with_frame_metadata_id(id),
            None => sdp.with_frame_metadata(),
        };
        declared.unwrap_or(sdp)
    }
    pub fn set_local_description(&self, sdp: &SessionDescription) -> Result<()> {
        self.set_description(sdp, true)
    }
    /// Apply the remote description, and arm this connection's
    /// [`FrameMetadataGate`](crate::FrameMetadataGate) from it.
    ///
    /// The gate opens when `sdp` declares
    /// [`FRAME_METADATA_URI`](crate::metadata::FRAME_METADATA_URI) and closes
    /// when it does not, on every call — so a renegotiation in which the peer
    /// drops support closes it again.
    ///
    /// On an answerer this runs before [`create_answer`](Self::create_answer),
    /// which is what lets the answer echo the offer's extmap id.
    pub fn set_remote_description(&self, sdp: &SessionDescription) -> Result<()> {
        self.set_description(sdp, false)?;
        // After the native call, not before: a description libwebrtc rejected was
        // never applied, and must not move the gate.
        self.frame_metadata_gate.set(sdp.frame_metadata_id());
        Ok(())
    }

    /// This connection's frame-metadata gate: what the remote peer declared.
    ///
    /// Cloneable and cheap. Reading it is diagnostic — the library already
    /// consults it when answering and when appending trailers, so a caller does
    /// not need to. It stays closed until
    /// [`set_remote_description`](Self::set_remote_description) sees a remote
    /// description that declares support.
    pub fn frame_metadata_gate(&self) -> crate::metadata::FrameMetadataGate {
        self.frame_metadata_gate.clone()
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
            Ok(DataChannel::from_raw(raw, Arc::clone(&self._factory)))
        }
    }

    // ── Stats ────────────────────────────────────────────────────────────────

    /// Collect a stats snapshot from this peer connection.
    ///
    /// Blocks the current thread (up to [`OP_TIMEOUT`]) until the WebRTC
    /// engine delivers the report. The returned [`StatsReport`] contains only
    /// the three stat types surfaced through the C ABI:
    ///
    /// - [`StatsReport::inbound_rtp`] — per-SSRC receive statistics
    ///   (packets, jitter, NACK count, decode time).
    /// - [`StatsReport::outbound_rtp`] — per-SSRC send statistics
    ///   (bytes sent, target bitrate, RTT).
    /// - [`StatsReport::candidate_pairs`] — ICE candidate pair state and RTT.
    pub fn get_stats(&self) -> Result<StatsReport> {
        run_stats(|ud| unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_get_stats(self.raw, ud, stats_cb)
        })
    }

    /// Set aggregate bitrate limits on the peer connection.
    ///
    /// Each parameter is optional; pass `None` to keep the libwebrtc default
    /// for that field. All values are in bits per second.
    ///
    /// # Parameters
    ///
    /// - `min_bps` — floor handed to the congestion controller; it will not
    ///   drop below this even when the network estimate is very low.
    /// - `start_bps` — initial encoder target. libwebrtc's built-in default
    ///   is ~300 kbps, which causes a visible quality ramp-up on new
    ///   connections. Set this close to your expected steady-state bitrate
    ///   (e.g. `Some(4_000_000)` for a 4 Mbps stream) to reach quality
    ///   quickly.
    /// - `max_bps` — ceiling; the GCC algorithm will not allocate above this.
    ///
    /// Can be called at any time after the peer connection is created,
    /// including after negotiation.
    pub fn set_bitrate(
        &self,
        min_bps: Option<i32>,
        start_bps: Option<i32>,
        max_bps: Option<i32>,
    ) -> crate::Result<()> {
        let mut err = [0 as std::os::raw::c_char; 256];
        let rc = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_set_bitrate(
                self.raw,
                min_bps.unwrap_or(-1),
                start_bps.unwrap_or(-1),
                max_bps.unwrap_or(-1),
                err.as_mut_ptr(),
                err.len() as std::os::raw::c_int,
            )
        };
        if rc != 0 {
            let reason = unsafe { std::ffi::CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(crate::Error::Webrtc(if reason.is_empty() {
                "set_bitrate failed".into()
            } else {
                reason
            }));
        }
        Ok(())
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        // Destroy the native PC (stops callbacks) before the observer box drops.
        unsafe { reactor_webrtc_sys::reactor_webrtc_peer_connection_destroy(self.raw) }
    }
}

#[cfg(test)]
mod sdp_ice_credentials_tests {
    use super::*;

    /// A two-section bundled description, as libwebrtc emits one.
    fn bundled() -> SessionDescription {
        SessionDescription {
            kind: SdpType::Answer,
            sdp: concat!(
                "v=0\r\n",
                "o=- 1 2 IN IP4 127.0.0.1\r\n",
                "s=-\r\n",
                "t=0 0\r\n",
                "a=group:BUNDLE 0 1\r\n",
                "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n",
                "a=mid:0\r\n",
                "a=ice-ufrag:jHFv\r\n",
                "a=ice-pwd:0123456789012345678901\r\n",
                "a=fingerprint:sha-256 AA:BB\r\n",
                "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
                "a=mid:1\r\n",
                "a=ice-ufrag:jHFv\r\n",
                "a=ice-pwd:0123456789012345678901\r\n",
                "a=fingerprint:sha-256 AA:BB\r\n",
            )
            .to_string(),
        }
    }

    const UFRAG: &str = "CgAHFcpsamqt/IIl8YtGLBP8al/dIA";
    const PWD: &str = "iMyV3ZlbyUC8SBiy/AeG2OVaSJ5di54s";

    #[test]
    fn replaces_every_section_not_just_the_first() {
        // Replacing one and leaving the other would produce an SDP that is
        // inconsistent rather than substituted, and bundled sections must agree.
        let out = bundled().with_ice_credentials(UFRAG, PWD).unwrap();
        assert_eq!(out.ice_ufrags(), vec![UFRAG, UFRAG]);
        assert_eq!(out.sdp.matches(&format!("a=ice-pwd:{PWD}")).count(), 2);
        assert!(!out.sdp.contains("jHFv"));
    }

    #[test]
    fn leaves_everything_else_byte_for_byte() {
        let out = bundled().with_ice_credentials(UFRAG, PWD).unwrap();
        for keep in [
            "a=group:BUNDLE 0 1",
            "a=fingerprint:sha-256 AA:BB",
            "m=video 9 UDP/TLS/RTP/SAVPF 96",
            "a=mid:1",
        ] {
            assert!(out.sdp.contains(keep), "lost {keep:?}");
        }
        assert_eq!(out.sdp.lines().count(), bundled().sdp.lines().count());
        assert_eq!(out.kind, bundled().kind);
    }

    #[test]
    fn the_fingerprint_survives_untouched() {
        // Load-bearing: DTLS is what keeps a relay out of the media, and it is
        // authenticated by this line. Rewriting it would silently break end-to-end
        // encryption rather than fail loudly.
        let before = bundled();
        let after = before.with_ice_credentials(UFRAG, PWD).unwrap();
        let fp = |s: &SessionDescription| -> Vec<String> {
            s.sdp
                .lines()
                .filter(|l| l.starts_with("a=fingerprint:"))
                .map(str::to_string)
                .collect()
        };
        assert_eq!(fp(&before), fp(&after));
    }

    #[test]
    fn output_is_crlf_terminated() {
        let out = bundled().with_ice_credentials(UFRAG, PWD).unwrap();
        assert!(out.sdp.ends_with("\r\n"));
        assert_eq!(
            out.sdp.matches('\n').count(),
            out.sdp.matches("\r\n").count()
        );
    }

    #[test]
    fn normalises_bare_lf_input_to_crlf() {
        let lf = SessionDescription {
            kind: SdpType::Offer,
            sdp: "v=0\na=ice-ufrag:jHFv\na=ice-pwd:0123456789012345678901\n".into(),
        };
        let out = lf.with_ice_credentials(UFRAG, PWD).unwrap();
        assert_eq!(out.sdp.matches("\r\n").count(), 3);
    }

    #[test]
    fn rejects_a_ufrag_that_is_too_short_or_too_long() {
        let d = bundled();
        assert!(d.with_ice_credentials("abc", PWD).is_err());
        assert!(d.with_ice_credentials(&"a".repeat(257), PWD).is_err());
        assert!(d.with_ice_credentials(&"a".repeat(4), PWD).is_ok());
        assert!(d.with_ice_credentials(&"a".repeat(256), PWD).is_ok());
    }

    #[test]
    fn rejects_a_password_below_the_rfc_minimum() {
        let d = bundled();
        assert!(d.with_ice_credentials(UFRAG, &"a".repeat(21)).is_err());
        assert!(d.with_ice_credentials(UFRAG, &"a".repeat(22)).is_ok());
    }

    #[test]
    fn rejects_characters_outside_ice_char() {
        // Rejecting here keeps the failure attributable. Passed through, these
        // surface much later as a generic invalid-parameter error from libwebrtc.
        let d = bundled();
        for bad in [
            "has space",
            "has=equals",
            "has\r\ninjected:line",
            "acentuação",
        ] {
            assert!(
                d.with_ice_credentials(bad, PWD).is_err(),
                "{bad:?} was accepted as a ufrag"
            );
        }
        assert!(d
            .with_ice_credentials(UFRAG, "short but has spaces!!")
            .is_err());
    }

    #[test]
    fn a_crlf_in_a_credential_cannot_inject_an_sdp_line() {
        // The alphabet check is what prevents this, and it is worth asserting
        // directly rather than trusting it as a side effect.
        let d = bundled();
        assert!(d
            .with_ice_credentials("aaaa\r\na=candidate:injected", PWD)
            .is_err());
    }

    #[test]
    fn refuses_a_description_with_no_credentials_to_replace() {
        let empty = SessionDescription {
            kind: SdpType::Offer,
            sdp: "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n".into(),
        };
        assert!(empty.with_ice_credentials(UFRAG, PWD).is_err());
    }

    #[test]
    fn ice_ufrags_reads_each_section() {
        assert_eq!(bundled().ice_ufrags(), vec!["jHFv", "jHFv"]);
        let none = SessionDescription {
            kind: SdpType::Offer,
            sdp: "v=0\r\n".into(),
        };
        assert!(none.ice_ufrags().is_empty());
    }
}

#[cfg(test)]
mod sdp_frame_metadata_tests {
    use super::*;
    use crate::metadata::FRAME_METADATA_URI;

    /// A bundled audio+video description, as libwebrtc emits one, with `extra`
    /// spliced into the video section's attribute block.
    fn described(extra: &str) -> SessionDescription {
        SessionDescription {
            kind: SdpType::Offer,
            sdp: format!(
                "v=0\r\n\
                 o=- 1 2 IN IP4 127.0.0.1\r\n\
                 s=-\r\n\
                 t=0 0\r\n\
                 a=group:BUNDLE 0 1\r\n\
                 m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                 c=IN IP4 0.0.0.0\r\n\
                 a=mid:0\r\n\
                 a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
                 m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                 c=IN IP4 0.0.0.0\r\n\
                 b=AS:2000\r\n\
                 a=mid:1\r\n\
                 {extra}\
                 a=fingerprint:sha-256 AA:BB\r\n"
            ),
        }
    }

    fn bundled() -> SessionDescription {
        described("")
    }

    /// Every `a=extmap` declaring our URI, as `(id, mid)` pairs in document order.
    fn declarations(sdp: &str) -> Vec<(u16, String)> {
        let mut out = Vec::new();
        let mut mid = String::new();
        for line in sdp.lines() {
            if let Some(v) = line.strip_prefix("a=mid:") {
                mid = v.to_string();
            } else if let Some((id, uri)) = parse_extmap(line) {
                if uri == FRAME_METADATA_URI {
                    out.push((id, mid.clone()));
                }
            }
        }
        out
    }

    #[test]
    fn declares_on_the_video_section_only() {
        let out = bundled().with_frame_metadata().unwrap();
        assert_eq!(declarations(&out.sdp), vec![(2, "1".to_string())]);
        assert_eq!(out.frame_metadata_id(), Some(2));
        assert!(out.declares_frame_metadata());
    }

    #[test]
    fn skips_ids_already_in_use() {
        // id 1 is taken by ssrc-audio-level in the audio section. Bundle-wide,
        // not per-section: RFC 8843 makes an id mean one URI across the group, and
        // the peer rejects a description that breaks it.
        assert_eq!(
            bundled().with_frame_metadata().unwrap().frame_metadata_id(),
            Some(2)
        );

        let crowded = described(concat!(
            "a=extmap:2 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
            "a=extmap:3 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n",
        ));
        assert_eq!(
            crowded.with_frame_metadata().unwrap().frame_metadata_id(),
            Some(4)
        );
    }

    #[test]
    fn refuses_when_every_one_byte_id_is_taken() {
        // Never silently reach into the two-byte range: those ids only work when
        // a=extmap-allow-mixed was negotiated, which we cannot see from here.
        let taken: String = (1..=EXTMAP_ONE_BYTE_MAX_ID)
            .map(|id| format!("a=extmap:{id} urn:example:ext:{id}\r\n"))
            .collect();
        let err = described(&taken).with_frame_metadata().unwrap_err();
        assert!(err.to_string().contains("is taken"), "{err}");
    }

    #[test]
    fn declares_the_same_id_in_every_video_section() {
        // One id, one URI, across the whole bundle group — otherwise the peer's
        // ValidateBundledRtpHeaderExtensions rejects what we signalled.
        let two_videos = SessionDescription {
            kind: SdpType::Offer,
            sdp: concat!(
                "v=0\r\n",
                "a=group:BUNDLE 0 1\r\n",
                "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
                "a=mid:0\r\n",
                "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
                "a=mid:1\r\n",
            )
            .to_string(),
        };
        let out = two_videos.with_frame_metadata().unwrap();
        assert_eq!(
            declarations(&out.sdp),
            vec![(1, "0".to_string()), (1, "1".to_string())]
        );
    }

    #[test]
    fn inserted_after_the_mid_never_before_c_or_b() {
        // RFC 8866 §5 orders an m-section as m=, i=, c=, b=, k=, then a=.
        let out = bundled().with_frame_metadata().unwrap();
        let lines: Vec<&str> = out.sdp.lines().collect();
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
        assert!(at("c=IN IP4 0.0.0.0") < at(FRAME_METADATA_URI));
        assert!(at("b=AS:2000") < at(FRAME_METADATA_URI));
        assert_eq!(at("a=mid:1") + 1, at(FRAME_METADATA_URI));
    }

    #[test]
    fn answer_echoes_the_offers_id() {
        let offer = described("a=extmap:7 urn:ietf:params:rtp-hdrext:sdes:mid\r\n")
            .with_frame_metadata()
            .unwrap();
        let id = offer.frame_metadata_id().unwrap();
        assert_eq!(id, 2); // 1 is ssrc-audio-level and 7 is sdes:mid, so 2 is lowest free

        let answer = bundled().with_frame_metadata_id(id).unwrap();
        assert_eq!(answer.frame_metadata_id(), Some(id));
    }

    #[test]
    fn refuses_an_id_bound_to_another_uri() {
        // Signalling a collision would surface as the *peer's*
        // set_remote_description failing, which is far harder to attribute.
        let err = bundled().with_frame_metadata_id(1).unwrap_err();
        assert!(err.to_string().contains("already bound to"), "{err}");
        assert!(err.to_string().contains("ssrc-audio-level"), "{err}");
    }

    #[test]
    fn rejects_out_of_range_ids() {
        for bad in [0, EXTMAP_MAX_ID + 1, 1000] {
            assert!(
                bundled().with_frame_metadata_id(bad).is_err(),
                "accepted extmap id {bad}"
            );
        }
        assert!(bundled().with_frame_metadata_id(EXTMAP_MAX_ID).is_ok());
    }

    #[test]
    fn is_idempotent() {
        let once = bundled().with_frame_metadata().unwrap();
        let twice = once.with_frame_metadata().unwrap();
        assert_eq!(once.sdp, twice.sdp);
        assert_eq!(
            twice
                .with_frame_metadata_id(once.frame_metadata_id().unwrap())
                .unwrap()
                .sdp,
            once.sdp
        );
    }

    #[test]
    fn refuses_a_second_declaration_under_a_different_id() {
        let once = bundled().with_frame_metadata().unwrap();
        let other = once.frame_metadata_id().unwrap() + 1;
        let err = once.with_frame_metadata_id(other).unwrap_err();
        assert!(err.to_string().contains("already declares"), "{err}");
    }

    #[test]
    fn a_different_uri_version_reads_as_unsupported() {
        // The URI is the wire version. A peer speaking a future trailer format
        // must not look like a peer speaking this one.
        let future = described(&format!("a=extmap:5 {FRAME_METADATA_URI}-2\r\n"));
        assert_eq!(future.frame_metadata_id(), None);
        assert!(!future.declares_frame_metadata());
        // …and declaring ours alongside it works, on a free id.
        let out = future.with_frame_metadata().unwrap();
        assert_eq!(out.frame_metadata_id(), Some(2));
    }

    #[test]
    fn errors_without_a_video_section() {
        let audio_only = SessionDescription {
            kind: SdpType::Offer,
            sdp: "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n".into(),
        };
        let err = audio_only.with_frame_metadata().unwrap_err();
        assert!(err.to_string().contains("no video m-section"), "{err}");
    }

    #[test]
    fn output_is_crlf_terminated() {
        let out = bundled().with_frame_metadata().unwrap();
        assert!(out.sdp.ends_with("\r\n"));
        assert_eq!(
            out.sdp.matches('\n').count(),
            out.sdp.matches("\r\n").count()
        );
        assert_eq!(out.kind, bundled().kind);
    }

    #[test]
    fn parses_a_direction_qualified_extmap() {
        // a=extmap:<id>["/"<direction>] <URI> — the direction is optional and we
        // emit none, but a peer may send one and its id still has to be seen.
        assert_eq!(
            parse_extmap("a=extmap:9/sendonly urn:example:ext"),
            Some((9, "urn:example:ext"))
        );
        assert_eq!(
            parse_extmap("a=extmap:9 urn:example:ext extra=params"),
            Some((9, "urn:example:ext"))
        );
        assert_eq!(parse_extmap("a=extmap:0 urn:example:ext"), None);
        assert_eq!(parse_extmap("a=extmap:256 urn:example:ext"), None);
        assert_eq!(parse_extmap("a=mid:0"), None);
    }

    #[test]
    fn declarations_are_read_back_from_a_direction_qualified_line() {
        let manual = described(&format!("a=extmap:4/sendrecv {FRAME_METADATA_URI}\r\n"));
        assert_eq!(manual.frame_metadata_id(), Some(4));
    }
}

#[cfg(test)]
mod frame_metadata_gate_tests {
    use crate::metadata::FrameMetadataGate;

    #[test]
    fn starts_closed() {
        // Closed-by-default is the safe direction: a sender that has not yet
        // applied a remote description must not append trailers.
        let gate = FrameMetadataGate::new();
        assert!(!gate.is_open());
        assert_eq!(gate.extmap_id(), None);
    }

    #[test]
    fn carries_the_negotiated_id() {
        // create_answer echoes this id, so losing it would mean answering with a
        // freshly allocated one that disagrees with the offer.
        let gate = FrameMetadataGate::new();
        gate.set(Some(7));
        assert!(gate.is_open());
        assert_eq!(gate.extmap_id(), Some(7));
    }

    #[test]
    fn clones_share_one_state() {
        let gate = FrameMetadataGate::new();
        let handed_to_transform = gate.clone();
        gate.set(Some(3));
        assert!(handed_to_transform.is_open());
        assert_eq!(handed_to_transform.extmap_id(), Some(3));
        // A renegotiation where the peer drops support closes it again.
        gate.set(None);
        assert!(!handed_to_transform.is_open());
    }
}
