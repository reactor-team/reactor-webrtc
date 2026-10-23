//! [`SctpTuning`] — dcsctp congestion control and timers for a factory's data
//! channels.

use crate::{Error, Result};

/// dcsctp settings for every data channel of a factory, set with
/// [`PeerConnectionFactoryBuilder::with_sctp_tuning`](crate::PeerConnectionFactoryBuilder::with_sctp_tuning)
/// (libwebrtc patch 0011). Each field is `None` by default, which keeps
/// upstream's value.
///
/// `#[non_exhaustive]` — start from [`SctpTuning::default`] or
/// [`SctpTuning::low_latency`] and assign the fields you want to change:
///
/// ```rust,ignore
/// let factory = PeerConnectionFactory::builder()
///     .with_sctp_tuning(SctpTuning::low_latency())
///     .build()?;
/// ```
///
/// Upstream tunes dcsctp for bulk transfers that share a path fairly. A
/// request/response channel on a long path pays for that in round trips:
///
/// - After a loss, the window halves (to no less than `min_cwnd_mtus`, 4
///   packets) and then grows by one packet per round trip. At 200 ms and 0.1%
///   loss it stays near 2 Mbit/s, so a 100 KB command takes two or three round
///   trips and 1 MB about five seconds.
/// - The receiver holds a SACK for up to `delayed_ack_max_ms` (200 ms). On a
///   200 ms path that reaches the 400 ms retransmission timeout, which fires
///   a spurious retransmission and drops the window to one packet: after an
///   idle pause the next large command starts over from one packet.
///
/// Both ends of a connection should use the same settings: the sender's
/// window and the receiver's acknowledgements decide each transfer together.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SctpTuning {
    /// Congestion window a new association starts with, in packets
    /// (upstream 10, about 12 KB).
    pub initial_cwnd_mtus: Option<u32>,
    /// Floor the window falls to on a loss detected by SACK, in packets
    /// (upstream 4). A retransmission timeout still drops it to one packet.
    pub min_cwnd_mtus: Option<u32>,
    /// Retransmission timeout before the first RTT sample, in ms (upstream 500).
    pub rto_initial_ms: Option<u32>,
    /// Lowest retransmission timeout, in ms (upstream 400). It must stay above
    /// the peer's delayed-ack time plus the round trip, or retransmissions
    /// fire spuriously.
    pub rto_min_ms: Option<u32>,
    /// Longest this side holds a SACK, in ms (upstream 200).
    pub delayed_ack_max_ms: Option<u32>,
    /// Below this window, in packets, DATA asks the peer for an immediate SACK
    /// (upstream 10, the initial window).
    pub immediate_sack_under_cwnd_mtus: Option<u32>,
}

/// The largest value each field accepts, so the glue's `int` never overflows
/// and a typo (milliseconds as microseconds) is refused rather than applied.
const MAX_MTUS: u32 = 100_000;
const MAX_MS: u32 = 60_000;

impl SctpTuning {
    /// Settings for a request/response data channel on a long or lossy path:
    /// a 200-packet initial window, a 100-packet floor after a loss, a SACK
    /// held for at most 20 ms and asked for at once below 1000 packets.
    ///
    /// On an emulated 200 ms path with both ends tuned, against upstream's
    /// values (median round trip of one session sending commands back to
    /// back): with 0.1% loss, 100 KB 605 → 203 ms, 200 KB 1,007 → 405 ms and
    /// 1 MB 5.5 → 1.8 s; with no loss and a 1 s pause before each command,
    /// 1 MB 1.4 s → 214 ms. Pings and back-to-back commands on a clean path
    /// stay at one round trip either way.
    ///
    /// The window floor is the aggressive part: after a loss the channel keeps
    /// sending about 120 KB per round trip instead of backing off as TCP
    /// would. That suits commands; a channel that moves tens of megabytes over
    /// congested links may lose more packets with it.
    pub fn low_latency() -> Self {
        Self {
            initial_cwnd_mtus: Some(200),
            min_cwnd_mtus: Some(100),
            rto_initial_ms: None,
            rto_min_ms: None,
            delayed_ack_max_ms: Some(20),
            immediate_sack_under_cwnd_mtus: Some(1000),
        }
    }

    /// Reject values the glue cannot carry or dcsctp cannot use.
    pub(crate) fn validate(&self) -> Result<()> {
        let check = |name: &str, value: Option<u32>, max: u32| match value {
            Some(v) if v == 0 || v > max => Err(Error::Webrtc(format!(
                "sctp tuning: {name} {v} must be between 1 and {max}"
            ))),
            _ => Ok(()),
        };
        check("initial_cwnd_mtus", self.initial_cwnd_mtus, MAX_MTUS)?;
        check("min_cwnd_mtus", self.min_cwnd_mtus, MAX_MTUS)?;
        check("rto_initial_ms", self.rto_initial_ms, MAX_MS)?;
        check("rto_min_ms", self.rto_min_ms, MAX_MS)?;
        check("delayed_ack_max_ms", self.delayed_ack_max_ms, MAX_MS)?;
        check(
            "immediate_sack_under_cwnd_mtus",
            self.immediate_sack_under_cwnd_mtus,
            MAX_MTUS,
        )?;
        Ok(())
    }

    /// Writes the set fields into the glue's options; validate() bounds them
    /// to i32, so the casts are lossless.
    pub(crate) fn apply(&self, opts: &mut reactor_webrtc_sys::ReactorFactoryOptions) {
        let v = |x: Option<u32>| x.map_or(0, |x| x as std::os::raw::c_int);
        opts.sctp_initial_cwnd_mtus = v(self.initial_cwnd_mtus);
        opts.sctp_min_cwnd_mtus = v(self.min_cwnd_mtus);
        opts.sctp_rto_initial_ms = v(self.rto_initial_ms);
        opts.sctp_rto_min_ms = v(self.rto_min_ms);
        opts.sctp_delayed_ack_max_ms = v(self.delayed_ack_max_ms);
        opts.sctp_immediate_sack_under_cwnd_mtus = v(self.immediate_sack_under_cwnd_mtus);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_keeps_every_upstream_value() {
        let mut opts = reactor_webrtc_sys::ReactorFactoryOptions::default();
        SctpTuning::default().apply(&mut opts);
        assert_eq!(
            [
                opts.sctp_initial_cwnd_mtus,
                opts.sctp_min_cwnd_mtus,
                opts.sctp_rto_initial_ms,
                opts.sctp_rto_min_ms,
                opts.sctp_delayed_ack_max_ms,
                opts.sctp_immediate_sack_under_cwnd_mtus,
            ],
            [0; 6]
        );
    }

    #[test]
    fn low_latency_sets_the_window_and_the_sacks() {
        let tuning = SctpTuning::low_latency();
        tuning.validate().unwrap();
        let mut opts = reactor_webrtc_sys::ReactorFactoryOptions::default();
        tuning.apply(&mut opts);
        assert_eq!(opts.sctp_initial_cwnd_mtus, 200);
        assert_eq!(opts.sctp_min_cwnd_mtus, 100);
        assert_eq!(opts.sctp_delayed_ack_max_ms, 20);
        assert_eq!(opts.sctp_immediate_sack_under_cwnd_mtus, 1000);
        assert_eq!(opts.sctp_rto_min_ms, 0, "keeps upstream's RTO floor");
    }

    #[test]
    fn zero_and_out_of_range_values_are_refused() {
        let with = |t: SctpTuning| t.validate();
        assert!(with(SctpTuning {
            min_cwnd_mtus: Some(0),
            ..Default::default()
        })
        .is_err());
        assert!(with(SctpTuning {
            delayed_ack_max_ms: Some(MAX_MS + 1),
            ..Default::default()
        })
        .is_err());
        assert!(with(SctpTuning {
            rto_min_ms: Some(MAX_MS),
            ..Default::default()
        })
        .is_ok());
    }
}
