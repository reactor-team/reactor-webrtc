//! [`DcChunking`] — the factory-wide settings for large data-channel messages.

use std::sync::atomic::{AtomicBool, Ordering};
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
    // The established negotiation: set once an offer/answer round completes
    // with the attribute on both sides, and sticky from then on.
    remote: Mutex<Option<Params>>,
    // The latest remote offer's parameters, while it awaits our answer. An
    // answer mirrors the offer it answers, so this is not sticky.
    pending: Mutex<Option<Params>>,
    // Whether the offer this side last applied locally declared chunking.
    local_offer_declared: AtomicBool,
}

impl DcNegotiation {
    pub(crate) fn new(settings: Option<DcChunking>) -> Arc<Self> {
        Arc::new(Self {
            settings,
            remote: Mutex::new(None),
            pending: Mutex::new(None),
            local_offer_declared: AtomicBool::new(false),
        })
    }

    pub(crate) fn settings(&self) -> Option<&DcChunking> {
        self.settings.as_ref()
    }

    /// The peer's parameters, once negotiated. `None` while this connection
    /// does not take part or no round has completed with chunking.
    pub(crate) fn remote(&self) -> Option<Params> {
        *self.remote.lock().unwrap()
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
            Some(s) if self.pending.lock().unwrap().is_some() => {
                answer.with_dc_chunking(&Params::local(s.max_message_size))
            }
            _ => answer,
        }
    }

    /// Track a description this side applied locally.
    ///
    /// An answer that declares chunking, to an offer that did, completes the
    /// round on the answerer's side.
    pub(crate) fn on_local_description(&self, local: &SessionDescription) {
        if self.settings.is_none() {
            return;
        }
        let declares = sdp::has_attribute(&local.sdp);
        match local.kind {
            SdpType::Offer => self.local_offer_declared.store(declares, Ordering::SeqCst),
            SdpType::Answer => {
                let pending = self.pending.lock().unwrap().take();
                if declares {
                    self.establish(pending);
                }
            }
            SdpType::Rollback => self.local_offer_declared.store(false, Ordering::SeqCst),
            SdpType::PrAnswer => {}
        }
    }

    /// Track a description the peer's side applied here.
    ///
    /// An offer only becomes pending: it may still be rolled back, or
    /// answered without the attribute. A final answer that declares chunking,
    /// to an offer of ours that did, completes the round on the offerer's
    /// side.
    ///
    /// Sticky once established: a later renegotiation that drops the
    /// attribute does not undo it. Channels already open keep their framing
    /// either way, and both ends must keep agreeing for new ones; each end
    /// completes the round before any SCTP data can flow, so both have
    /// settled before the first channel opens. The one way the ends can
    /// still disagree is an answer that loses the attribute between the
    /// answerer and the offerer.
    pub(crate) fn on_remote_description(&self, remote: &SessionDescription) {
        if self.settings.is_none() {
            return;
        }
        let parsed = sdp::parse(&remote.sdp);
        match remote.kind {
            SdpType::Offer => *self.pending.lock().unwrap() = parsed,
            SdpType::Answer => {
                if self.local_offer_declared.load(Ordering::SeqCst) {
                    self.establish(parsed);
                }
            }
            SdpType::Rollback => *self.pending.lock().unwrap() = None,
            SdpType::PrAnswer => {}
        }
    }

    fn establish(&self, params: Option<Params>) {
        let mut slot = self.remote.lock().unwrap();
        if slot.is_none() {
            *slot = params;
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
}
