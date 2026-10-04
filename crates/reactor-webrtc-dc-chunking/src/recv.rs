//! The per-channel reassembler.
//!
//! Frames of one message arrive contiguously on an ordered channel, so the
//! reassembler only ever holds one partial message. A message larger than the
//! local limit is not buffered: its frames are skipped up to the last one, the
//! drop is reported, and the channel keeps working for the next message.

use crate::frame::{FrameError, Header};

/// What one frame completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The frame belongs to a message that is not complete yet.
    Pending,
    /// A whole message, with the type it was sent as.
    Message { data: Vec<u8>, binary: bool },
    /// A message larger than the local limit ended; none of it was kept.
    Dropped { size: u64 },
}

/// Turns one channel's frames back into messages.
pub struct Reassembler {
    max_message_size: u64,
    buf: Vec<u8>,
    /// Type of the message in progress; `None` between messages.
    text: Option<bool>,
    /// Bytes skipped so far when the message in progress passed the limit.
    skipping: Option<u64>,
}

impl Reassembler {
    /// A reassembler that keeps messages up to `max_message_size` bytes.
    pub fn new(max_message_size: u64) -> Self {
        Self {
            max_message_size,
            buf: Vec::new(),
            text: None,
            skipping: None,
        }
    }

    /// Feed one frame as received from the channel.
    ///
    /// An error means the peer broke the format; the platform should close
    /// the channel, and [`reset`](Self::reset) before any reuse.
    pub fn push(&mut self, frame: &[u8]) -> Result<Delivery, FrameError> {
        let (&first, payload) = frame.split_first().ok_or(FrameError::Empty)?;
        let header = Header::decode(first)?;
        match self.text {
            Some(text) if text != header.text => return Err(FrameError::TypeChanged),
            _ => self.text = Some(header.text),
        }

        if let Some(skipped) = self.skipping.as_mut() {
            *skipped += payload.len() as u64;
        } else if self.buf.len() as u64 + payload.len() as u64 > self.max_message_size {
            self.skipping = Some(self.buf.len() as u64 + payload.len() as u64);
            self.buf = Vec::new();
        } else {
            self.buf.extend_from_slice(payload);
        }

        if header.more {
            return Ok(Delivery::Pending);
        }
        self.text = None;
        if let Some(size) = self.skipping.take() {
            return Ok(Delivery::Dropped { size });
        }
        Ok(Delivery::Message {
            data: std::mem::take(&mut self.buf),
            binary: !header.text,
        })
    }

    /// Whether a message has started arriving but is not complete.
    pub fn mid_message(&self) -> bool {
        self.text.is_some()
    }

    /// Discard any partial message (the channel closed).
    pub fn reset(&mut self) {
        self.buf = Vec::new();
        self.text = None;
        self.skipping = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_frame_message() {
        let mut r = Reassembler::new(100);
        assert_eq!(
            r.push(b"\x00abc"),
            Ok(Delivery::Message {
                data: b"abc".to_vec(),
                binary: true
            })
        );
    }

    #[test]
    fn multi_frame_text_message() {
        let mut r = Reassembler::new(100);
        assert_eq!(r.push(b"\x03hel"), Ok(Delivery::Pending));
        assert!(r.mid_message());
        assert_eq!(
            r.push(b"\x02lo"),
            Ok(Delivery::Message {
                data: b"hello".to_vec(),
                binary: false
            })
        );
        assert!(!r.mid_message());
    }

    #[test]
    fn header_only_frame_is_an_empty_message() {
        let mut r = Reassembler::new(100);
        assert_eq!(
            r.push(&[0x00]),
            Ok(Delivery::Message {
                data: vec![],
                binary: true
            })
        );
    }

    #[test]
    fn drops_an_oversized_message_and_keeps_going() {
        let mut r = Reassembler::new(5);
        assert_eq!(r.push(b"\x01abcd"), Ok(Delivery::Pending));
        assert_eq!(r.push(b"\x01efgh"), Ok(Delivery::Pending));
        assert_eq!(r.push(b"\x00ij"), Ok(Delivery::Dropped { size: 10 }));
        assert_eq!(
            r.push(b"\x00ok"),
            Ok(Delivery::Message {
                data: b"ok".to_vec(),
                binary: true
            })
        );
    }

    #[test]
    fn a_message_exactly_at_the_limit_is_kept() {
        let mut r = Reassembler::new(4);
        r.push(b"\x01ab").unwrap();
        assert_eq!(
            r.push(b"\x00cd"),
            Ok(Delivery::Message {
                data: b"abcd".to_vec(),
                binary: true
            })
        );
    }

    #[test]
    fn rejects_broken_frames() {
        let mut r = Reassembler::new(100);
        assert_eq!(r.push(b""), Err(FrameError::Empty));
        assert_eq!(r.push(b"\x80x"), Err(FrameError::UnknownVersion(0x80)));
        r.push(b"\x01ab").unwrap();
        assert_eq!(r.push(b"\x02cd"), Err(FrameError::TypeChanged));
    }

    #[test]
    fn reset_discards_a_partial_message() {
        let mut r = Reassembler::new(100);
        r.push(b"\x01ab").unwrap();
        r.reset();
        assert!(!r.mid_message());
        assert_eq!(
            r.push(b"\x02cd"),
            Ok(Delivery::Message {
                data: b"cd".to_vec(),
                binary: false
            })
        );
    }
}
