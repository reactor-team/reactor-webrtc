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
    // Held across a description's record, native apply and undo; see
    // `apply`. Channels read `state` alone, never this.
    applying: Mutex<()>,
}

/// The bookkeeping a description moves, as one value so that a description
/// libwebrtc rejects can be undone in one step.
#[derive(Clone, Copy, Default)]
struct NegotiationState {
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

/// How a description's native apply failed.
pub(crate) enum ApplyError {
    /// libwebrtc refused the description: it was never applied.
    Rejected(Error),
    /// libwebrtc did not answer in time. It may still apply the description.
    Unconfirmed(Error),
}

impl ApplyError {
    fn into_error(self) -> Error {
        match self {
            ApplyError::Rejected(e) | ApplyError::Unconfirmed(e) => e,
        }
    }
}

impl DcNegotiation {
    pub(crate) fn new(settings: Option<DcChunking>) -> Arc<Self> {
        Arc::new(Self {
            settings,
            state: Mutex::new(NegotiationState::default()),
            applying: Mutex::new(()),
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

    /// Apply a local description through `native`, tracking it here first.
    /// See [`apply`](Self::apply).
    pub(crate) fn apply_local(
        &self,
        local: &SessionDescription,
        native: impl FnOnce() -> std::result::Result<(), ApplyError>,
    ) -> Result<()> {
        self.apply(|state| state.on_local_description(local), native)
    }

    /// Apply a remote description through `native`, tracking it here first.
    /// See [`apply`](Self::apply).
    pub(crate) fn apply_remote(
        &self,
        remote: &SessionDescription,
        native: impl FnOnce() -> std::result::Result<(), ApplyError>,
    ) -> Result<()> {
        self.apply(|state| state.on_remote_description(remote), native)
    }

    /// Record a description, then hand it to libwebrtc, undoing the record
    /// if libwebrtc rejects it.
    ///
    /// The record comes *before* the native apply, not after: once libwebrtc
    /// has the description, the SCTP association can come up and a channel
    /// open and decide at any moment. A channel that decided before the round
    /// was recorded would stay plain for its whole life while its twin framed,
    /// and every message between them would be lost.
    ///
    /// A rejected description was never applied, so it must not move the
    /// negotiation. No channel can have decided from it in the meantime: a
    /// rejected description brings no association up, and a connection that
    /// already has an open channel settled in an earlier round, which a later
    /// description cannot change. An apply that timed out is not a rejection:
    /// libwebrtc may still apply the description, and the record stays.
    ///
    /// The mirror case is accepted, not missed: if libwebrtc later rejects a
    /// description whose apply timed out, the record stays settled from a
    /// description that never ran, and a retried round cannot change it. That
    /// description brought no association up, so no channel decided from it;
    /// channels decide once a retried round brings one up, and between the same
    /// two peers that round declares chunking exactly as the one that timed out
    /// did, so both ends still agree. The alternative, undoing on a timeout,
    /// reopens the race this ordering closes whenever libwebrtc does apply it.
    ///
    /// The whole record–apply–undo runs under one lock, so two descriptions
    /// applied from different threads cannot interleave: an undo never
    /// discards a description that libwebrtc accepted in between, nor does
    /// one description build on another's unconfirmed record.
    fn apply(
        &self,
        record: impl FnOnce(&mut NegotiationState),
        native: impl FnOnce() -> std::result::Result<(), ApplyError>,
    ) -> Result<()> {
        if self.settings.is_none() {
            return native().map_err(ApplyError::into_error);
        }
        let _applying = self.applying.lock().unwrap();
        let before = {
            let mut state = self.state.lock().unwrap();
            let before = *state;
            record(&mut state);
            before
        };
        native().map_err(|e| {
            if let ApplyError::Rejected(_) = e {
                *self.state.lock().unwrap() = before;
            }
            e.into_error()
        })
    }

    /// Track a description this side applied locally. Tests drive the
    /// bookkeeping through this, without a native apply.
    #[cfg(test)]
    fn on_local_description(&self, local: &SessionDescription) {
        self.state.lock().unwrap().on_local_description(local);
    }

    /// Track a description the peer's side applied here. Tests drive the
    /// bookkeeping through this, without a native apply.
    #[cfg(test)]
    fn on_remote_description(&self, remote: &SessionDescription) {
        self.state.lock().unwrap().on_remote_description(remote);
    }
}

impl NegotiationState {
    fn settle(&mut self, params: Option<Params>) {
        if !self.settled {
            self.settled = true;
            self.remote = params;
        }
    }

    /// An answer that declares chunking, to an offer that did, completes the
    /// round on the answerer's side.
    fn on_local_description(&mut self, local: &SessionDescription) {
        let declares = sdp::has_attribute(&local.sdp);
        match local.kind {
            SdpType::Offer => self.local_offer_declared = declares,
            SdpType::Answer => {
                let pending = self.pending.take();
                self.settle(pending.filter(|_| declares));
            }
            SdpType::Rollback => self.local_offer_declared = false,
            SdpType::PrAnswer => {}
        }
    }

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
    fn on_remote_description(&mut self, remote: &SessionDescription) {
        let parsed = sdp::parse(&remote.sdp);
        match remote.kind {
            SdpType::Offer => self.pending = parsed,
            SdpType::Answer => {
                let offered = self.local_offer_declared;
                self.settle(parsed.filter(|_| offered));
            }
            SdpType::Rollback => self.pending = None,
            SdpType::PrAnswer => {}
        }
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

    fn rejected() -> std::result::Result<(), ApplyError> {
        Err(ApplyError::Rejected(Error::Webrtc("rejected".into())))
    }

    fn timed_out() -> std::result::Result<(), ApplyError> {
        Err(ApplyError::Unconfirmed(Error::Webrtc("timed out".into())))
    }

    #[test]
    fn the_round_is_recorded_before_the_native_apply_runs() {
        let offerer = negotiation();
        offerer.on_local_description(&desc(SdpType::Offer, true));
        offerer
            .apply_remote(&desc(SdpType::Answer, true), || {
                assert!(offerer.remote().is_some(), "a channel opening now");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_rejected_answer_leaves_the_round_open() {
        let offerer = negotiation();
        offerer.on_local_description(&desc(SdpType::Offer, true));
        // Without the attribute: recorded, it would settle the offerer plain.
        assert!(offerer
            .apply_remote(&desc(SdpType::Answer, false), rejected)
            .is_err());
        offerer
            .apply_remote(&desc(SdpType::Answer, true), || Ok(()))
            .unwrap();
        assert!(offerer.remote().is_some());
    }

    #[test]
    fn an_answer_that_timed_out_stays_recorded() {
        // libwebrtc may still apply it, and a channel may then open.
        let offerer = negotiation();
        offerer.on_local_description(&desc(SdpType::Offer, true));
        assert!(offerer
            .apply_remote(&desc(SdpType::Answer, true), timed_out)
            .is_err());
        assert!(offerer.remote().is_some());
    }

    #[test]
    fn a_rejected_offer_is_not_pending() {
        let answerer = negotiation();
        assert!(answerer
            .apply_remote(&desc(SdpType::Offer, true), rejected)
            .is_err());
        assert!(!answerer
            .answer(desc(SdpType::Answer, false))
            .declares_dc_chunking());
    }

    #[test]
    fn an_undo_never_discards_a_description_applied_meanwhile() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let offerer = negotiation();
        offerer.on_local_description(&desc(SdpType::Offer, true));
        let (started, wait_started) = mpsc::channel();
        let (release, wait_release) = mpsc::channel::<()>();
        let first = thread::spawn({
            let offerer = offerer.clone();
            move || {
                offerer.apply_remote(&desc(SdpType::Answer, false), || {
                    started.send(()).unwrap();
                    wait_release.recv().unwrap();
                    rejected()
                })
            }
        });
        wait_started.recv().unwrap();
        let second = thread::spawn({
            let offerer = offerer.clone();
            move || offerer.apply_remote(&desc(SdpType::Answer, true), || Ok(()))
        });
        // The second apply waits for the first to finish, undo included.
        thread::sleep(Duration::from_millis(50));
        assert!(!second.is_finished());
        release.send(()).unwrap();
        assert!(first.join().unwrap().is_err());
        second.join().unwrap().unwrap();
        assert!(offerer.remote().is_some());
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
