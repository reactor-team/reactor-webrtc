//! The peer connection and its associated signaling/data types.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Whether this description declares data-channel chunking at a version
    /// this build speaks (`a=x-reactor-dc-chunking:1 …`). Read off the SDP
    /// string, like [`declares_frame_metadata`](Self::declares_frame_metadata).
    pub fn declares_dc_chunking(&self) -> bool {
        reactor_webrtc_dc_chunking::sdp::parse(&self.sdp).is_some()
    }

    /// Return a copy declaring data-channel chunking with `params`, at session
    /// level. Idempotent. [`PeerConnection::create_offer`] and
    /// [`create_answer`](PeerConnection::create_answer) already apply it when
    /// the connection takes part, so callers using this crate's signalling
    /// never need it.
    pub fn with_dc_chunking(&self, params: &crate::DcChunkingParams) -> Self {
        Self {
            kind: self.kind,
            sdp: reactor_webrtc_dc_chunking::sdp::declare(&self.sdp, params),
        }
    }

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

    /// Hold this transceiver's received media in the jitter buffer for at least
    /// `delay` (`RTCRtpReceiver.jitterBufferTarget`); `None` restores the
    /// default. The buffer still adds whatever the network's jitter calls for
    /// on top — this is a floor, so it can only add latency, never remove it.
    /// To cut playout latency instead, see
    /// [`with_receive_playout_delay`](crate::PeerConnectionFactoryBuilder::with_receive_playout_delay).
    ///
    /// Takes effect immediately on a running stream, and is remembered when set
    /// before one exists — on a transceiver of your own before you offer, or in
    /// `on_track` — so it
    /// applies from the first frame. Read the result back as
    /// [`InboundRtpStats::jitter_buffer_target_delay_s`]. Fails when `delay`
    /// is above libwebrtc's 10 s limit.
    pub fn set_jitter_buffer_minimum_delay(
        &self,
        delay: Option<std::time::Duration>,
    ) -> Result<()> {
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
        if delay.is_some_and(|d| d > LIMIT) {
            return Err(Error::Webrtc(format!(
                "jitter buffer minimum delay {delay:?} is above libwebrtc's {LIMIT:?}"
            )));
        }
        let ok = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_jitter_buffer_minimum_delay(
                self.raw,
                delay.is_some() as c_int,
                delay.map_or(0.0, |d| d.as_secs_f64()),
            )
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(Error::Webrtc(
                "transceiver set_jitter_buffer_minimum_delay failed".into(),
            ))
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

    /// Set this transceiver's **per-sender** bitrate bounds — the ceiling that
    /// actually caps the video encoder.
    ///
    /// This is a different knob from [`PeerConnection::set_bitrate`], and the
    /// two are conjunctive: the lower one wins.
    ///
    /// - [`PeerConnection::set_bitrate`] bounds the *aggregate* congestion-control
    ///   estimate for the whole connection — how much bandwidth the GCC algorithm
    ///   believes it may allocate.
    /// - This method bounds *this one stream's* share of that allocation.
    ///
    /// Without this call, the stream's ceiling is libwebrtc's resolution-keyed
    /// default: 600 kbps up to 320x240, 1700 up to 640x480, 2000 up to 960x540,
    /// and **2500 kbps for everything above that**. So 720p, 1080p and 4K all cap
    /// at 2.5 Mbps no matter how high the congestion-control ceiling is raised —
    /// setting `max_bps` here is the only way to lift it.
    ///
    /// Both bounds are optional; pass `None` to leave one at the libwebrtc
    /// default. Values are in bits per second, and apply to the first encoding
    /// (the single-stream case — simulcast layers are not addressed individually).
    ///
    /// **When the sender has encodings to write depends on where the transceiver
    /// came from**, and it is the one sharp edge here:
    ///
    /// | Transceiver from | audio | video |
    /// |---|---|---|
    /// | [`PeerConnection::add_transceiver`] | has encodings | has encodings |
    /// | applying a remote description | **none until the local description is applied** | has encodings |
    ///
    /// `add_transceiver` seeds a default encoding; one materialised from a
    /// remote offer does not get one for audio until the answer is set locally.
    /// Called before that, this returns "sender has no encodings".
    ///
    /// Only the *default* being lifted is video-specific — it is keyed on frame
    /// size. The bounds themselves apply to an audio sender too, capping its
    /// allocation. So an answerer that only wants to clear that default can
    /// bound its video senders while building the answer, and one that also
    /// wants an audio bound applies it after
    /// [`PeerConnection::set_local_description`].
    ///
    /// Can be called again at any point to change the bounds mid-call.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Webrtc`] when either bound is negative — `None`, not a
    /// negative number, is how a bound is left unset — when `min_bps` exceeds
    /// `max_bps`, when the transceiver has no sender or no encodings, or when
    /// libwebrtc rejects the parameters.
    pub fn set_send_bitrate(&self, min_bps: Option<i32>, max_bps: Option<i32>) -> Result<()> {
        // `None` is how a caller says "leave this at the libwebrtc default", and
        // it crosses the ABI as -1. A negative `Some` is therefore ambiguous with
        // that sentinel, and resolving it as "unset" would take a typo — or an
        // arithmetic slip like `budget - overhead` going below zero — and quietly
        // remove a cap the caller had set, reporting success. Refuse it here, so
        // it never reaches the boundary where the two become indistinguishable.
        for (label, value) in [("min_bps", min_bps), ("max_bps", max_bps)] {
            if let Some(v) = value {
                if v < 0 {
                    return Err(Error::Webrtc(format!(
                        "{label} must be >= 0; pass None to leave it at the libwebrtc default \
                         (got {v})"
                    )));
                }
            }
        }

        let mut err = [0 as std::os::raw::c_char; 256];
        let rc = unsafe {
            reactor_webrtc_sys::reactor_webrtc_rtp_transceiver_set_send_bitrate(
                self.raw,
                min_bps.unwrap_or(-1),
                max_bps.unwrap_or(-1),
                err.as_mut_ptr(),
                err.len() as std::os::raw::c_int,
            )
        };
        if rc != 0 {
            let reason = unsafe { std::ffi::CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(Error::Webrtc(if reason.is_empty() {
                "transceiver set_send_bitrate failed".into()
            } else {
                reason
            }));
        }
        Ok(())
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

/// Media kind of an RTP stream (`RTCRtpStreamStats::kind`).
///
/// The field that lets a reader tell which of several receive streams is the
/// video one. Without it there is only the SSRC, and "the video stream's jitter"
/// becomes "the worst jitter of anything arriving" — a different statistic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// The engine did not report a kind.
    Unknown,
    Audio,
    Video,
}

impl StreamKind {
    fn from_raw(v: c_int) -> Self {
        match v {
            0 => StreamKind::Audio,
            1 => StreamKind::Video,
            _ => StreamKind::Unknown,
        }
    }
}

/// Type of an ICE candidate (`RTCIceCandidateStats::candidate_type`).
///
/// [`IceCandidateType::Relay`] means the media is going through a TURN server,
/// which is the first thing worth knowing when latency is bad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceCandidateType {
    /// The engine did not report one, or the pair named no local candidate.
    Unknown,
    /// A local interface address.
    Host,
    /// Server-reflexive: discovered through STUN.
    Srflx,
    /// Peer-reflexive: learned from an incoming connectivity check.
    Prflx,
    /// Relayed through TURN.
    Relay,
}

impl IceCandidateType {
    fn from_raw(v: c_int) -> Self {
        match v {
            0 => IceCandidateType::Host,
            1 => IceCandidateType::Srflx,
            2 => IceCandidateType::Prflx,
            3 => IceCandidateType::Relay,
            _ => IceCandidateType::Unknown,
        }
    }
}

/// Transport a relayed candidate uses to reach its TURN server
/// (`RTCIceCandidateStats::relay_protocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayProtocol {
    /// Not relayed, or not reported. Distinct from a protocol: a `Host`
    /// candidate has no relay to reach.
    ///
    /// Spelled out rather than `None` because the Python binding mirrors these
    /// variant names, and `RelayProtocol.None` is not addressable from Python.
    NotRelayed,
    Udp,
    Tcp,
    Tls,
}

impl RelayProtocol {
    fn from_raw(v: c_int) -> Self {
        match v {
            0 => RelayProtocol::Udp,
            1 => RelayProtocol::Tcp,
            2 => RelayProtocol::Tls,
            _ => RelayProtocol::NotRelayed,
        }
    }
}

/// `RTCInboundRtpStreamStats` subset.
#[derive(Debug, Clone)]
pub struct InboundRtpStats {
    pub ssrc: u32,
    /// Audio or video. See [`StreamKind`].
    pub kind: StreamKind,
    /// The transceiver this stream belongs to (`RTCInboundRtpStreamStats::mid`).
    /// Several tracks of one kind are told apart by this, not by
    /// [`StreamKind`]: match it against [`Transceiver::mid`]. `None` before the
    /// stream is negotiated.
    pub mid: Option<String>,
    /// The codec this stream carries, as its mime type (`"video/VP9"`,
    /// `"audio/opus"`), from the `RTCCodecStats` its `codec_id` names. `None`
    /// until the stream has a codec.
    pub codec_mime_type: Option<String>,
    pub packets_received: u32,
    pub bytes_received: u64,
    /// Jitter in seconds.
    pub jitter_s: f64,
    pub packets_lost: i32,
    /// Retransmissions this endpoint asked the sender for.
    ///
    /// Repair traffic moves before loss does — a retransmission that arrives in
    /// time hides the loss that prompted it — so this climbs while the stream
    /// still plays. Read it as the earliest sign a receive path is going bad.
    pub nack_count: u32,
    /// Keyframe requests this endpoint sent because its decoder could not
    /// continue. The step past [`InboundRtpStats::nack_count`]: a NACK asks for
    /// one packet again, a Picture Loss Indication says repair has been outrun
    /// and the stream has to restart from a fresh keyframe.
    pub pli_count: u32,
    /// Full Intra Refresh requests this endpoint sent. Serves the same purpose
    /// as [`InboundRtpStats::pli_count`] and which one a decoder sends depends
    /// on the codec, so a reader after "the stream had to be restarted" wants
    /// the two together.
    pub fir_count: u32,
    /// Cumulative decode time in seconds.
    pub total_decode_time_s: f64,
    /// Decoded frames per second; `0.0` if not measured. Video only.
    pub frames_per_second: f64,
    pub frames_decoded: u32,
    pub frames_dropped: u32,
    /// Decoded frame size; `0` for audio, and before the first frame.
    pub frame_width: u32,
    pub frame_height: u32,
    /// Cumulative time, in seconds, that the frames counted by
    /// [`jitter_buffer_emitted_count`](Self::jitter_buffer_emitted_count) spent
    /// in the jitter buffer (`RTCInboundRtpStreamStats::jitterBufferDelay`). For
    /// video, from a frame's first packet arriving to the frame leaving for the
    /// decoder; divide by the count for the per-frame average.
    pub jitter_buffer_delay_s: f64,
    /// Cumulative target delay in seconds, over the same frames: what the
    /// jitter buffer was aiming for, with every floor in force — a
    /// [`Transceiver::set_jitter_buffer_minimum_delay`], a playout delay, or
    /// what A/V sync needed. A floor shows up here, less the receiver's ~10 ms
    /// render delay.
    pub jitter_buffer_target_delay_s: f64,
    /// Cumulative minimum delay in seconds, over the same frames. For video
    /// this is libwebrtc's own computed minimum — jitter estimate plus decode
    /// and render time — not a floor the app set; read
    /// [`jitter_buffer_target_delay_s`](Self::jitter_buffer_target_delay_s)
    /// for that.
    pub jitter_buffer_minimum_delay_s: f64,
    /// Frames that have left the jitter buffer — the denominator for the three
    /// cumulative delays above.
    pub jitter_buffer_emitted_count: u64,
    /// Cumulative time from a frame's first packet arriving to it being
    /// decoded, in seconds, over [`InboundRtpStats::frames_decoded`].
    pub total_processing_delay_s: f64,
    /// The slowest timing frame of the last second, or `None` if none arrived
    /// in it. Video only.
    pub timing_frame: Option<TimingFrameInfo>,
}

impl InboundRtpStats {
    /// Average time a frame spent in the jitter buffer, or `None` before the
    /// first one left it.
    pub fn average_jitter_buffer_delay(&self) -> Option<std::time::Duration> {
        (self.jitter_buffer_emitted_count > 0).then(|| {
            std::time::Duration::from_secs_f64(
                (self.jitter_buffer_delay_s / self.jitter_buffer_emitted_count as f64).max(0.0),
            )
        })
    }
}

/// One frame libwebrtc stamped at each stage of its trip, as reported by the
/// receiver (`goog_timing_frame_info`).
///
/// By default the sender marks a frame every 200 ms, plus any frame at least
/// five times the average size, and carries its stamps in the `video-timing`
/// RTP header extension, on the last packet of the frame; the receiver adds
/// its own. It is the only per-frame view of the packetizer and the pacer. Of
/// the timing frames that arrived in the last second, libwebrtc reports the
/// one that took longest, so this is the worst recent frame, not a typical
/// one.
///
/// The timestamps come in two groups, by the side that took them. All of them
/// are on our clock: libwebrtc moves the sender's onto it once it has estimated
/// the offset between the two clocks, and until then reports them as negative
/// values that are still right relative to each other. So differences within
/// one group are times: `sender.encode_finish_ms - sender.encode_start_ms` is
/// encode time, `receiver.decode_start_ms - receiver.receive_finish_ms` is how
/// long the frame waited in the jitter buffer. A difference across the two
/// groups, such as `receiver.receive_start_ms - sender.pacer_exit_ms`, is only
/// an estimate, off by the error in the clock offset, and meaningless while
/// the sender stamps are negative.
///
/// The same frame can be reported by several consecutive reads; compare
/// [`TimingFrameInfo::rtp_timestamp`] to tell a new sample from a repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingFrameInfo {
    pub rtp_timestamp: u32,
    /// The sender marked this frame for its size.
    pub is_outlier: bool,
    /// The sender marked this frame because the periodic timer was due.
    pub is_timer_triggered: bool,
    pub sender: TimingFrameSenderTimestamps,
    pub receiver: TimingFrameReceiverTimestamps,
}

/// The timestamps the sender took for a [`TimingFrameInfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingFrameSenderTimestamps {
    /// When the frame was captured (pushed into the track):
    /// `encode_start_ms - capture_ms` is how long it waited for the encoder.
    pub capture_ms: i64,
    pub encode_start_ms: i64,
    pub encode_finish_ms: i64,
    /// When the encoded frame had been cut into packets and handed to the pacer.
    pub packetization_finish_ms: i64,
    /// When the pacer sent the frame's last packet.
    pub pacer_exit_ms: i64,
}

/// The timestamps the receiver (this side) took for a [`TimingFrameInfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingFrameReceiverTimestamps {
    pub receive_start_ms: i64,
    pub receive_finish_ms: i64,
    pub decode_start_ms: i64,
    pub decode_finish_ms: i64,
}

/// `RTCOutboundRtpStreamStats` subset.
#[derive(Debug, Clone)]
pub struct OutboundRtpStats {
    pub ssrc: u32,
    /// Audio or video. See [`StreamKind`].
    pub kind: StreamKind,
    /// The transceiver this stream belongs to (`RTCOutboundRtpStreamStats::mid`).
    /// Several tracks of one kind are told apart by this, not by
    /// [`StreamKind`]: match it against [`Transceiver::mid`]. `None` before the
    /// stream is negotiated.
    pub mid: Option<String>,
    /// The codec this stream carries, as its mime type (`"video/VP9"`,
    /// `"audio/opus"`), from the `RTCCodecStats` its `codec_id` names. `None`
    /// until the stream has a codec.
    pub codec_mime_type: Option<String>,
    /// 64-bit because `RTCSentRtpStreamStats` reports it that way. It was `u32`
    /// until 0.15.0, which wrapped silently after ~4.3 billion packets — about
    /// seven weeks at a thousand packets a second — and then reported a
    /// cumulative counter that had gone backwards.
    pub packets_sent: u64,
    pub bytes_sent: u64,
    /// Target encoder bitrate in bps.
    pub target_bitrate_bps: f64,
    /// Round-trip time in seconds; `0.0` if not yet measured.
    ///
    /// From the receiver's RTCP report about us
    /// (`RTCRemoteInboundRtpStreamStats`), which is where libwebrtc moved it in
    /// M7907 — so it stays `0.0` until the far end has sent one, and a zero is
    /// "not measured yet" rather than a zero-latency link.
    pub round_trip_time_s: f64,
    /// Cumulative round-trip time in seconds, from the same report.
    pub total_round_trip_time_s: f64,
    /// Fraction of this stream the receiver reports as lost, `0.0`–`1.0`.
    pub fraction_lost: f64,
    /// Packets the receiver reports as lost. Signed, per RFC 3550.
    pub packets_lost: i32,
    /// 64-bit for the same reason as [`OutboundRtpStats::packets_sent`].
    pub retransmitted_packets_sent: u64,
    /// Retransmissions the receiver asked this endpoint for.
    ///
    /// The requests themselves, where
    /// [`OutboundRtpStats::retransmitted_packets_sent`] is what was sent in
    /// answer to them. The two differ when a request went unanswered, and they
    /// count different things — one is requests, the other packets. Repair
    /// traffic moves before loss does, so this is the earliest sign that the
    /// path to a viewer is going bad.
    pub nack_count: u32,
    /// Keyframe requests the receiver sent because its decoder could not
    /// continue. The step past [`OutboundRtpStats::nack_count`]: a NACK asks
    /// for one packet again, a Picture Loss Indication says repair has been
    /// outrun and the stream has to restart from a fresh keyframe. Answering
    /// one costs a keyframe, which is the bitrate spike a viewer sees as the
    /// picture snapping back.
    pub pli_count: u32,
    /// Full Intra Refresh requests the receiver sent. Serves the same purpose
    /// as [`OutboundRtpStats::pli_count`] and which one a decoder sends depends
    /// on the codec, so a reader after "the stream had to be restarted" wants
    /// the two together.
    pub fir_count: u32,
    /// Encoded frames per second; `0.0` if not measured. Video only.
    pub frames_per_second: f64,
    pub frames_sent: u32,
    /// Encoded frame size; `0` for audio, and before the first frame.
    pub frame_width: u32,
    pub frame_height: u32,
    pub frames_encoded: u32,
    /// Cumulative encode time in seconds, over
    /// [`OutboundRtpStats::frames_encoded`].
    pub total_encode_time_s: f64,
    /// Cumulative time packets waited in the pacer before being sent, in
    /// seconds. Summed over packets, not frames: divide by
    /// [`OutboundRtpStats::packets_sent`].
    pub total_packet_send_delay_s: f64,
}

/// `RTCIceCandidatePairStats` subset.
#[derive(Debug, Clone)]
pub struct IceCandidatePairStats {
    /// Current RTT in seconds; `0.0` if not yet measured.
    pub current_round_trip_time_s: f64,
    /// Cumulative RTT in seconds across every check on this pair.
    pub total_round_trip_time_s: f64,
    pub priority: u64,
    pub state: IceCandidatePairState,
    /// Whether ICE selected this pair. The one to read rather than inferring
    /// the selected pair from `state` and `priority`.
    pub nominated: bool,
    pub writable: bool,
    /// Congestion-control estimates in bps; `0.0` when the engine has none yet.
    pub available_outgoing_bitrate_bps: f64,
    pub available_incoming_bitrate_bps: f64,
    /// Everything this pair carried — RTCP and data channel included, so wider
    /// than the per-stream RTP counters.
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    /// Type of this pair's *local* candidate. [`IceCandidateType::Relay`] is
    /// what says the session is going through TURN.
    pub local_candidate_type: IceCandidateType,
    /// Transport to the TURN server, when relayed.
    pub local_relay_protocol: RelayProtocol,
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

/// One callback, shared so it can be called with its slot unlocked.
type Shared<F> = Arc<Mutex<F>>;

/// A callback slot. A callback runs with the slot's lock released, so it may
/// replace any handler of its own channel; its own lock, held while it runs,
/// keeps calls to it in order.
struct Slot<F>(Mutex<Option<Shared<F>>>);

impl<F> Slot<F> {
    fn empty() -> Self {
        Self(Mutex::new(None))
    }

    fn set(&self, f: F) {
        *self.0.lock().unwrap() = Some(Arc::new(Mutex::new(f)));
    }

    fn get(&self) -> Option<Shared<F>> {
        self.0.lock().unwrap().clone()
    }
}

/// The native channel handle, shared with the callbacks that run on WebRTC's
/// threads. Valid for as long as the owning [`DataChannel`] lives: the native
/// observer is unregistered before the handle is destroyed.
#[derive(Clone, Copy)]
struct RawDc(*mut reactor_webrtc_sys::DataChannel);
// SAFETY: the native data channel is internally thread-safe.
unsafe impl Send for RawDc {}
unsafe impl Sync for RawDc {}

impl RawDc {
    fn state(self) -> DataChannelState {
        DataChannelState::from_raw(unsafe {
            reactor_webrtc_sys::reactor_webrtc_data_channel_state(self.0)
        })
    }
    fn buffered_amount(self) -> u64 {
        unsafe { reactor_webrtc_sys::reactor_webrtc_data_channel_buffered_amount(self.0) }
    }
    fn send(self, data: &[u8], binary: bool) -> bool {
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_data_channel_send(
                self.0,
                data.as_ptr(),
                data.len(),
                binary as c_int,
            ) == 1
        }
    }
    fn close(self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_data_channel_close(self.0) }
    }
}

/// Per-channel state of a chunked channel.
struct Chunked {
    queue: Mutex<reactor_webrtc_dc_chunking::SendQueue>,
    // Held while frames move from the queue to the native channel, so the
    // frames of one message stay contiguous whoever pumps. Taken with
    // try_lock by every caller, after setting `pump_again`: a caller that
    // finds it busy leaves the work to the holder instead of waiting, so a
    // callback never blocks on a sender.
    pump: Mutex<()>,
    pump_again: AtomicBool,
    reassembler: Mutex<reactor_webrtc_dc_chunking::Reassembler>,
    // Whole messages that arrived before on_message was set: libwebrtc holds
    // messages for a channel with no observer, and this keeps that promise
    // for the frames the pump's observer had to accept. Bounded by
    // `send_buffer_limit` bytes.
    pending: Mutex<std::collections::VecDeque<(Vec<u8>, bool)>>,
    pending_bytes: AtomicU64,
    pending_limit: u64,
    // The caller's on_buffered_amount_low threshold. The native threshold
    // belongs to the pump (the queue's low-water mark).
    user_low_threshold: AtomicU64,
    user_low_armed: AtomicBool,
}

/// Everything the native observer reaches through `userdata`.
struct ChannelCore {
    raw: RawDc,
    on_message: Slot<MessageCb>,
    on_state_change: Slot<StateCb>,
    // Their own slots, so on_open, on_close and on_state_change coexist.
    on_open: Slot<EventCb>,
    on_close: Slot<EventCb>,
    on_buffered_amount_low: Slot<EventCb>,
    registered: AtomicBool,
    // The owning connection's chunking negotiation, and this channel's
    // decision, made once it is open.
    negotiation: Option<Arc<crate::dc_chunking::DcNegotiation>>,
    decided: std::sync::OnceLock<Option<Chunked>>,
    // The threshold the caller set before the channel decided, so a chunked
    // channel can take it over.
    early_low_threshold: AtomicU64,
}

impl ChannelCore {
    /// This channel's chunking state, deciding it on first call once open.
    fn chunked(&self) -> Option<&Chunked> {
        if let Some(decided) = self.decided.get() {
            return decided.as_ref();
        }
        let negotiation = self.negotiation.as_ref()?;
        if self.raw.state() != DataChannelState::Open {
            return None;
        }
        let ordered =
            unsafe { reactor_webrtc_sys::reactor_webrtc_data_channel_ordered(self.raw.0) != 0 };
        let reliable =
            unsafe { reactor_webrtc_sys::reactor_webrtc_data_channel_reliable(self.raw.0) != 0 };
        let chunked = match (negotiation.settings(), negotiation.remote()) {
            (Some(settings), Some(remote)) if ordered && reliable => {
                let config = settings.send_config(&remote);
                Some(Chunked {
                    queue: Mutex::new(reactor_webrtc_dc_chunking::SendQueue::new(config)),
                    pump: Mutex::new(()),
                    pump_again: AtomicBool::new(false),
                    reassembler: Mutex::new(reactor_webrtc_dc_chunking::Reassembler::new(
                        settings.max_message_size,
                    )),
                    pending: Mutex::new(std::collections::VecDeque::new()),
                    pending_bytes: AtomicU64::new(0),
                    pending_limit: settings.send_buffer_limit,
                    user_low_threshold: AtomicU64::new(
                        self.early_low_threshold.load(Ordering::SeqCst),
                    ),
                    user_low_armed: AtomicBool::new(false),
                })
            }
            _ => None,
        };
        let decided = self.decided.get_or_init(|| chunked).as_ref();
        if let Some(c) = decided {
            // The pump needs the native callbacks whether or not the caller
            // set any, and resumes at the queue's low-water mark.
            let low = c.queue.lock().unwrap().config().low_water;
            unsafe {
                reactor_webrtc_sys::reactor_webrtc_data_channel_set_low_threshold(self.raw.0, low)
            };
            self.ensure_registered();
        }
        decided
    }

    fn ensure_registered(&self) {
        if !self.registered.swap(true, Ordering::SeqCst) {
            self.register();
        }
    }

    fn register(&self) {
        let ud = self as *const ChannelCore as *mut c_void;
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_data_channel_register_observer(
                self.raw.0,
                ud,
                dc_on_message,
                dc_on_state_change,
                dc_on_buffered_amount_low,
            );
        }
    }

    /// Native buffered bytes plus, on a chunked channel, the queued ones.
    fn buffered_amount(&self) -> u64 {
        let native = self.raw.buffered_amount();
        match self.chunked() {
            Some(c) => native + c.queue.lock().unwrap().queued(),
            None => native,
        }
    }

    /// Move frames from the queue to the native channel while it has room.
    fn pump(&self, c: &Chunked) {
        loop {
            // Ask first, then try the lock. A failed try_lock means another
            // thread holds it and has not yet re-checked the flag after
            // unlocking, so the request cannot be lost between its last pass
            // and its exit.
            c.pump_again.store(true, Ordering::SeqCst);
            let Ok(_guard) = c.pump.try_lock() else {
                return;
            };
            while c.pump_again.swap(false, Ordering::SeqCst) {
                loop {
                    let frame = c
                        .queue
                        .lock()
                        .unwrap()
                        .next_frame(self.raw.buffered_amount());
                    let Some(frame) = frame else { break };
                    if !self.raw.send(&frame, true) {
                        // libwebrtc closes the channel on a failed send. The
                        // peer may hold part of a message; nothing more can
                        // follow it.
                        c.queue.lock().unwrap().clear();
                        return;
                    }
                }
            }
            drop(_guard);
            // A request that arrived after our last pass is ours to serve.
            if !c.pump_again.load(Ordering::SeqCst) {
                break;
            }
        }
    }

    fn maybe_fire_user_low(&self, c: &Chunked) {
        let threshold = c.user_low_threshold.load(Ordering::SeqCst);
        if self.buffered_amount() <= threshold && c.user_low_armed.swap(false, Ordering::SeqCst) {
            if let Some(cb) = self.on_buffered_amount_low.get() {
                (*cb.lock().unwrap())();
            }
        }
    }

    /// Hand a whole message to the caller, or hold it until on_message is set.
    ///
    /// The slot stays locked only while deciding, so a message cannot be
    /// held just after `on_message` flushed what was held.
    fn deliver(&self, c: &Chunked, data: Vec<u8>, binary: bool) {
        let cb = {
            let slot = self.on_message.0.lock().unwrap();
            match slot.clone() {
                Some(cb) => cb,
                None => {
                    let size = data.len() as u64;
                    if c.pending_bytes.load(Ordering::SeqCst) + size <= c.pending_limit {
                        c.pending_bytes.fetch_add(size, Ordering::SeqCst);
                        c.pending.lock().unwrap().push_back((data, binary));
                    }
                    return;
                }
            }
        };
        (*cb.lock().unwrap())(&data, binary);
    }
}

extern "C" fn dc_on_message(ud: *mut c_void, data: *const u8, len: usize, binary: c_int) {
    let core = unsafe { &*(ud as *const ChannelCore) };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    let Some(c) = core.chunked() else {
        if let Some(cb) = core.on_message.get() {
            (*cb.lock().unwrap())(bytes, binary != 0);
        }
        return;
    };
    // Every frame of a chunked channel is sent as binary.
    let delivery = if binary != 0 {
        c.reassembler.lock().unwrap().push(bytes)
    } else {
        Err(reactor_webrtc_dc_chunking::FrameError::TypeChanged)
    };
    match delivery {
        Ok(reactor_webrtc_dc_chunking::Delivery::Message { data, binary }) => {
            core.deliver(c, data, binary)
        }
        Ok(reactor_webrtc_dc_chunking::Delivery::Pending) => {}
        // Larger than this side's limit: dropped without buffering, and the
        // channel keeps working.
        Ok(reactor_webrtc_dc_chunking::Delivery::Dropped { .. }) => {}
        // The peer broke the wire format; the stream cannot be trusted.
        Err(_) => {
            c.reassembler.lock().unwrap().reset();
            core.raw.close();
        }
    }
}

extern "C" fn dc_on_state_change(ud: *mut c_void, state: c_int) {
    let core = unsafe { &*(ud as *const ChannelCore) };
    let state = DataChannelState::from_raw(state);
    match state {
        // Decide at the Open transition, so the framing is fixed before the
        // first message either way.
        DataChannelState::Open => {
            core.chunked();
        }
        DataChannelState::Closed => {
            if let Some(Some(c)) = core.decided.get() {
                c.queue.lock().unwrap().clear();
                c.reassembler.lock().unwrap().reset();
            }
        }
        _ => {}
    }
    if let Some(cb) = core.on_state_change.get() {
        (*cb.lock().unwrap())(state);
    }
    let event = match state {
        DataChannelState::Open => Some(&core.on_open),
        DataChannelState::Closed => Some(&core.on_close),
        _ => None,
    };
    if let Some(cb) = event.and_then(Slot::get) {
        (*cb.lock().unwrap())();
    }
}

extern "C" fn dc_on_buffered_amount_low(ud: *mut c_void) {
    let core = unsafe { &*(ud as *const ChannelCore) };
    match core.decided.get() {
        // The caller's own threshold is checked here, from the native event,
        // never from inside a send: like libwebrtc, `send` does not call back.
        Some(Some(c)) => {
            core.pump(c);
            core.maybe_fire_user_low(c);
        }
        _ => {
            if let Some(cb) = core.on_buffered_amount_low.get() {
                (*cb.lock().unwrap())();
            }
        }
    }
}

/// A data channel — either locally created or handed to `on_data_channel` by
/// the remote peer. Dropping releases the native handle.
///
/// On a channel whose connection negotiated chunking (see
/// [`is_chunked`](Self::is_chunked)), messages travel as frames: `send`
/// accepts payloads larger than libwebrtc's 16 MiB send buffer, queueing
/// them and feeding the native channel as it drains, and `on_message`
/// receives each message whole. Every other channel behaves exactly as a
/// plain libwebrtc data channel.
pub struct DataChannel {
    raw: *mut reactor_webrtc_sys::DataChannel,
    // Addressed by the native observer as `userdata`; boxed so its address
    // is stable for as long as the observer is registered.
    core: Box<ChannelCore>,
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
            core: Box::new(ChannelCore {
                raw: RawDc(raw),
                on_message: Slot::empty(),
                on_state_change: Slot::empty(),
                on_open: Slot::empty(),
                on_close: Slot::empty(),
                on_buffered_amount_low: Slot::empty(),
                registered: AtomicBool::new(false),
                negotiation: None,
                decided: std::sync::OnceLock::new(),
                early_low_threshold: AtomicU64::new(0),
            }),
            _factory: factory,
        }
    }

    pub(crate) fn with_dc_negotiation(
        mut self,
        negotiation: Arc<crate::dc_chunking::DcNegotiation>,
    ) -> Self {
        self.core.negotiation = Some(negotiation);
        self
    }

    /// Whether this channel carries chunked messages.
    ///
    /// Decided once — at the `Open` transition, or the first time it is
    /// asked while open — and fixed from then on: a channel is chunked when
    /// its connection negotiated chunking with the peer and the channel is
    /// ordered and fully reliable (no `maxRetransmits`, no
    /// `maxPacketLifeTime`). Both ends see the same SDP and the same channel
    /// parameters, so they reach the same answer without signalling anything
    /// else. `false` before the channel opens.
    pub fn is_chunked(&self) -> bool {
        self.core.chunked().is_some()
    }

    /// Whether the channel delivers messages in order.
    pub fn ordered(&self) -> bool {
        unsafe { reactor_webrtc_sys::reactor_webrtc_data_channel_ordered(self.raw) != 0 }
    }

    /// Whether the channel retransmits until delivery: neither
    /// `maxRetransmits` nor `maxPacketLifeTime` is set.
    pub fn reliable(&self) -> bool {
        unsafe { reactor_webrtc_sys::reactor_webrtc_data_channel_reliable(self.raw) != 0 }
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
        self.core.raw.state()
    }

    /// Bytes waiting to be sent (backpressure signal). On a chunked channel
    /// this includes what is still queued beyond libwebrtc's own buffer.
    pub fn buffered_amount(&self) -> u64 {
        self.core.buffered_amount()
    }

    /// Set the threshold below which `on_buffered_amount_low` fires. On a
    /// chunked channel it applies to [`buffered_amount`](Self::buffered_amount),
    /// queue included.
    pub fn set_buffered_amount_low_threshold(&self, threshold: u64) {
        self.core
            .early_low_threshold
            .store(threshold, Ordering::SeqCst);
        match self.core.chunked() {
            Some(c) => c.user_low_threshold.store(threshold, Ordering::SeqCst),
            None => unsafe {
                reactor_webrtc_sys::reactor_webrtc_data_channel_set_low_threshold(
                    self.raw, threshold,
                )
            },
        }
    }

    /// Send a message. `binary` selects the message type the receiver sees.
    ///
    /// On a chunked channel the message is queued and leaves as frames, so
    /// it may be larger than libwebrtc's 16 MiB send buffer, up to the
    /// effective max message size; a full queue or an oversized message is
    /// an [`Error::DataChannel`] and nothing is sent. Elsewhere this is a
    /// plain libwebrtc send.
    pub fn send(&self, data: &[u8], binary: bool) -> Result<()> {
        let Some(c) = self.core.chunked() else {
            return if self.core.raw.send(data, binary) {
                Ok(())
            } else {
                Err(Error::Webrtc("data channel send failed".into()))
            };
        };
        // A closed channel would take the message and drop it on the first
        // native send; refuse it as the plain path does.
        if self.state() != DataChannelState::Open {
            return Err(Error::Webrtc("data channel send failed".into()));
        }
        c.queue
            .lock()
            .unwrap()
            .push(data.to_vec(), binary)
            .map_err(Error::DataChannel)?;
        c.user_low_armed.store(true, Ordering::SeqCst);
        self.core.pump(c);
        Ok(())
    }

    /// Block until everything queued has been handed to SCTP and libwebrtc's
    /// own buffer is empty, or `timeout` passes. Returns whether it drained.
    ///
    /// Don't call it (or [`close`](Self::close) with a drain timeout) from
    /// this channel's callbacks: they run on libwebrtc's network thread, which
    /// is the thread that sends, so nothing drains until the timeout passes.
    pub fn drain(&self, timeout: std::time::Duration) -> bool {
        // A timeout too large to add to now never expires.
        let deadline = std::time::Instant::now().checked_add(timeout);
        loop {
            if self.buffered_amount() == 0 {
                return true;
            }
            let expired = deadline.is_some_and(|d| std::time::Instant::now() >= d);
            if self.state() != DataChannelState::Open || expired {
                return false;
            }
            // Refill here too, rather than only from the native low-water
            // event, so a drain never waits on an event it already missed.
            if let Some(c) = self.core.chunked() {
                self.core.pump(c);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Close the channel. A chunked channel first drains what it has queued,
    /// for up to `drain_timeout`, so a message already accepted by `send`
    /// is not cut short.
    pub fn close(&self, drain_timeout: std::time::Duration) {
        if self.core.chunked().is_some() {
            self.drain(drain_timeout);
        }
        self.core.raw.close();
    }

    /// Receive handler — fires on every incoming message. The closure runs on
    /// a WebRTC network thread; return quickly or offload heavy work. On a
    /// chunked channel it fires once per whole message, including any that
    /// arrived before it was set.
    pub fn on_message(&self, cb: impl for<'a> FnMut(&'a [u8], bool) + Send + 'static) {
        // The new callback is published already locked, and the held messages
        // are taken out under the slot's lock: a message that arrives while
        // they are flushed waits on the callback, so the order holds, and the
        // callback can still replace any handler of this channel.
        let shared: Shared<MessageCb> = Arc::new(Mutex::new(Box::new(cb)));
        let mut running = shared.lock().unwrap();
        let held = {
            let mut slot = self.core.on_message.0.lock().unwrap();
            *slot = Some(Arc::clone(&shared));
            match self.core.decided.get() {
                Some(Some(c)) => {
                    c.pending_bytes.store(0, Ordering::SeqCst);
                    std::mem::take(&mut *c.pending.lock().unwrap())
                }
                _ => std::collections::VecDeque::new(),
            }
        };
        for (data, binary) in held {
            (*running)(&data, binary);
        }
        drop(running);
        self.reregister();
    }

    /// State-change handler — fires for every transition including
    /// Connecting → Open → Closing → Closed.
    pub fn on_state_change(&self, cb: impl FnMut(DataChannelState) + Send + 'static) {
        self.core.on_state_change.set(Box::new(cb));
        self.reregister();
    }

    /// Fires when the channel becomes `Open`. Independent of
    /// [`on_state_change`](Self::on_state_change) and [`on_close`](Self::on_close):
    /// setting one does not replace the others.
    pub fn on_open(&self, cb: impl FnMut() + Send + 'static) {
        self.core.on_open.set(Box::new(cb));
        self.reregister();
    }

    /// Fires when the channel reaches `Closed`. Independent of
    /// [`on_state_change`](Self::on_state_change) and [`on_open`](Self::on_open).
    pub fn on_close(&self, cb: impl FnMut() + Send + 'static) {
        self.core.on_close.set(Box::new(cb));
        self.reregister();
    }

    /// Flow-control handler — fires when `buffered_amount` drops at or below
    /// the threshold set by [`set_buffered_amount_low_threshold`](Self::set_buffered_amount_low_threshold).
    ///
    /// Like libwebrtc's, it never fires from inside [`send`](Self::send), so
    /// it may call `send` to refill. On a chunked channel it is checked when
    /// libwebrtc's buffer falls to the queue's low-water mark.
    pub fn on_buffered_amount_low(&self, cb: impl FnMut() + Send + 'static) {
        self.core.on_buffered_amount_low.set(Box::new(cb));
        self.reregister();
    }

    // The native observer is registered once, by whichever comes first: a
    // setter, or a chunked channel deciding. It addresses the channel's core
    // and every callback reads its slot when it fires, so a later setter only
    // swaps the slot. Re-registering would wait on the signaling thread, which
    // runs the callbacks: a setter called from a callback while another
    // thread re-registered deadlocked.
    fn reregister(&self) {
        self.core.ensure_registered();
    }
}

impl Drop for DataChannel {
    fn drop(&mut self) {
        // Unregisters the native observer before the core box is freed.
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

/// One of [`ReactorStatEntry`]'s fixed string buffers; `None` when empty, which
/// is how the glue says absent.
fn stat_str(buf: &[c_char]) -> Option<String> {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

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
                kind: StreamKind::from_raw(e.stream_kind),
                mid: stat_str(&e.mid),
                codec_mime_type: stat_str(&e.codec_mime_type),
                packets_received: e.packets_received,
                bytes_received: e.bytes_received,
                jitter_s: e.jitter,
                packets_lost: e.packets_lost,
                nack_count: e.nack_count,
                pli_count: e.pli_count,
                fir_count: e.fir_count,
                total_decode_time_s: e.total_decode_time,
                frames_per_second: e.frames_per_second,
                frames_decoded: e.frames_decoded,
                frames_dropped: e.frames_dropped,
                frame_width: e.frame_width,
                frame_height: e.frame_height,
                jitter_buffer_delay_s: e.jitter_buffer_delay,
                jitter_buffer_target_delay_s: e.jitter_buffer_target_delay,
                jitter_buffer_minimum_delay_s: e.jitter_buffer_minimum_delay,
                jitter_buffer_emitted_count: e.jitter_buffer_emitted_count,
                total_processing_delay_s: e.total_processing_delay,
                timing_frame: (e.timing_frame_present != 0).then_some(TimingFrameInfo {
                    rtp_timestamp: e.timing_frame_rtp_timestamp,
                    is_outlier: e.timing_is_outlier != 0,
                    is_timer_triggered: e.timing_is_timer_triggered != 0,
                    sender: TimingFrameSenderTimestamps {
                        capture_ms: e.timing_capture_ms,
                        encode_start_ms: e.timing_encode_start_ms,
                        encode_finish_ms: e.timing_encode_finish_ms,
                        packetization_finish_ms: e.timing_packetization_finish_ms,
                        pacer_exit_ms: e.timing_pacer_exit_ms,
                    },
                    receiver: TimingFrameReceiverTimestamps {
                        receive_start_ms: e.timing_receive_start_ms,
                        receive_finish_ms: e.timing_receive_finish_ms,
                        decode_start_ms: e.timing_decode_start_ms,
                        decode_finish_ms: e.timing_decode_finish_ms,
                    },
                }),
            }),
            1 => report.outbound_rtp.push(OutboundRtpStats {
                ssrc: e.ssrc,
                kind: StreamKind::from_raw(e.stream_kind),
                mid: stat_str(&e.mid),
                codec_mime_type: stat_str(&e.codec_mime_type),
                packets_sent: e.packets_sent,
                bytes_sent: e.bytes_sent,
                target_bitrate_bps: e.target_bitrate,
                round_trip_time_s: e.round_trip_time,
                total_round_trip_time_s: e.total_round_trip_time,
                fraction_lost: e.fraction_lost,
                packets_lost: e.packets_lost,
                retransmitted_packets_sent: e.retransmitted_packets_sent,
                nack_count: e.nack_count,
                pli_count: e.pli_count,
                fir_count: e.fir_count,
                frames_per_second: e.frames_per_second,
                frames_sent: e.frames_sent,
                frame_width: e.frame_width,
                frame_height: e.frame_height,
                frames_encoded: e.frames_encoded,
                total_encode_time_s: e.total_encode_time,
                total_packet_send_delay_s: e.total_packet_send_delay,
            }),
            2 => report.candidate_pairs.push(IceCandidatePairStats {
                current_round_trip_time_s: e.current_round_trip_time,
                total_round_trip_time_s: e.total_round_trip_time,
                priority: e.priority,
                state: IceCandidatePairState::from_raw(e.pair_state),
                nominated: e.nominated != 0,
                writable: e.writable != 0,
                available_outgoing_bitrate_bps: e.available_outgoing_bitrate,
                available_incoming_bitrate_bps: e.available_incoming_bitrate,
                bytes_sent: e.bytes_sent,
                bytes_received: e.bytes_received,
                packets_sent: e.packets_sent,
                // The pair's own 64-bit counter, not kind 0's 32-bit one.
                packets_received: e.pair_packets_received,
                local_candidate_type: IceCandidateType::from_raw(e.local_candidate_type),
                local_relay_protocol: RelayProtocol::from_raw(e.local_relay_protocol),
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
    // The factory's DcChunking (unless RtcConfiguration::dc_chunking opted
    // this connection out) and, once negotiated, the peer's parameters.
    // Shared with the observer and every data channel.
    dc_negotiation: Arc<crate::dc_chunking::DcNegotiation>,
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
        dc_negotiation: Arc<crate::dc_chunking::DcNegotiation>,
    ) -> Self {
        Self {
            raw,
            _observer: observer,
            _factory: factory,
            frame_metadata_gate: crate::metadata::FrameMetadataGate::new(),
            frame_metadata_enabled,
            dc_negotiation,
        }
    }

    /// The chunking settings this connection takes part with: its factory's
    /// [`DcChunking`](crate::DcChunking), or `None` when the factory does not
    /// enable chunking or this connection opted out. A channel is only
    /// chunked when the peer declares chunking too.
    pub fn dc_chunking(&self) -> Option<&crate::DcChunking> {
        self.dc_negotiation.settings()
    }

    /// Whether chunking was negotiated: this connection takes part, and the
    /// first offer/answer round to complete declared it on both sides. Fixed
    /// from then on; a later renegotiation neither adds nor drops it. Each
    /// channel still decides for itself: see [`DataChannel::is_chunked`].
    pub fn dc_chunking_negotiated(&self) -> bool {
        self.dc_negotiation.remote().is_some()
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
        let offer = if self.frame_metadata_enabled {
            offer.with_frame_metadata()
        } else {
            offer
        };
        // Declared whenever this connection takes part in chunking.
        Ok(self.dc_negotiation.offer(offer))
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
        let answer = if self.frame_metadata_gate.is_open() {
            answer.with_frame_metadata()
        } else {
            answer
        };
        // Mirrors the offer: declared only when the offer declared chunking
        // and this connection takes part.
        Ok(self.dc_negotiation.answer(answer))
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
        self.dc_negotiation.on_local_description(sdp);
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
        self.dc_negotiation.on_remote_description(sdp);
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
            Ok(DataChannel::from_raw(raw, Arc::clone(&self._factory))
                .with_dc_negotiation(Arc::clone(&self.dc_negotiation)))
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
