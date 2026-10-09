//! Where a chunked channel's messages spend their time on this side.
//!
//! A message sent on a chunked channel waits in the channel's queue, then
//! leaves as a run of frames that the pump hands to the native channel while
//! its buffer has room. A message received arrives as a run of frames that
//! the reassembler joins. [`ChunkingTimes`] times those steps message by
//! message and keeps running totals, which [`ChunkingStats`] reports.
//!
//! The steps are timed here and not in `reactor-webrtc-dc-chunking`, which
//! has no clock: it also runs in the browser, where a monotonic clock is the
//! platform's to provide.

use std::collections::VecDeque;
use std::time::Instant;

use reactor_webrtc_dc_chunking::Header;

/// Running totals of a chunked channel's messages, since it opened.
///
/// Every time is summed over the messages that finished the step, so the
/// average over an interval is the change in a time divided by the change in
/// its count. Times are in seconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ChunkingStats {
    /// Messages whose last frame was handed to the native channel.
    pub messages_sent: u64,
    /// Frames handed to the native channel.
    pub frames_sent: u64,
    /// From `send` to the message's first frame leaving the queue, summed over
    /// `messages_sent`: the wait behind earlier messages, and for the native
    /// buffer to drop below the high-water mark.
    pub queue_wait_s: f64,
    /// From a message's first frame to its last being handed to the native
    /// channel, summed over `messages_sent`. A one-frame message adds zero.
    pub send_s: f64,
    /// Times the pump stopped with frames still queued because the native
    /// buffer was at the high-water mark.
    pub stalls: u64,
    /// The time the pump spent stopped that way, summed over `stalls`.
    pub stall_s: f64,
    /// Messages the reassembler completed, including any it dropped for being
    /// larger than the local limit.
    pub messages_received: u64,
    /// Frames received.
    pub frames_received: u64,
    /// From a message's first frame arriving to its last, summed over
    /// `messages_received`: how long the message's body took to come in. A
    /// one-frame message adds zero.
    pub reassembly_s: f64,
}

#[derive(Default)]
pub(crate) struct ChunkingTimes {
    stats: ChunkingStats,
    /// When each queued message was sent, oldest first, in the queue's order.
    sent_at: VecDeque<Instant>,
    /// When the message leaving now handed over its first frame, and how long
    /// it had waited in the queue. The wait joins the totals only with the
    /// message's last frame, so a message that never finishes leaving adds
    /// nothing.
    leaving_since: Option<(Instant, f64)>,
    /// When the pump stopped with frames still queued.
    stalled_since: Option<Instant>,
    /// When the message arriving now delivered its first frame.
    arriving_since: Option<Instant>,
}

impl ChunkingTimes {
    pub(crate) fn stats(&self) -> ChunkingStats {
        self.stats
    }

    /// A message was queued.
    pub(crate) fn queued(&mut self, now: Instant) {
        self.sent_at.push_back(now);
    }

    /// The queue handed out `frame`.
    pub(crate) fn frame_sent(&mut self, frame: &[u8], now: Instant) {
        if let Some(since) = self.stalled_since.take() {
            self.stats.stall_s += (now - since).as_secs_f64();
        }
        let (first, waited) = *self.leaving_since.get_or_insert_with(|| {
            let waited = self
                .sent_at
                .front()
                .map_or(0.0, |sent| (now - *sent).as_secs_f64());
            (now, waited)
        });
        self.stats.frames_sent += 1;
        if !more(frame) {
            self.stats.queue_wait_s += waited;
            self.stats.send_s += (now - first).as_secs_f64();
            self.stats.messages_sent += 1;
            self.sent_at.pop_front();
            self.leaving_since = None;
        }
    }

    /// The pump stopped with frames still queued: the native buffer is full.
    pub(crate) fn stalled(&mut self, now: Instant) {
        if self.stalled_since.is_none() {
            self.stalled_since = Some(now);
            self.stats.stalls += 1;
        }
    }

    /// The queue was cleared: what it held never leaves.
    pub(crate) fn cleared(&mut self) {
        self.sent_at.clear();
        self.leaving_since = None;
        self.stalled_since = None;
    }

    /// A frame arrived. `completes` is whether it ended a message.
    pub(crate) fn frame_received(&mut self, completes: bool, now: Instant) {
        let first = *self.arriving_since.get_or_insert(now);
        self.stats.frames_received += 1;
        if completes {
            self.stats.reassembly_s += (now - first).as_secs_f64();
            self.stats.messages_received += 1;
            self.arriving_since = None;
        }
    }

    /// The reassembler was reset: the message in progress never completes.
    pub(crate) fn reset_receive(&mut self) {
        self.arriving_since = None;
    }
}

/// Whether more frames of the message follow `frame`. A frame the queue made
/// always has a valid header.
fn more(frame: &[u8]) -> bool {
    frame
        .first()
        .and_then(|&b| Header::decode(b).ok())
        .is_some_and(|h| h.more)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn frame(more: bool) -> Vec<u8> {
        vec![Header { more, text: false }.encode(), 0]
    }

    #[test]
    fn a_message_waits_in_the_queue_then_leaves_frame_by_frame() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut times = ChunkingTimes::default();

        times.queued(t0);
        times.frame_sent(&frame(true), ms(5));
        times.frame_sent(&frame(true), ms(7));
        times.frame_sent(&frame(false), ms(12));

        let s = times.stats();
        assert_eq!((s.messages_sent, s.frames_sent), (1, 3));
        assert!((s.queue_wait_s - 0.005).abs() < 1e-9);
        assert!((s.send_s - 0.007).abs() < 1e-9);
    }

    #[test]
    fn a_second_message_waits_behind_the_first() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut times = ChunkingTimes::default();

        times.queued(t0);
        times.queued(ms(1));
        times.frame_sent(&frame(false), ms(2));
        times.frame_sent(&frame(false), ms(10));

        let s = times.stats();
        assert_eq!(s.messages_sent, 2);
        // 2 ms for the first, 9 ms for the second.
        assert!((s.queue_wait_s - 0.011).abs() < 1e-9);
        assert_eq!(s.send_s, 0.0, "one-frame messages take no time to leave");
    }

    #[test]
    fn a_stall_lasts_until_the_next_frame_leaves() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut times = ChunkingTimes::default();

        times.queued(t0);
        times.frame_sent(&frame(true), t0);
        times.stalled(ms(1));
        times.stalled(ms(3));
        times.frame_sent(&frame(false), ms(41));

        let s = times.stats();
        assert_eq!(
            s.stalls, 1,
            "a stall is counted once however often the pump stops"
        );
        assert!((s.stall_s - 0.040).abs() < 1e-9);
    }

    #[test]
    fn a_cleared_queue_leaves_no_message_half_timed() {
        let t0 = Instant::now();
        let mut times = ChunkingTimes::default();

        times.queued(t0);
        times.frame_sent(&frame(true), t0);
        times.cleared();
        times.queued(t0 + Duration::from_millis(100));
        times.frame_sent(&frame(false), t0 + Duration::from_millis(101));

        let s = times.stats();
        assert_eq!(s.messages_sent, 1);
        assert!((s.queue_wait_s - 0.001).abs() < 1e-9);
    }

    #[test]
    fn a_message_that_never_finishes_leaving_adds_no_wait() {
        let t0 = Instant::now();
        let mut times = ChunkingTimes::default();

        times.queued(t0);
        times.frame_sent(&frame(true), t0 + Duration::from_millis(50));
        times.cleared();

        let s = times.stats();
        assert_eq!((s.messages_sent, s.frames_sent), (0, 1));
        assert_eq!(
            s.queue_wait_s, 0.0,
            "the wait goes with a message that left"
        );
    }

    #[test]
    fn reassembly_runs_from_the_first_frame_to_the_last() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut times = ChunkingTimes::default();

        times.frame_received(false, t0);
        times.frame_received(false, ms(4));
        times.frame_received(true, ms(30));
        times.frame_received(true, ms(50));

        let s = times.stats();
        assert_eq!((s.messages_received, s.frames_received), (2, 4));
        assert!((s.reassembly_s - 0.030).abs() < 1e-9);
    }
}
