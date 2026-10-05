//! [`PlayoutDelay`] — the limits a video receiver's playout delay is held to.

use std::time::Duration;

use crate::{Error, Result};

/// Bounds on how long a video receiver holds a frame before rendering it — the
/// playout-delay RTP header extension
/// (`http://www.webrtc.org/experiments/rtp-hdrext/playout-delay`).
///
/// The receiver keeps its delay between `min` and `max`, so `max` caps the
/// smoothing its jitter buffer would otherwise add, and `min` holds frames back
/// at least that long. [`PlayoutDelay::IMMEDIATE`] (both zero) is the
/// low-latency case: the receiver skips render-time smoothing altogether and
/// hands each frame to the decoder as soon as it is complete.
///
/// Set it on the sending side with
/// [`with_send_playout_delay`](crate::PeerConnectionFactoryBuilder::with_send_playout_delay),
/// so every receiver that understands the extension honours it, or on the
/// receiving side with
/// [`with_receive_playout_delay`](crate::PeerConnectionFactoryBuilder::with_receive_playout_delay),
/// which needs nothing from the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutDelay {
    pub min: Duration,
    pub max: Duration,
}

impl PlayoutDelay {
    /// Render every frame as soon as it can be decoded.
    pub const IMMEDIATE: PlayoutDelay = PlayoutDelay {
        min: Duration::ZERO,
        max: Duration::ZERO,
    };

    /// The largest limit the extension can carry: 12 bits of 10 ms units.
    pub const MAX: Duration = Duration::from_millis(40_950);

    pub fn new(min: Duration, max: Duration) -> Self {
        Self { min, max }
    }

    /// `(min_ms, max_ms)`, or why the limits cannot be sent. The wire format
    /// counts in 10 ms units, so values are rounded down to a multiple of 10 ms
    /// on the way out.
    pub(crate) fn validate(&self) -> Result<(i32, i32)> {
        if self.min > self.max {
            return Err(Error::Webrtc(format!(
                "playout delay: min {:?} is larger than max {:?}",
                self.min, self.max
            )));
        }
        if self.max > Self::MAX {
            return Err(Error::Webrtc(format!(
                "playout delay: max {:?} is larger than the extension's {:?}",
                self.max,
                Self::MAX
            )));
        }
        // Bounded by MAX above, so both fit in an i32.
        Ok((self.min.as_millis() as i32, self.max.as_millis() as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_is_zero_zero() {
        assert_eq!(PlayoutDelay::IMMEDIATE.validate().unwrap(), (0, 0));
    }

    #[test]
    fn min_above_max_is_refused() {
        let d = PlayoutDelay::new(Duration::from_millis(200), Duration::from_millis(100));
        assert!(d.validate().is_err());
    }

    #[test]
    fn max_beyond_the_extension_is_refused() {
        let d = PlayoutDelay::new(
            Duration::ZERO,
            PlayoutDelay::MAX + Duration::from_millis(10),
        );
        assert!(d.validate().is_err());
        let d = PlayoutDelay::new(Duration::ZERO, PlayoutDelay::MAX);
        assert_eq!(d.validate().unwrap(), (0, 40_950));
    }
}
