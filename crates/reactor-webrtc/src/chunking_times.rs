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
    /// From the message being in the queue (when `enqueue_s` ends) to its
    /// first frame leaving it, summed over `messages_sent`: the wait behind
    /// earlier messages, and for the native buffer to drop below the
    /// high-water mark.
    pub queue_wait_s: f64,
    /// From `send` until the message is in the queue, summed over
    /// `messages_sent`: deciding the chunking on the channel's first send and
    /// the channel state check (both answered on libwebrtc's network thread),
    /// the wait for the queue's lock and the copy of the
    /// message into it. `queue_wait_s` starts when it ends, so the two do not
    /// overlap.
    pub enqueue_s: f64,
    /// Within `queue_wait_s`, for each message's first frame, summed over
    /// `messages_sent`: reading libwebrtc's buffered amount before taking it.
    pub first_check_s: f64,
    /// Within `queue_wait_s`: cutting the message's first frame off the queue.
    pub first_build_s: f64,
    /// Within `queue_wait_s`: libwebrtc's `Send` of the message's first frame.
    pub first_native_s: f64,
    /// libwebrtc's `Send` of every frame, summed over `frames_sent`.
    pub native_send_s: f64,
    /// From a message's first frame to its last being handed to the native
    /// channel, summed over `messages_sent`. A one-frame message adds zero.
    pub send_s: f64,
    /// Times the pump stopped with frames still queued because the native
    /// buffer was at the high-water mark, counted when it resumed. A stall
    /// still going, or ended by the channel closing, is not counted.
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

/// When the steps of handing one frame to libwebrtc started. The last ends
/// when the frame was handed over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameSteps {
    /// Before reading libwebrtc's buffered amount.
    pub(crate) check_start: Instant,
    /// Before cutting the frame off the queue.
    pub(crate) build_start: Instant,
    /// Before libwebrtc's `Send` of the frame.
    pub(crate) native_start: Instant,
}

/// How long the steps of handing one frame to libwebrtc took, in seconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct FrameTimes {
    /// Reading libwebrtc's buffered amount.
    check_s: f64,
    /// Cutting the frame off the queue.
    build_s: f64,
    /// libwebrtc's `Send` of the frame.
    native_s: f64,
}

/// The message leaving now: what its first frame recorded, kept until its
/// last frame leaves.
#[derive(Clone, Copy)]
struct Leaving {
    /// When the message handed over its first frame.
    first_at: Instant,
    /// How long it waited in the queue before that.
    waited_s: f64,
    /// The steps of handing over its first frame.
    first_steps: FrameTimes,
    /// How long its copy into the queue took.
    enqueue_s: f64,
}

#[derive(Default)]
pub(crate) struct ChunkingTimes {
    stats: ChunkingStats,
    /// When each queued message was sent, oldest first, in the queue's order,
    /// and how long its copy into the queue took.
    sent_at: VecDeque<(Instant, f64)>,
    /// The message leaving now. Its times join the totals only with the
    /// message's last frame, so a message that never finishes leaving adds
    /// nothing.
    leaving: Option<Leaving>,
    /// When the pump stopped with frames still queued.
    stalled_since: Option<Instant>,
    /// When the message arriving now delivered its first frame.
    arriving_since: Option<Instant>,
}

impl ChunkingTimes {
    pub(crate) fn stats(&self) -> ChunkingStats {
        self.stats
    }

    /// A message was queued, its copy into the queue having taken
    /// *enqueue_s*.
    pub(crate) fn queued(&mut self, now: Instant, enqueue_s: f64) {
        self.sent_at.push_back((now, enqueue_s));
    }

    /// The queue handed out `frame`, its steps having started at *steps*
    /// and the last ending at *now*.
    pub(crate) fn frame_sent(&mut self, frame: &[u8], now: Instant, steps: FrameSteps) {
        self.stats.native_send_s += (now - steps.native_start).as_secs_f64();
        // A stall is counted with its duration, when it ends, so both land in
        // the same reading.
        if let Some(since) = self.stalled_since.take() {
            self.stats.stalls += 1;
            self.stats.stall_s += (now - since).as_secs_f64();
        }
        let leaving = *self.leaving.get_or_insert_with(|| {
            let (sent, enqueue_s) = self.sent_at.front().copied().unwrap_or((now, 0.0));
            // A pump waiting on the queue's lock while a sender copied this
            // message in started its steps before the message was queued.
            // Count them from then, so they stay within its queue wait.
            let from = |t: Instant| t.max(sent);
            Leaving {
                first_at: now,
                waited_s: (now - sent).as_secs_f64(),
                first_steps: FrameTimes {
                    check_s: (from(steps.build_start) - from(steps.check_start)).as_secs_f64(),
                    build_s: (from(steps.native_start) - from(steps.build_start)).as_secs_f64(),
                    native_s: (now - from(steps.native_start)).as_secs_f64(),
                },
                enqueue_s,
            }
        });
        self.stats.frames_sent += 1;
        if !more(frame) {
            self.stats.queue_wait_s += leaving.waited_s;
            self.stats.enqueue_s += leaving.enqueue_s;
            self.stats.first_check_s += leaving.first_steps.check_s;
            self.stats.first_build_s += leaving.first_steps.build_s;
            self.stats.first_native_s += leaving.first_steps.native_s;
            self.stats.send_s += (now - leaving.first_at).as_secs_f64();
            self.stats.messages_sent += 1;
            self.sent_at.pop_front();
            self.leaving = None;
        }
    }

    /// The pump stopped with frames still queued: the native buffer is full.
    pub(crate) fn stalled(&mut self, now: Instant) {
        self.stalled_since.get_or_insert(now);
    }

    /// The queue was cleared: what it held never leaves.
    pub(crate) fn cleared(&mut self) {
        self.sent_at.clear();
        self.leaving = None;
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

    /// Steps that all started at `t`: a frame handed over at `t` took no time.
    fn at(t: Instant) -> FrameSteps {
        FrameSteps {
            check_start: t,
            build_start: t,
            native_start: t,
        }
    }

    #[test]
    fn a_message_waits_in_the_queue_then_leaves_frame_by_frame() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut times = ChunkingTimes::default();

        times.queued(t0, 0.0);
        times.frame_sent(&frame(true), ms(5), at(ms(5)));
        times.frame_sent(&frame(true), ms(7), at(ms(7)));
        times.frame_sent(&frame(false), ms(12), at(ms(12)));

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

        times.queued(t0, 0.0);
        times.queued(ms(1), 0.0);
        times.frame_sent(&frame(false), ms(2), at(ms(2)));
        times.frame_sent(&frame(false), ms(10), at(ms(10)));

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

        times.queued(t0, 0.0);
        times.frame_sent(&frame(true), t0, at(t0));
        times.stalled(ms(1));
        times.stalled(ms(3));
        assert_eq!(
            times.stats().stalls,
            0,
            "a stall still going is not counted yet"
        );
        times.frame_sent(&frame(false), ms(41), at(ms(41)));

        let s = times.stats();
        assert_eq!(
            s.stalls, 1,
            "a stall is counted once however often the pump stops"
        );
        assert!((s.stall_s - 0.040).abs() < 1e-9);
    }

    #[test]
    fn a_stall_ended_by_a_cleared_queue_is_not_counted() {
        let t0 = Instant::now();
        let mut times = ChunkingTimes::default();

        times.queued(t0, 0.0);
        times.frame_sent(&frame(true), t0, at(t0));
        times.stalled(t0 + Duration::from_millis(1));
        times.cleared();

        let s = times.stats();
        assert_eq!((s.stalls, s.stall_s), (0, 0.0));
    }

    #[test]
    fn a_cleared_queue_leaves_no_message_half_timed() {
        let t0 = Instant::now();
        let mut times = ChunkingTimes::default();

        times.queued(t0, 0.0);
        times.frame_sent(&frame(true), t0, at(t0));
        times.cleared();
        times.queued(t0 + Duration::from_millis(100), 0.0);
        times.frame_sent(
            &frame(false),
            t0 + Duration::from_millis(101),
            at(t0 + Duration::from_millis(101)),
        );

        let s = times.stats();
        assert_eq!(s.messages_sent, 1);
        assert!((s.queue_wait_s - 0.001).abs() < 1e-9);
    }

    #[test]
    fn a_message_that_never_finishes_leaving_adds_no_wait() {
        let t0 = Instant::now();
        let mut times = ChunkingTimes::default();

        times.queued(t0, 0.0);
        times.frame_sent(
            &frame(true),
            t0 + Duration::from_millis(50),
            at(t0 + Duration::from_millis(50)),
        );
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

    #[test]
    fn the_steps_of_the_first_frame_and_every_send_are_summed() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut times = ChunkingTimes::default();
        let steps = |start: Instant| FrameSteps {
            check_start: start,
            build_start: start + Duration::from_millis(1),
            native_start: start + Duration::from_millis(3),
        };

        times.queued(t0, 0.004);
        times.frame_sent(&frame(true), ms(13), steps(ms(0)));
        times.frame_sent(&frame(false), ms(33), steps(ms(10)));

        let s = times.stats();
        assert_eq!(s.messages_sent, 1);
        assert!((s.enqueue_s - 0.004).abs() < 1e-9);
        assert!((s.first_check_s - 0.001).abs() < 1e-9);
        assert!((s.first_build_s - 0.002).abs() < 1e-9);
        assert!(
            (s.first_native_s - 0.010).abs() < 1e-9,
            "only the first frame's Send"
        );
        assert!((s.native_send_s - 0.030).abs() < 1e-9, "every frame's Send");
    }

    #[test]
    fn first_frame_steps_begun_before_the_message_was_queued_count_from_then() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut times = ChunkingTimes::default();

        // The pump read the buffered amount and waited on the queue's lock
        // while the sender copied the message in, until 2 ms.
        times.queued(ms(2), 0.0);
        let steps = FrameSteps {
            check_start: ms(0),
            build_start: ms(1),
            native_start: ms(3),
        };
        times.frame_sent(&frame(false), ms(4), steps);

        let s = times.stats();
        assert!((s.queue_wait_s - 0.002).abs() < 1e-9);
        assert_eq!(s.first_check_s, 0.0, "the check was before the queue");
        assert!((s.first_build_s - 0.001).abs() < 1e-9);
        assert!((s.first_native_s - 0.001).abs() < 1e-9);
        let steps_s = s.first_check_s + s.first_build_s + s.first_native_s;
        assert!(steps_s <= s.queue_wait_s + 1e-9, "within queue_wait_s");
        assert!(
            (s.native_send_s - 0.001).abs() < 1e-9,
            "Send itself is not clamped"
        );
    }
}
