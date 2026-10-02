//! The per-channel send queue.
//!
//! `push` accepts a whole message, however large, up to the configured
//! limits; `next_frame` hands out its frames one at a time, but only while the
//! native channel's buffer is below the high-water mark. The platform calls
//! `next_frame` after every `push` and from the native low-threshold callback,
//! so the native buffer never approaches libwebrtc's 16 MiB limit and the
//! SCTP queue is refilled before it drains (an empty queue would collapse the
//! flight and restart the ramp in the middle of a message).

use std::collections::VecDeque;
use std::fmt;

use crate::frame::Header;

/// How a queue splits and paces messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendConfig {
    /// Frame size on the wire, header included.
    pub chunk_size: usize,
    /// Hand out no frame while the native buffer holds this many bytes or more.
    pub high_water: u64,
    /// The native low-threshold the platform registers; the pump resumes there.
    pub low_water: u64,
    /// Bytes the queue may hold that the native channel has not taken yet.
    pub queue_limit: u64,
    /// Largest message `push` accepts: the smaller of the local limit and the
    /// peer's advertised one (see [`crate::sdp::effective_max_message_size`]).
    pub max_message_size: u64,
}

impl Default for SendConfig {
    fn default() -> Self {
        Self {
            chunk_size: crate::DEFAULT_CHUNK_SIZE,
            high_water: crate::DEFAULT_HIGH_WATER,
            low_water: crate::DEFAULT_LOW_WATER,
            queue_limit: crate::DEFAULT_SEND_BUFFER_LIMIT,
            max_message_size: crate::DEFAULT_MAX_MESSAGE_SIZE,
        }
    }
}

impl SendConfig {
    /// Check the values against each other and against libwebrtc's limits.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // A frame must carry at least one payload byte and fit the 256 KiB a
        // data-channel message may be on any peer.
        if self.chunk_size < 2 || self.chunk_size as u64 > crate::LEGACY_MAX_MESSAGE_SIZE {
            return Err(ConfigError::ChunkSize(self.chunk_size));
        }
        if self.low_water >= self.high_water {
            return Err(ConfigError::Watermarks {
                low: self.low_water,
                high: self.high_water,
            });
        }
        // high_water plus one frame must stay under the native limit. Written
        // as a subtraction (chunk_size is at most 256 KiB here) so an extreme
        // high_water cannot overflow.
        if self.high_water > crate::NATIVE_SEND_BUFFER_LIMIT - self.chunk_size as u64 {
            return Err(ConfigError::HighWater(self.high_water));
        }
        if self.queue_limit < self.max_message_size {
            return Err(ConfigError::QueueBelowMessage {
                queue: self.queue_limit,
                message: self.max_message_size,
            });
        }
        Ok(())
    }
}

/// A [`SendConfig`] whose values cannot work together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    ChunkSize(usize),
    Watermarks { low: u64, high: u64 },
    HighWater(u64),
    QueueBelowMessage { queue: u64, message: u64 },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChunkSize(n) => write!(f, "chunk_size {n} must be between 2 bytes and 256 KiB"),
            Self::Watermarks { low, high } => {
                write!(f, "low_water {low} must be below high_water {high}")
            }
            Self::HighWater(n) => write!(
                f,
                "high_water {n} plus one frame must stay under libwebrtc's 16 MiB send buffer"
            ),
            Self::QueueBelowMessage { queue, message } => {
                write!(
                    f,
                    "queue_limit {queue} must be at least max_message_size {message}"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Why `push` refused a message. Nothing of it was queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The message is larger than the effective max message size.
    TooLarge { size: u64, max: u64 },
    /// Queueing it would pass the queue limit.
    QueueFull { queued: u64, size: u64, limit: u64 },
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { size, max } => write!(
                f,
                "message of {size} bytes is larger than the {max}-byte limit"
            ),
            Self::QueueFull {
                queued,
                size,
                limit,
            } => {
                write!(
                    f,
                    "queue holds {queued} bytes; adding {size} would pass its {limit}-byte limit"
                )
            }
        }
    }
}

impl std::error::Error for SendError {}

struct Pending {
    data: Vec<u8>,
    text: bool,
    /// Payload bytes already handed out as frames.
    offset: usize,
}

/// Messages waiting to leave one channel, in send order.
pub struct SendQueue {
    config: SendConfig,
    pending: VecDeque<Pending>,
    queued: u64,
}

impl SendQueue {
    /// A queue for one channel. `config` should have passed [`SendConfig::validate`].
    pub fn new(config: SendConfig) -> Self {
        Self {
            config,
            pending: VecDeque::new(),
            queued: 0,
        }
    }

    pub fn config(&self) -> &SendConfig {
        &self.config
    }

    /// Queue a whole message. `binary` is the type the receiver should see.
    pub fn push(&mut self, data: Vec<u8>, binary: bool) -> Result<(), SendError> {
        let size = data.len() as u64;
        if size > self.config.max_message_size {
            return Err(SendError::TooLarge {
                size,
                max: self.config.max_message_size,
            });
        }
        if self.queued + size > self.config.queue_limit {
            return Err(SendError::QueueFull {
                queued: self.queued,
                size,
                limit: self.config.queue_limit,
            });
        }
        self.queued += size;
        self.pending.push_back(Pending {
            data,
            text: !binary,
            offset: 0,
        });
        Ok(())
    }

    /// The next frame to hand to the native channel, or `None` when the queue
    /// is empty or `native_buffered` is at the high-water mark. Frames always
    /// go out as binary messages.
    pub fn next_frame(&mut self, native_buffered: u64) -> Option<Vec<u8>> {
        if native_buffered >= self.config.high_water {
            return None;
        }
        let head = self.pending.front_mut()?;
        let room = self.config.chunk_size - 1;
        let take = room.min(head.data.len() - head.offset);
        let more = head.offset + take < head.data.len();
        let mut frame = Vec::with_capacity(take + 1);
        frame.push(
            Header {
                more,
                text: head.text,
            }
            .encode(),
        );
        frame.extend_from_slice(&head.data[head.offset..head.offset + take]);
        head.offset += take;
        self.queued -= take as u64;
        if !more {
            self.pending.pop_front();
        }
        Some(frame)
    }

    /// Payload bytes not yet handed to the native channel.
    pub fn queued(&self) -> u64 {
        self.queued
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Whether a message has started leaving but its last frame has not, so a
    /// failure now would leave the peer with a partial message.
    pub fn mid_message(&self) -> bool {
        self.pending.front().is_some_and(|p| p.offset > 0)
    }

    /// Drop everything queued (the channel closed).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.queued = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> SendConfig {
        SendConfig {
            chunk_size: 5,
            high_water: 100,
            low_water: 50,
            queue_limit: 64,
            max_message_size: 32,
        }
    }

    fn drain(q: &mut SendQueue) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| q.next_frame(0)).collect()
    }

    #[test]
    fn default_config_is_valid() {
        assert_eq!(SendConfig::default().validate(), Ok(()));
    }

    #[test]
    fn an_extreme_high_water_is_rejected_without_overflow() {
        let c = SendConfig {
            high_water: u64::MAX,
            ..SendConfig::default()
        };
        assert_eq!(c.validate(), Err(ConfigError::HighWater(u64::MAX)));
    }

    #[test]
    fn rejects_inconsistent_configs() {
        let ok = SendConfig::default();
        assert!(SendConfig {
            chunk_size: 1,
            ..ok
        }
        .validate()
        .is_err());
        assert!(SendConfig {
            chunk_size: 300 * 1024,
            ..ok
        }
        .validate()
        .is_err());
        assert!(SendConfig {
            low_water: ok.high_water,
            ..ok
        }
        .validate()
        .is_err());
        assert!(SendConfig {
            high_water: crate::NATIVE_SEND_BUFFER_LIMIT,
            ..ok
        }
        .validate()
        .is_err());
        assert!(SendConfig {
            queue_limit: ok.max_message_size - 1,
            ..ok
        }
        .validate()
        .is_err());
    }

    #[test]
    fn splits_into_frames_of_chunk_size_with_header() {
        let mut q = SendQueue::new(small());
        q.push(b"abcdefghij".to_vec(), true).unwrap();
        // chunk_size 5 = 1 header byte + 4 payload bytes.
        assert_eq!(
            drain(&mut q),
            vec![
                b"\x01abcd".to_vec(),
                b"\x01efgh".to_vec(),
                b"\x00ij".to_vec()
            ]
        );
        assert!(q.is_empty());
        assert_eq!(q.queued(), 0);
    }

    #[test]
    fn exact_multiple_ends_with_a_full_last_frame() {
        let mut q = SendQueue::new(small());
        q.push(b"abcdefgh".to_vec(), true).unwrap();
        assert_eq!(
            drain(&mut q),
            vec![b"\x01abcd".to_vec(), b"\x00efgh".to_vec()]
        );
    }

    #[test]
    fn empty_message_is_one_header_only_frame() {
        let mut q = SendQueue::new(small());
        q.push(Vec::new(), false).unwrap();
        assert_eq!(drain(&mut q), vec![vec![0x02]]);
    }

    #[test]
    fn text_flag_is_on_every_frame() {
        let mut q = SendQueue::new(small());
        q.push(b"hello".to_vec(), false).unwrap();
        assert_eq!(drain(&mut q), vec![b"\x03hell".to_vec(), b"\x02o".to_vec()]);
    }

    #[test]
    fn messages_leave_in_order_and_never_interleave() {
        let mut q = SendQueue::new(small());
        q.push(b"aaaaaa".to_vec(), true).unwrap();
        q.push(b"bb".to_vec(), true).unwrap();
        assert_eq!(
            drain(&mut q),
            vec![b"\x01aaaa".to_vec(), b"\x00aa".to_vec(), b"\x00bb".to_vec()]
        );
    }

    #[test]
    fn holds_frames_at_the_high_water_mark() {
        let mut q = SendQueue::new(small());
        q.push(b"abcdefgh".to_vec(), true).unwrap();
        assert_eq!(q.next_frame(100), None);
        assert_eq!(q.next_frame(99), Some(b"\x01abcd".to_vec()));
        assert!(q.mid_message());
        assert_eq!(q.queued(), 4);
    }

    #[test]
    fn rejects_oversized_messages_and_a_full_queue() {
        let mut q = SendQueue::new(small());
        assert_eq!(
            q.push(vec![0; 33], true),
            Err(SendError::TooLarge { size: 33, max: 32 })
        );
        q.push(vec![0; 32], true).unwrap();
        q.push(vec![0; 32], true).unwrap();
        assert_eq!(
            q.push(vec![0; 1], true),
            Err(SendError::QueueFull {
                queued: 64,
                size: 1,
                limit: 64
            })
        );
        assert_eq!(q.queued(), 64);
    }

    #[test]
    fn clear_drops_everything() {
        let mut q = SendQueue::new(small());
        q.push(vec![1; 10], true).unwrap();
        q.next_frame(0);
        q.clear();
        assert!(q.is_empty());
        assert!(!q.mid_message());
        assert_eq!(q.queued(), 0);
    }
}
