//! [`DcChunking`] — the factory-wide settings for large data-channel messages.

use std::sync::{Arc, Mutex};

use reactor_webrtc_dc_chunking::sdp::{self, Params};

use crate::{Error, Result, SdpType, SessionDescription};

/// Settings for chunked data channels, set on a factory with
/// [`PeerConnectionFactoryBuilder::with_dc_chunking`](crate::PeerConnectionFactoryBuilder::with_dc_chunking).
///
/// `#[non_exhaustive]` — construct via `Default` and assign the fields you
/// want to change:
///
/// ```rust,ignore
/// let mut chunking = DcChunking::default();
/// chunking.max_message_size = 16 * 1024 * 1024;
/// let factory = PeerConnectionFactory::builder().with_dc_chunking(chunking).build()?;
/// ```
///
/// Two things change on a factory built with it:
///
/// - **`max_burst`** applies to every data channel of the factory, chunked or
///   not. dcsctp sends at most this many packets per `send()` and per SACK;
///   upstream's 4 makes a message larger than ~4.6 KB ramp up over one round
///   trip per doubling of its size. The default, 256, lets a message up to the
///   256 KiB data-channel limit leave in one flight, bounded as always by the
///   congestion window.
/// - **Chunking** is offered to peers, through the `a=x-reactor-dc-chunking`
///   SDP attribute. A channel is chunked only when both peers declare it; with
///   any other peer it behaves exactly as it does without this setting.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcChunking {
    /// dcsctp's per-event packet limit (libwebrtc patch 0005). Default 256.
    pub max_burst: u32,
    /// Largest message this side sends or accepts on a chunked channel, and
    /// the receive limit it advertises to the peer. Default 64 MiB.
    pub max_message_size: u64,
    /// Bytes a chunked channel may queue beyond libwebrtc's own send buffer;
    /// at least `max_message_size`. Default 128 MiB.
    pub send_buffer_limit: u64,
    /// Frame size on the wire, header included. Default 64 KiB.
    pub chunk_size: usize,
}

impl Default for DcChunking {
    fn default() -> Self {
        Self {
            max_burst: 256,
            max_message_size: reactor_webrtc_dc_chunking::DEFAULT_MAX_MESSAGE_SIZE,
            send_buffer_limit: reactor_webrtc_dc_chunking::DEFAULT_SEND_BUFFER_LIMIT,
            chunk_size: reactor_webrtc_dc_chunking::DEFAULT_CHUNK_SIZE,
        }
    }
}

impl DcChunking {
    /// The send-queue configuration for a channel whose peer declared `remote`.
    pub(crate) fn send_config(&self, remote: &Params) -> reactor_webrtc_dc_chunking::SendConfig {
        reactor_webrtc_dc_chunking::SendConfig {
            chunk_size: self.chunk_size,
            queue_limit: self.send_buffer_limit,
            max_message_size: sdp::effective_max_message_size(self.max_message_size, remote),
            ..reactor_webrtc_dc_chunking::SendConfig::default()
        }
    }

    /// Reject settings that cannot work, before any factory exists.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.max_burst == 0 || self.max_burst > i32::MAX as u32 {
            return Err(Error::Webrtc(format!(
                "dc chunking: max_burst {} must be between 1 and {}",
                self.max_burst,
                i32::MAX
            )));
        }
        let config = reactor_webrtc_dc_chunking::SendConfig {
            chunk_size: self.chunk_size,
            queue_limit: self.send_buffer_limit,
            max_message_size: self.max_message_size,
            ..reactor_webrtc_dc_chunking::SendConfig::default()
        };
        config
            .validate()
            .map_err(|e| Error::Webrtc(format!("dc chunking: {e}")))
    }
}

/// What one peer connection knows about data-channel chunking: its own
/// settings, and the peer's parameters once the peer has declared chunking.
/// Shared by the connection, its observer (for channels the peer opens) and
/// every channel, so they all read the same negotiation.
pub(crate) struct DcNegotiation {
    settings: Option<DcChunking>,
    state: Mutex<NegotiationState>,
}

/// The bookkeeping a description moves, as one value so that a description
/// libwebrtc rejects can be undone in one step.
#[derive(Clone, Copy, Default)]
pub(crate) struct NegotiationState {
    // The negotiation, settled by the first offer/answer round to complete:
    // the peer's parameters when both sides declared chunking in it.
    remote: Option<Params>,
    settled: bool,
    // The latest remote offer's parameters, while it awaits our answer. An
    // answer mirrors the offer it answers, so this is not sticky.
    pending: Option<Params>,
    // Whether the offer this side last applied locally declared chunking.
    local_offer_declared: bool,
}

impl NegotiationState {
    fn settle(&mut self, params: Option<Params>) {
        if !self.settled {
            self.settled = true;
            self.remote = params;
        }
    }
}

impl DcNegotiation {
    pub(crate) fn new(settings: Option<DcChunking>) -> Arc<Self> {
        Arc::new(Self {
            settings,
            state: Mutex::new(NegotiationState::default()),
        })
    }

    pub(crate) fn settings(&self) -> Option<&DcChunking> {
        self.settings.as_ref()
    }

    /// The peer's parameters, once negotiated. `None` while this connection
    /// does not take part or no round has completed with chunking.
    pub(crate) fn remote(&self) -> Option<Params> {
        self.state.lock().unwrap().remote
    }

    /// An offer declares chunking whenever this connection takes part.
    pub(crate) fn offer(&self, offer: SessionDescription) -> SessionDescription {
        match &self.settings {
            Some(s) => offer.with_dc_chunking(&Params::local(s.max_message_size)),
            None => offer,
        }
    }

    /// An answer declares it only when the offer it answers did:
    /// offer/answer cannot introduce a capability the offerer never asked
    /// for, even on a renegotiation of a connection that already chunks.
    pub(crate) fn answer(&self, answer: SessionDescription) -> SessionDescription {
        match &self.settings {
            Some(s) if self.state.lock().unwrap().pending.is_some() => {
                answer.with_dc_chunking(&Params::local(s.max_message_size))
            }
            _ => answer,
        }
    }

    /// Track a description this side is about to apply locally. Returns the
    /// state before it, for [`restore`](Self::restore) should libwebrtc
    /// reject the description.
    ///
    /// Called *before* the native apply, not after: once libwebrtc has the
    /// description, the SCTP association can come up and a channel open and
    /// decide at any moment. A channel that decided before the round was
    /// recorded here would stay plain for its whole life while its twin
    /// framed, and every message between them would be lost.
    ///
    /// An answer that declares chunking, to an offer that did, completes the
    /// round on the answerer's side.
    pub(crate) fn on_local_description(&self, local: &SessionDescription) -> NegotiationState {
        let mut state = self.state.lock().unwrap();
        let before = *state;
        if self.settings.is_none() {
            return before;
        }
        let declares = sdp::has_attribute(&local.sdp);
        match local.kind {
            SdpType::Offer => state.local_offer_declared = declares,
            SdpType::Answer => {
                let pending = state.pending.take();
                state.settle(pending.filter(|_| declares));
            }
            SdpType::Rollback => state.local_offer_declared = false,
            SdpType::PrAnswer => {}
        }
        before
    }

    /// Track a description the peer's side is about to apply here. Returns
    /// the state before it, for [`restore`](Self::restore); called before the
    /// native apply for the reason given on
    /// [`on_local_description`](Self::on_local_description).
    ///
    /// An offer only becomes pending: it may still be rolled back, or
    /// answered without the attribute. A final answer that declares chunking,
    /// to an offer of ours that did, completes the round on the offerer's
    /// side.
    ///
    /// Settled by the first round to complete, either way, and fixed for the
    /// connection's life: a later renegotiation neither adds nor drops it.
    /// Each end records that round before it hands the description to
    /// libwebrtc, so before any SCTP data can flow, and a channel reaches the
    /// same decision as its twin whenever it is asked. The one way the ends
    /// can still disagree is an answer that loses the attribute between the
    /// answerer and the offerer.
    pub(crate) fn on_remote_description(&self, remote: &SessionDescription) -> NegotiationState {
        let mut state = self.state.lock().unwrap();
        let before = *state;
        if self.settings.is_none() {
            return before;
        }
        let parsed = sdp::parse(&remote.sdp);
        match remote.kind {
            SdpType::Offer => state.pending = parsed,
            SdpType::Answer => {
                let offered = state.local_offer_declared;
                state.settle(parsed.filter(|_| offered));
            }
            SdpType::Rollback => state.pending = None,
            SdpType::PrAnswer => {}
        }
        before
    }

    /// Undo a description libwebrtc rejected: it was never applied, so it
    /// must not move the negotiation. No channel can have decided from it in
    /// the meantime — a rejected description brings no association up, and a
    /// connection that already has an open channel settled in an earlier
    /// round, which a later description cannot change.
    pub(crate) fn restore(&self, before: NegotiationState) {
        *self.state.lock().unwrap() = before;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(kind: SdpType, declares: bool) -> SessionDescription {
        let mut sdp =
            "v=0\r\ns=-\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n".to_owned();
        if declares {
            sdp = sdp::declare(&sdp, &Params::local(1024));
        }
        SessionDescription { kind, sdp }
    }

    fn negotiation() -> Arc<DcNegotiation> {
        DcNegotiation::new(Some(DcChunking::default()))
    }

    #[test]
    fn a_completed_round_with_the_attribute_establishes_both_ends() {
        let (offerer, answerer) = (negotiation(), negotiation());
        offerer.on_local_description(&desc(SdpType::Offer, true));
        answerer.on_remote_description(&desc(SdpType::Offer, true));
        assert!(
            answerer.remote().is_none(),
            "an offer alone settles nothing"
        );
        let answer = answerer.answer(desc(SdpType::Answer, false));
        assert!(answer.declares_dc_chunking());
        answerer.on_local_description(&answer);
        offerer.on_remote_description(&answer);
        assert!(offerer.remote().is_some() && answerer.remote().is_some());
    }

    #[test]
    fn a_rolled_back_offer_leaves_nothing_established() {
        let answerer = negotiation();
        answerer.on_remote_description(&desc(SdpType::Offer, true));
        answerer.on_remote_description(&desc(SdpType::Rollback, false));
        answerer.on_remote_description(&desc(SdpType::Offer, false));
        let answer = answerer.answer(desc(SdpType::Answer, false));
        assert!(!answer.declares_dc_chunking());
        answerer.on_local_description(&answer);
        assert!(answerer.remote().is_none());
    }

    #[test]
    fn an_answer_without_the_attribute_establishes_neither_end() {
        let (offerer, answerer) = (negotiation(), negotiation());
        offerer.on_local_description(&desc(SdpType::Offer, true));
        answerer.on_remote_description(&desc(SdpType::Offer, true));
        // The answer is stripped before either end applies it.
        let answer = desc(SdpType::Answer, false);
        answerer.on_local_description(&answer);
        offerer.on_remote_description(&answer);
        assert!(offerer.remote().is_none() && answerer.remote().is_none());
    }

    #[test]
    fn an_offerer_whose_offer_lost_the_attribute_ignores_a_declaring_answer() {
        let offerer = negotiation();
        offerer.on_local_description(&desc(SdpType::Offer, false));
        offerer.on_remote_description(&desc(SdpType::Answer, true));
        assert!(offerer.remote().is_none());
    }

    #[test]
    fn a_later_round_cannot_add_chunking_to_a_connection_settled_without_it() {
        let (offerer, answerer) = (negotiation(), negotiation());
        // First round: the answer comes back without the attribute.
        offerer.on_local_description(&desc(SdpType::Offer, true));
        answerer.on_remote_description(&desc(SdpType::Offer, false));
        let first = answerer.answer(desc(SdpType::Answer, false));
        answerer.on_local_description(&first);
        offerer.on_remote_description(&first);
        // Second round: both declare it.
        offerer.on_local_description(&desc(SdpType::Offer, true));
        answerer.on_remote_description(&desc(SdpType::Offer, true));
        let second = answerer.answer(desc(SdpType::Answer, false));
        assert!(second.declares_dc_chunking(), "the answer still mirrors");
        answerer.on_local_description(&second);
        offerer.on_remote_description(&second);
        assert!(offerer.remote().is_none() && answerer.remote().is_none());
    }

    #[test]
    fn a_completed_round_is_recorded_before_it_is_applied() {
        // Each end must know the outcome as soon as it hands the description
        // to libwebrtc: a channel can open and decide right after.
        let (offerer, answerer) = (negotiation(), negotiation());
        offerer.on_local_description(&desc(SdpType::Offer, true));
        answerer.on_remote_description(&desc(SdpType::Offer, true));
        let answer = answerer.answer(desc(SdpType::Answer, false));
        answerer.on_local_description(&answer);
        assert!(answerer.remote().is_some());
        offerer.on_remote_description(&answer);
        assert!(offerer.remote().is_some());
    }

    #[test]
    fn a_restored_description_leaves_the_round_open() {
        let offerer = negotiation();
        offerer.on_local_description(&desc(SdpType::Offer, true));
        // An answer without the attribute, which libwebrtc then rejects.
        let before = offerer.on_remote_description(&desc(SdpType::Answer, false));
        offerer.restore(before);
        // The real answer still settles the round.
        offerer.on_remote_description(&desc(SdpType::Answer, true));
        assert!(offerer.remote().is_some());
    }

    #[test]
    fn a_restored_offer_is_no_longer_pending() {
        let answerer = negotiation();
        let before = answerer.on_remote_description(&desc(SdpType::Offer, true));
        answerer.restore(before);
        assert!(!answerer
            .answer(desc(SdpType::Answer, false))
            .declares_dc_chunking());
    }

    #[test]
    fn default_is_valid() {
        assert!(DcChunking::default().validate().is_ok());
    }

    #[test]
    fn rejects_bad_settings() {
        let bad = |f: fn(&mut DcChunking)| {
            let mut c = DcChunking::default();
            f(&mut c);
            c.validate().is_err()
        };
        assert!(bad(|c| c.max_burst = 0));
        assert!(bad(|c| c.max_burst = i32::MAX as u32 + 1));
        assert!(bad(|c| c.chunk_size = 1));
        assert!(bad(|c| c.chunk_size = 1 << 20));
        assert!(bad(|c| c.send_buffer_limit = c.max_message_size - 1));
    }

    #[test]
    fn send_config_takes_the_smaller_message_limit() {
        let c = DcChunking {
            max_message_size: 1000,
            ..Default::default()
        };
        assert_eq!(c.send_config(&Params::local(500)).max_message_size, 500);
        assert_eq!(c.send_config(&Params::local(5000)).max_message_size, 1000);
        assert_eq!(
            c.send_config(&Params {
                version: 1,
                max_message_size: None
            })
            .max_message_size,
            1000.min(reactor_webrtc_dc_chunking::LEGACY_MAX_MESSAGE_SIZE)
        );
    }
}
