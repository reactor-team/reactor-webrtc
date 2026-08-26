//! The peer connection and its associated signaling/data types.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reactor_webrtc_sys::ReactorStatEntry;

use crate::encoded::{FrameTransform, VideoCodec};
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

    /// Whether this description declares frame-metadata support.
    ///
    /// True when it carries a session-level
    /// `a=x-reactor-frame-metadata:<version>` whose version this build understands
    /// ([`FRAME_METADATA_VERSION`](crate::metadata::FRAME_METADATA_VERSION)). A peer
    /// speaking a different trailer format therefore reads as unsupported rather
    /// than as a partial match.
    ///
    /// Read off the SDP string, not from libwebrtc: it drops `a=` lines it does not
    /// recognise when parsing, so the parsed description never carries this.
    ///
    /// This is what [`PeerConnection::set_remote_description`] arms the connection's
    /// [`FrameMetadataGate`](crate::FrameMetadataGate) from.
    pub fn declares_frame_metadata(&self) -> bool {
        let prefix = format!("a={}:", crate::metadata::FRAME_METADATA_ATTRIBUTE);
        self.sdp.lines().any(|line| {
            line.strip_prefix(prefix.as_str())
                .and_then(|v| v.trim_end().parse::<u32>().ok())
                .is_some_and(|v| v == crate::metadata::FRAME_METADATA_VERSION)
        })
    }

    /// Return a copy declaring frame-metadata support, as a session-level attribute.
    ///
    /// [`create_offer`](PeerConnection::create_offer) already applies this to every
    /// offer, and [`create_answer`](PeerConnection::create_answer) mirrors the offer
    /// — so a caller using this crate's signalling path never needs it. It is public
    /// for callers that assemble or rewrite descriptions themselves.
    ///
    /// Idempotent: a description that already declares the capability comes back
    /// unchanged, as does one with no lines at all.
    ///
    /// # Why a bespoke attribute
    ///
    /// The declaration says only "this peer understands the trailer". An
    /// `a=extmap` would have been the recognisable spelling, but it means "I will
    /// send this RTP header extension", which is not true — no header extension is
    /// ever emitted — and it would drag in a shared id namespace that the *peer*
    /// validates (RFC 8843 requires one id to mean one URI across a BUNDLE group),
    /// so a collision would surface as the far side's `set_remote_description`
    /// failing. An unregistered `x-` attribute claims nothing false and has no id
    /// to collide.
    ///
    /// The cost is that libwebrtc discards it while parsing, so it is only ever
    /// readable from the SDP string. Nothing here depends on the parsed form.
    ///
    /// # Placement
    ///
    /// Inserted immediately before the first `m=` line, which is the end of the
    /// session section: RFC 8866 §5 puts session-level attributes after `t=`/`z=`/`k=`
    /// and before the first media description, and everything preceding the first
    /// `m=` is by definition session level.
    pub fn with_frame_metadata(&self) -> Self {
        if self.declares_frame_metadata() || self.sdp.lines().next().is_none() {
            return self.clone();
        }
        let declaration = format!(
            "a={}:{}\r\n",
            crate::metadata::FRAME_METADATA_ATTRIBUTE,
            crate::metadata::FRAME_METADATA_VERSION
        );
        let mut out = String::with_capacity(self.sdp.len() + declaration.len());
        let mut inserted = false;
        for line in self.sdp.lines() {
            if !inserted && line.starts_with("m=") {
                out.push_str(&declaration);
                inserted = true;
            }
            out.push_str(line);
            out.push_str("\r\n");
        }
        // A description with no media section at all: session level is still session
        // level, so it goes at the end.
        if !inserted {
            out.push_str(&declaration);
        }
        Self {
            kind: self.kind,
            sdp: out,
        }
    }
}

/// A trickled ICE candidate.
///
/// An empty [`IceCandidate::candidate`] string is the end-of-candidates
/// marker (RFC 8838), not a candidate to parse; `sdp_mid` and
/// `sdp_mline_index` still identify the m-line it ends.
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
    pc_id: usize,
    // Shared with the owning PeerConnection: what negotiation concluded about
    // frame metadata. Consulted by set_track when replacing a disallowed
    // track with an allowed one.
    frame_metadata_gate: crate::FrameMetadataGate,
}

// SAFETY: the native transceiver is internally thread-safe.
unsafe impl Send for Transceiver {}
unsafe impl Sync for Transceiver {}

impl Transceiver {
    pub(crate) fn from_raw(
        raw: *mut reactor_webrtc_sys::RtpTransceiver,
        pc_id: usize,
        gate: crate::FrameMetadataGate,
    ) -> Self {
        Self {
            raw,
            pc_id,
            frame_metadata_gate: gate,
        }
    }

    /// The transceiver's media kind (audio/video).
    /// Identity of the transceiver itself, as an opaque value.
    ///
    /// Stable for the transceiver's life, unlike the handle pointer — `transceivers()`
    /// allocates a fresh handle each call. Usable as a key before a track is attached
    /// and before a mid is assigned.
    pub(crate) fn transceiver_id(&self) -> usize {
        unsafe { reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_id(self.raw) }
    }

    /// Identity of the track on this transceiver's sender, as an opaque value.
    ///
    /// Only ever compared — it is how the crate recognises which of its own
    /// [`Track`](crate::Track)s a transceiver is sending, so that state living in
    /// that track can be found from here. 0 when the sender has no track.
    pub(crate) fn sender_track_id(&self) -> usize {
        unsafe { reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_sender_track_id(self.raw) }
    }

    /// Identity of the track this transceiver's receiver delivers, on the same
    /// terms as [`sender_track_id`](Self::sender_track_id). Non-zero once the
    /// remote description has been applied.
    pub(crate) fn receiver_track_id(&self) -> usize {
        unsafe { reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_receiver_track_id(self.raw) }
    }

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
    ///
    /// The track is published in the same MediaStream as every other track this
    /// peer sends, the way [`PeerConnection::add_track`] publishes one. The
    /// remote groups the streams it receives by that id, so an audio track and a
    /// video track published here play out in sync with each other.
    pub fn set_track(&self, track: &Track) -> Result<()> {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_track(self.raw, track.raw())
        };
        if ok == 1 {
            let id = self.transceiver_id();
            let sender_id = self.sender_track_id();
            // Re-wire the embed source when the gate is already open (replaceTrack
            // post-negotiation). Without this the old track's source stays in the
            // slot and pushes to the new track are silently dropped until the next
            // renegotiation re-runs install_frame_metadata_transforms.
            if let Some(source) = crate::sender_meta::lookup(sender_id) {
                crate::sender_meta::update_embed_source(self.pc_id, id, source);
            }
            // Fresh embed only when both the negotiated gate is open and the new
            // track is allowed to carry metadata — the mirror for, e.g., replacing
            // a track created with frame_metadata off by one with it on. If the slot
            // already runs an embed step this is a no-op rather than a second
            // transformer.
            if !self.frame_metadata_gate.is_open() || !crate::sender_meta::allowed(sender_id) {
            } else if let Some(source) = crate::sender_meta::lookup(sender_id) {
                if let Some(native) = crate::sender_meta::attach_embed(
                    self.pc_id,
                    id,
                    source,
                    self.frame_metadata_gate.clone(),
                ) {
                    if self
                        .attach_native_transform(crate::sender_meta::Side::Send, &native)
                        .is_err()
                    {
                        crate::sender_meta::release_install(
                            self.pc_id,
                            id,
                            crate::sender_meta::Side::Send,
                        );
                    }
                }
            }
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

    /// Reorder this video transceiver's codec preferences: `codecs`, most
    /// preferred first, sort ahead of every other codec the endpoint
    /// supports. Mirrors [`RTCRtpTransceiver.setCodecPreferences`](
    /// https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiver-setcodecpreferences),
    /// plus one behavior the browser API does not need: once negotiation
    /// completes, [`PeerConnection::set_local_description`] and
    /// [`PeerConnection::set_remote_description`] also make this
    /// transceiver's own sender actually *encode* with whichever preferred
    /// codec was negotiated, not just list it first in the SDP. Without
    /// that, a fresh sender follows the remote offer's own codec order
    /// regardless of what got negotiated — libwebrtc's SDP negotiation and
    /// its sender codec selection are two separate mechanisms, and only the
    /// first one is driven by preference order. See
    /// [`try_lock_negotiated_send_codec`](Self::try_lock_negotiated_send_codec).
    ///
    /// Nothing is dropped: a codec left out of `codecs`, and every
    /// retransmission/RED/FEC entry, keeps its original relative order after
    /// the preferred ones — retransmission stays associated with its codec,
    /// and the peer that doesn't support a preferred codec still gets an
    /// offer/answer it can negotiate against. A codec named in `codecs` that
    /// this endpoint does not actually support is silently ignored rather
    /// than treated as an error.
    ///
    /// Takes effect on the next [`PeerConnection::create_offer`] or
    /// [`PeerConnection::create_answer`] for this transceiver's m-section —
    /// call it before negotiating. Returns an error if this transceiver
    /// carries audio, not video.
    pub fn set_codec_preferences(&self, codecs: &[VideoCodec]) -> Result<()> {
        let names: Vec<CString> = codecs
            .iter()
            .map(|c| CString::new(c.name()).expect("codec name is a static ASCII string"))
            .collect();
        let ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_video_codec_preferences(
                self.raw,
                ptrs.as_ptr(),
                ptrs.len() as c_int,
            )
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc(
                "transceiver set_codec_preferences failed (not a video transceiver?)".into(),
            ))
        }
    }

    /// Best-effort counterpart to [`set_codec_preferences`](Self::set_codec_preferences):
    /// make this transceiver's sender actually encode with the codec
    /// `set_codec_preferences` put first, instead of whatever it would
    /// otherwise pick (e.g. the remote offer's own codec order).
    /// `set_codec_preferences` only controls SDP negotiation; it does not by
    /// itself change which negotiated codec an existing sender encodes
    /// with — that is libwebrtc's separate "codec switching" mechanism.
    ///
    /// Not public: [`PeerConnection::set_local_description`] and
    /// [`PeerConnection::set_remote_description`] call this on every video
    /// transceiver after applying the description, so callers only ever
    /// need `set_codec_preferences`. Returns `false` rather than erroring
    /// when there is nothing to do yet — no preference was set, there is no
    /// sender, or negotiation has not completed on this side yet — since
    /// whichever of the two description calls comes second on either role
    /// (offerer or answerer) is the one that finds a completed negotiation.
    pub(crate) fn try_lock_negotiated_send_codec(&self) -> bool {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_lock_negotiated_send_codec(self.raw)
        };
        ok == 1
    }

    /// Attach an encoded-frame transform to this transceiver's **sender**
    /// (encoder → packetizer): observe/replace/drop each encoded frame before
    /// it is sent. See [`crate::FrameTransform`].
    ///
    /// Composes rather than replaces. The crate owns libwebrtc's single
    /// `SetFrameTransformer` slot per sender and runs both this callback and the
    /// frame-metadata step under it, so encoded-frame access and per-frame metadata
    /// work on the same transceiver. The callback runs first, before any trailer is
    /// appended, so it sees exactly the bytes the encoder produced.
    ///
    /// Calling this again replaces the callback. The `FrameTransform` may be dropped
    /// afterwards — the registration holds its own reference.
    pub fn set_sender_transform(&self, transform: &FrameTransform) -> Result<()> {
        self.attach_caller_transform(crate::sender_meta::Side::Send, transform)
    }

    /// Attach an encoded-frame transform to this transceiver's **receiver**
    /// (depacketizer → decoder): observe each encoded frame before decode, and
    /// [`FrameAction::Drop`](crate::FrameAction) to bypass the decoder. See
    /// [`crate::FrameTransform`].
    ///
    /// Composes rather than replaces, as on the sender. The callback runs before the
    /// metadata trailer is stripped, so it sees exactly the bytes that arrived; call
    /// [`decode_and_strip_trailer`](crate::metadata::decode_and_strip_trailer)
    /// yourself if you want the payload without the framing.
    pub fn set_receiver_transform(&self, transform: &FrameTransform) -> Result<()> {
        self.attach_caller_transform(crate::sender_meta::Side::Receive, transform)
    }

    fn attach_caller_transform(
        &self,
        side: crate::sender_meta::Side,
        transform: &FrameTransform,
    ) -> Result<()> {
        let id = self.transceiver_id();
        let Some(native) =
            crate::sender_meta::attach_caller(self.pc_id, id, side, transform.callback())
        else {
            // Either the transformer is already attached — the registration above is
            // all that was needed — or this transceiver has no native identity, in
            // which case there is nothing to attach it to.
            return if id == 0 {
                Err(Error::Webrtc(
                    "transceiver has no native identity to attach a transform to".into(),
                ))
            } else {
                Ok(())
            };
        };
        let result = self.attach_native_transform(side, &native);
        if result.is_err() {
            // The slot was claimed but the native attach failed: un-claim it so
            // a retry can install a new transformer rather than seeing installed=true
            // and silently doing nothing.
            crate::sender_meta::release_install(self.pc_id, id, side);
        }
        result
    }

    /// Attach the crate-owned composed transformer to one side.
    ///
    /// Dropping `native` afterwards is safe and is what the callers do: the native
    /// transformer owns its callback state and the sender/receiver holds a reference
    /// to it (see the [`crate::FrameTransform`] docs).
    pub(crate) fn attach_native_transform(
        &self,
        side: crate::sender_meta::Side,
        native: &crate::encoded::NativeTransform,
    ) -> Result<()> {
        let ok = unsafe {
            match side {
                crate::sender_meta::Side::Send => {
                    reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_sender_transform(
                        self.raw,
                        native.raw(),
                    )
                }
                crate::sender_meta::Side::Receive => {
                    reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_receiver_transform(
                        self.raw,
                        native.raw(),
                    )
                }
            }
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc(format!(
                "transceiver set_{}_transform failed",
                match side {
                    crate::sender_meta::Side::Send => "sender",
                    crate::sender_meta::Side::Receive => "receiver",
                }
            )))
        }
    }
}

impl Drop for Transceiver {
    fn drop(&mut self) {
        // Deliberately *not* forgetting this transceiver's composed slots: handles
        // are recreated per `transceivers()` call, so dropping one says nothing
        // about the underlying transceiver going away. The slots are keyed by native
        // identity and released with the peer connection instead.
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
    // Reclaims this callback's own strong ref (see the comment on `run_sdp`
    // for why the caller cannot be the sole owner of the box).
    let tx = unsafe { Arc::from_raw(ud as *const SdpTx) };
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
    let tx = unsafe { Arc::from_raw(ud as *const SdpTx) };
    let msg = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    let _ = tx.try_send(Err(Error::Webrtc(msg)));
}
extern "C" fn complete_cb(ud: *mut c_void, error: *const c_char) {
    let tx = unsafe { Arc::from_raw(ud as *const CompleteTx) };
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
    let tx = unsafe { Arc::from_raw(ud as *const StatsTx) };
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

// `call` dispatches onto a libwebrtc thread that invokes the C callback
// asynchronously; the public API gives no guarantee that thread has finished
// unwinding out of the callback (still touching `tx` inside its own `notify()`
// after `try_send`'s value became visible to `recv_timeout`) by the time this
// function's wait returns. Sharing the box via `Arc` instead of freeing it
// unilaterally here means whichever side — this caller, or the callback —
// finishes last is the one that frees it, closing that use-after-free window.
// Mirrors the AddRef/Release fix applied to StatsCallback on the C++ side.
//
// Two consequences of the `Arc::from_raw` in each callback above, since it's
// the contract those four `extern "C"` fns must honour:
// - Exactly-once delivery is now a *safety* requirement, not just a
//   correctness one: a second `Arc::from_raw` on the same pointer double-frees
//   once both callback-side refs drop. Holds today (`CreateSdpObserver` fires
//   exactly one of `OnSuccess`/`OnFailure`; the `Set*DescObserver`s and
//   `AddIceCandidate`'s completion each fire once; every early-return path in
//   the glue invokes the callback before returning) but isn't enforced by the
//   type system.
// - If a callback is *never* invoked (e.g. an in-flight completion dropped
//   during peer-connection teardown), its ref is never reclaimed and the
//   channel leaks. Deliberate — a leak beats the UAF it replaces — but it
//   does mean "whichever side finishes last frees it" assumes the callback
//   side eventually runs at all.

fn run_stats(call: impl FnOnce(*mut c_void)) -> Result<StatsReport> {
    let (tx, rx) = sync_channel::<StatsReport>(1);
    let tx = Arc::new(tx);
    let p = Arc::into_raw(tx.clone());
    call(p as *mut c_void);
    let r = rx.recv_timeout(OP_TIMEOUT);
    drop(tx);
    r.map_err(|_| Error::Webrtc("get_stats timed out".into()))
}

fn run_sdp(call: impl FnOnce(*mut c_void)) -> Result<SessionDescription> {
    let (tx, rx) = sync_channel::<Result<SessionDescription>>(1);
    let tx = Arc::new(tx);
    let p = Arc::into_raw(tx.clone());
    call(p as *mut c_void);
    let r = rx.recv_timeout(OP_TIMEOUT);
    drop(tx);
    r.map_err(|_| Error::Webrtc("sdp operation timed out".into()))?
}

fn run_complete(call: impl FnOnce(*mut c_void)) -> Result<()> {
    let (tx, rx) = sync_channel::<Result<()>>(1);
    let tx = Arc::new(tx);
    let p = Arc::into_raw(tx.clone());
    call(p as *mut c_void);
    let r = rx.recv_timeout(OP_TIMEOUT);
    drop(tx);
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
    // RtcConfiguration::frame_metadata. When false this connection behaves like one
    // built before the capability existed: nothing is advertised, nothing is
    // mirrored, the gate never opens and no transform is installed.
    frame_metadata_enabled: bool,
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
        frame_metadata_enabled: bool,
    ) -> Self {
        Self {
            raw,
            _observer: observer,
            _factory: factory,
            frame_metadata_gate: crate::metadata::FrameMetadataGate::new(),
            frame_metadata_enabled,
        }
    }

    // ── Signaling (blocking on the native callback) ──────────────────────────

    /// Create an offer.
    ///
    /// Every offer advertises frame-metadata support as a session-level
    /// `a=x-reactor-frame-metadata:<version>`, because this crate does support it. A
    /// peer that does not understand the attribute ignores it — RFC 8866 §6 requires
    /// unrecognised attributes to be ignored.
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
        if !self.frame_metadata_enabled {
            return Ok(offer);
        }
        Ok(offer.with_frame_metadata())
    }

    /// Create an answer.
    ///
    /// Mirrors the offer on frame metadata: the capability is declared only when the
    /// offer declared it. Introducing it in an answer that was not offered it is not
    /// something offer/answer can express, so a silent offer produces a silent answer
    /// and the gate stays closed in both directions.
    ///
    /// Requires [`set_remote_description`](Self::set_remote_description) to have been
    /// called with the offer first, which is already the only valid order.
    pub fn create_answer(&self) -> Result<SessionDescription> {
        let answer = run_sdp(|ud| unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_create_answer(
                self.raw, ud, sdp_ok, sdp_err,
            )
        })?;
        // The gate is only ever armed when the flag is on, so this covers both "the
        // offer did not ask" and "this connection does not take part".
        if self.frame_metadata_gate.is_open() {
            return Ok(answer.with_frame_metadata());
        }
        Ok(answer)
    }

    /// Apply the local description.
    ///
    /// Also runs the frame-metadata install, for the same reason
    /// [`set_remote_description`](Self::set_remote_description) does. An answerer
    /// applies the offer *before* it attaches its outbound tracks — apply, attach,
    /// answer — so at the point the remote description armed the gate a sender had
    /// no track to find metadata state on. By the time the answer is set locally it
    /// does. Installing at both points covers the offerer (armed by the answer) and
    /// the answerer (tracks attached after the offer) without either needing to know
    /// which role it is playing.
    pub fn set_local_description(&self, sdp: &SessionDescription) -> Result<()> {
        self.set_description(sdp, true)?;
        self.install_frame_metadata_transforms();
        self.lock_negotiated_send_codecs();
        Ok(())
    }

    /// Apply the remote description, and arm this connection's
    /// [`FrameMetadataGate`](crate::FrameMetadataGate) from it.
    ///
    /// The gate opens when `sdp` declares the capability and closes when it does
    /// not, on every call — so a renegotiation in which the peer drops support
    /// closes it again.
    ///
    /// On an answerer this runs before [`create_answer`](Self::create_answer), which
    /// is what lets the answer mirror the offer.
    pub fn set_remote_description(&self, sdp: &SessionDescription) -> Result<()> {
        self.set_description(sdp, false)?;
        // After the native call, not before: a description libwebrtc rejected was
        // never applied, and must not move the gate.
        //
        // A disabled connection never arms the gate, so it never answers with the
        // capability and never installs a transform.
        self.frame_metadata_gate
            .set(self.frame_metadata_enabled && sdp.declares_frame_metadata());
        self.install_frame_metadata_transforms();
        self.lock_negotiated_send_codecs();
        Ok(())
    }

    /// This connection's frame-metadata gate: what the remote peer declared.
    ///
    /// Cloneable and cheap. Reading it is diagnostic — the library already consults
    /// it when answering, when installing the transforms, and when appending a
    /// trailer, so a caller does not need to. It stays closed until
    /// [`set_remote_description`](Self::set_remote_description) sees a remote
    /// description that declares support.
    pub fn frame_metadata_gate(&self) -> crate::metadata::FrameMetadataGate {
        self.frame_metadata_gate.clone()
    }

    /// Wire the frame-metadata steps into every video transceiver, now that the
    /// remote has said it strips trailers.
    ///
    /// Runs after the remote description has been applied, which is what makes it
    /// possible at all: libwebrtc creates a receiver's track while applying the
    /// description (the same point `on_track` fires), so both directions are
    /// reachable from a transceiver by the time this runs.
    ///
    /// Idempotent, and run from both `set_local_description` and
    /// `set_remote_description`: whichever of the two comes after the tracks were
    /// attached is the one that finds them. A slot installs its native transformer
    /// once and picks up a metadata step configured later on the next frame.
    ///
    /// Silent about failures on purpose. A transceiver with no track, a track whose
    /// Rust wrapper has already been dropped, or a native attach that declines —
    /// none of these are the caller's problem to handle, and none should fail
    /// applying a description. The consequence is only that metadata does not flow,
    /// which is the same as not having negotiated it.
    fn install_frame_metadata_transforms(&self) {
        if !self.frame_metadata_gate.is_open() {
            // Nothing to install. A step left over from a previous generation keeps
            // consulting the gate per frame, so a peer that dropped support stops
            // getting trailers without anything being detached here.
            return;
        }
        let pc_id = self.raw as usize;
        let transceivers = self.transceivers();
        // Prune slots for transceivers that libwebrtc stopped and freed internally
        // (ClearStoppedTransceivers) without going through our Drop path. Doing it
        // here closes the address-reuse window: a new transceiver at the same native
        // address would otherwise inherit a stale slot with installed=true and never
        // get a transformer of its own.
        let live_tc_ids: std::collections::HashSet<usize> =
            transceivers.iter().map(|tc| tc.transceiver_id()).collect();
        crate::sender_meta::prune_stale_slots(pc_id, &live_tc_ids);
        for tc in &transceivers {
            if tc.kind() != MediaKind::Video {
                continue;
            }
            // Composed, not exclusive: a caller's own transform on either side keeps
            // working, and attach_* returns a transformer to install only the first
            // time this side needs one.
            let id = tc.transceiver_id();
            // Per-track gate: a track created with frame_metadata off never
            // gets a trailer writer, whatever the connection negotiated.
            if crate::sender_meta::allowed(tc.sender_track_id()) {
                if let Some(source) = crate::sender_meta::lookup(tc.sender_track_id()) {
                    if let Some(native) = crate::sender_meta::attach_embed(
                        pc_id,
                        id,
                        source,
                        self.frame_metadata_gate.clone(),
                    ) {
                        if tc
                            .attach_native_transform(crate::sender_meta::Side::Send, &native)
                            .is_err()
                        {
                            crate::sender_meta::release_install(
                                pc_id,
                                id,
                                crate::sender_meta::Side::Send,
                            );
                        }
                    }
                }
            }
            if let Some(queue) = crate::sender_meta::lookup_receiver(tc.receiver_track_id()) {
                if let Some(native) = crate::sender_meta::attach_strip(pc_id, id, queue) {
                    if tc
                        .attach_native_transform(crate::sender_meta::Side::Receive, &native)
                        .is_err()
                    {
                        crate::sender_meta::release_install(
                            pc_id,
                            id,
                            crate::sender_meta::Side::Receive,
                        );
                    }
                }
            }
        }
    }

    /// Best-effort: apply [`Transceiver::try_lock_negotiated_send_codec`] to
    /// every video transceiver.
    ///
    /// Idempotent, and run from both `set_local_description` and
    /// `set_remote_description` for the same reason
    /// [`install_frame_metadata_transforms`](Self::install_frame_metadata_transforms)
    /// is: whichever of the two completes negotiation on this side is the one
    /// that finds a sender with a negotiated codec list to lock onto — the
    /// answerer's own sender is ready after its `set_local_description`, the
    /// offerer's only after the answer arrives via `set_remote_description`.
    ///
    /// Silent about failures on purpose, same as the metadata install: a
    /// transceiver with no preference set, no sender, or nothing negotiated
    /// yet on this call is not the caller's problem, and none of it should
    /// fail applying a description. Calling this again after the codec is
    /// already locked just re-confirms the same match.
    fn lock_negotiated_send_codecs(&self) {
        for tc in self.transceivers() {
            if tc.kind() != MediaKind::Video {
                continue;
            }
            tc.try_lock_negotiated_send_codec();
        }
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
    /// Add a remote ICE candidate received out of band (trickle ICE).
    ///
    /// An empty [`IceCandidate::candidate`] string is the end-of-candidates
    /// marker (RFC 8838) and succeeds as a no-op rather than failing the
    /// candidate-string parse.
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
    ///
    /// Every track a peer publishes shares one MediaStream, so the remote can
    /// sync the audio it receives against the video.
    pub fn add_track(&self, track: &Track) -> Result<()> {
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_peer_connection_add_track(self.raw, track.raw())
        };
        if ok == 1 {
            // Re-run after add_track so that an answerer that calls
            // set_remote_description → add_track → create_answer → set_local_description
            // does not have to wait for set_local_description to wire metadata.
            // Idempotent: a pre-negotiation call is a no-op (gate is still closed).
            self.install_frame_metadata_transforms();
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
            Ok(Transceiver::from_raw(
                raw,
                self.raw as usize,
                self.frame_metadata_gate.clone(),
            ))
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
                (!raw.is_null()).then(|| {
                    Transceiver::from_raw(raw, self.raw as usize, self.frame_metadata_gate.clone())
                })
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
        // Release the composed transform slots first, while the transceivers can
        // still be enumerated. Keyed by (pc_id, tc_id), so leaving them would let a
        // recycled pointer on the same or another connection inherit stale callbacks.
        let pc_id = self.raw as usize;
        for tc in self.transceivers() {
            crate::sender_meta::forget_transceiver(pc_id, tc.transceiver_id());
        }
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
    use crate::metadata::{FRAME_METADATA_ATTRIBUTE, FRAME_METADATA_VERSION};

    fn declaration() -> String {
        format!("a={FRAME_METADATA_ATTRIBUTE}:{FRAME_METADATA_VERSION}")
    }

    /// A bundled audio+video description, as libwebrtc emits one, with `extra`
    /// spliced in at session level (after `t=`, before the first `m=`).
    fn described(extra: &str) -> SessionDescription {
        SessionDescription {
            kind: SdpType::Offer,
            sdp: format!(
                "v=0\r\n\
                 o=- 1 2 IN IP4 127.0.0.1\r\n\
                 s=-\r\n\
                 t=0 0\r\n\
                 a=group:BUNDLE 0 1\r\n\
                 {extra}\
                 m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                 c=IN IP4 0.0.0.0\r\n\
                 a=mid:0\r\n\
                 m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                 c=IN IP4 0.0.0.0\r\n\
                 a=mid:1\r\n\
                 a=fingerprint:sha-256 AA:BB\r\n"
            ),
        }
    }

    fn bundled() -> SessionDescription {
        described("")
    }

    #[test]
    fn declares_once_at_session_level() {
        let out = bundled().with_frame_metadata();
        assert!(out.declares_frame_metadata());
        assert_eq!(out.sdp.matches(&declaration()).count(), 1);
    }

    #[test]
    fn inserted_before_the_first_media_section() {
        // RFC 8866 §5 puts session-level attributes after t=/z=/k= and before the
        // first media description; everything before the first m= is session level.
        let out = bundled().with_frame_metadata();
        let lines: Vec<&str> = out.sdp.lines().collect();
        let at = |needle: &str| lines.iter().position(|l| l.starts_with(needle)).unwrap();
        assert!(at("t=") < at("a=x-reactor-frame-metadata:"));
        assert!(at("a=x-reactor-frame-metadata:") < at("m="));
    }

    #[test]
    fn declares_on_an_audio_only_description() {
        // Session level, so there is nothing about video to condition it on — and a
        // renegotiation that adds video must not have to introduce the capability.
        let audio_only = SessionDescription {
            kind: SdpType::Offer,
            sdp: "v=0\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n".into(),
        };
        let out = audio_only.with_frame_metadata();
        assert!(out.declares_frame_metadata());
        let lines: Vec<&str> = out.sdp.lines().collect();
        let declared = lines
            .iter()
            .position(|l| l.starts_with("a=x-reactor-frame-metadata:"))
            .expect("declaration");
        let first_media = lines.iter().position(|l| l.starts_with("m=")).expect("m=");
        assert!(
            declared < first_media,
            "declaration landed inside a media section"
        );
    }

    #[test]
    fn declares_on_a_description_with_no_media_section() {
        let no_media = SessionDescription {
            kind: SdpType::Offer,
            sdp: "v=0\r\nt=0 0\r\n".into(),
        };
        assert!(no_media.with_frame_metadata().declares_frame_metadata());
    }

    #[test]
    fn an_empty_description_is_left_alone() {
        // Emitting a lone attribute line would be invalid SDP, and there is nothing
        // useful to declare it on.
        let empty = SessionDescription {
            kind: SdpType::Offer,
            sdp: String::new(),
        };
        let out = empty.with_frame_metadata();
        assert!(out.sdp.is_empty());
        assert!(!out.declares_frame_metadata());
    }

    #[test]
    fn is_idempotent() {
        let once = bundled().with_frame_metadata();
        let twice = once.with_frame_metadata();
        assert_eq!(once.sdp, twice.sdp);
        assert_eq!(once.sdp.matches(&declaration()).count(), 1);
    }

    #[test]
    fn a_different_version_reads_as_unsupported() {
        // The version is the compatibility token: a peer speaking a future trailer
        // format must not look like a peer speaking this one.
        let future = described(&format!(
            "a={FRAME_METADATA_ATTRIBUTE}:{}\r\n",
            FRAME_METADATA_VERSION + 1
        ));
        assert!(!future.declares_frame_metadata());
        // …and declaring ours alongside it works.
        let out = future.with_frame_metadata();
        assert!(out.declares_frame_metadata());
    }

    #[test]
    fn a_malformed_version_reads_as_unsupported() {
        for bad in ["", "abc", "1.0", "-1"] {
            let d = described(&format!("a={FRAME_METADATA_ATTRIBUTE}:{bad}\r\n"));
            assert!(
                !d.declares_frame_metadata(),
                "accepted version {bad:?} as ours"
            );
        }
    }

    #[test]
    fn a_similar_attribute_name_does_not_match() {
        let d = described(&format!(
            "a={FRAME_METADATA_ATTRIBUTE}-2:{FRAME_METADATA_VERSION}\r\n"
        ));
        assert!(!d.declares_frame_metadata());
    }

    #[test]
    fn leaves_everything_else_intact() {
        let before = bundled();
        let after = before.with_frame_metadata();
        for keep in [
            "a=group:BUNDLE 0 1",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111",
            "m=video 9 UDP/TLS/RTP/SAVPF 96",
            "a=mid:1",
            "a=fingerprint:sha-256 AA:BB",
        ] {
            assert!(after.sdp.contains(keep), "lost {keep:?}");
        }
        assert_eq!(after.sdp.lines().count(), before.sdp.lines().count() + 1);
        assert_eq!(after.kind, before.kind);
    }

    #[test]
    fn output_is_crlf_terminated() {
        let out = bundled().with_frame_metadata();
        assert!(out.sdp.ends_with("\r\n"));
        assert_eq!(
            out.sdp.matches('\n').count(),
            out.sdp.matches("\r\n").count()
        );
    }
}

#[cfg(test)]
mod frame_metadata_gate_tests {
    use crate::metadata::FrameMetadataGate;

    #[test]
    fn starts_closed() {
        // Closed-by-default is the safe direction: a sender that has not yet applied
        // a remote description must not append trailers.
        assert!(!FrameMetadataGate::new().is_open());
    }

    #[test]
    fn clones_share_one_state() {
        let gate = FrameMetadataGate::new();
        let handed_to_transform = gate.clone();
        gate.set(true);
        assert!(handed_to_transform.is_open());
        // A renegotiation where the peer drops support closes it again.
        gate.set(false);
        assert!(!handed_to_transform.is_open());
    }
}
