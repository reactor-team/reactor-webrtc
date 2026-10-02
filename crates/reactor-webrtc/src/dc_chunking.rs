//! [`DcChunking`] — the factory-wide settings for large data-channel messages.

use std::sync::{Arc, Mutex};

use reactor_webrtc_dc_chunking::sdp::{self, Params};

use crate::{Error, Result, SessionDescription};

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
    remote: Mutex<Option<Params>>,
}

impl DcNegotiation {
    pub(crate) fn new(settings: Option<DcChunking>) -> Arc<Self> {
        Arc::new(Self {
            settings,
            remote: Mutex::new(None),
        })
    }

    pub(crate) fn settings(&self) -> Option<&DcChunking> {
        self.settings.as_ref()
    }

    /// The peer's parameters, once negotiated. `None` while this connection
    /// does not take part or the peer has not declared chunking.
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

    /// An answer declares it only when the offer did: offer/answer cannot
    /// introduce a capability the offerer never asked for.
    pub(crate) fn answer(&self, answer: SessionDescription) -> SessionDescription {
        match (&self.settings, self.remote()) {
            (Some(s), Some(_)) => answer.with_dc_chunking(&Params::local(s.max_message_size)),
            _ => answer,
        }
    }

    /// Record the peer's parameters from an applied remote description.
    ///
    /// Sticky: the first description that declares chunking settles it for
    /// the life of the connection, and a later renegotiation that drops the
    /// attribute does not undo it. Channels already open keep their framing
    /// either way, and both ends must keep agreeing for new ones; the
    /// offerer applies the answer before any SCTP data can flow, so both
    /// sides have settled before the first channel opens.
    pub(crate) fn on_remote_description(&self, remote: &SessionDescription) {
        if self.settings.is_none() {
            return;
        }
        let mut slot = self.remote.lock().unwrap();
        if slot.is_none() {
            *slot = sdp::parse(&remote.sdp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
