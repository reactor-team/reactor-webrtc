//! The one-byte frame header.
//!
//! Every message on a chunked channel starts with it. A small message is one
//! frame; a large one is a run of frames that the sender writes back to back,
//! so on an ordered channel the frames of one message are always contiguous
//! and need no message id.
//!
//! ```text
//! bit 0   MORE   1 = more frames of this message follow, 0 = last or only frame
//! bit 1   TEXT   the original message was text (every frame is sent as binary)
//! bits 2-7       version, 0 in v1; anything else is a protocol error
//! ```
//!
//! Frames always go out as binary. A text message split at an arbitrary byte
//! could cut a UTF-8 character in two, and a browser decoding each frame as a
//! string would corrupt it, so the original type travels in `TEXT` instead.

use std::fmt;

const MORE: u8 = 0b01;
const TEXT: u8 = 0b10;
const VERSION_MASK: u8 = !(MORE | TEXT);

/// A decoded frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// More frames of this message follow.
    pub more: bool,
    /// The original message was text rather than binary.
    pub text: bool,
}

impl Header {
    /// The header byte for this frame.
    pub fn encode(self) -> u8 {
        (if self.more { MORE } else { 0 }) | (if self.text { TEXT } else { 0 })
    }

    /// Decode a header byte; any version bit set is a protocol error.
    pub fn decode(byte: u8) -> Result<Self, FrameError> {
        if byte & VERSION_MASK != 0 {
            return Err(FrameError::UnknownVersion(byte));
        }
        Ok(Self {
            more: byte & MORE != 0,
            text: byte & TEXT != 0,
        })
    }
}

/// A frame the receiver cannot accept. Each one means the peer is broken or
/// speaks another version, so the platform closes the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// A zero-length message: every frame carries at least its header.
    Empty,
    /// The header sets bits this version does not define.
    UnknownVersion(u8),
    /// A frame's `TEXT` flag differs from the first frame of its message.
    TypeChanged,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty frame: every frame carries a header byte"),
            Self::UnknownVersion(b) => write!(f, "frame header {b:#04x} sets unknown version bits"),
            Self::TypeChanged => {
                f.write_str("frame changes text/binary type in the middle of a message")
            }
        }
    }
}

impl std::error::Error for FrameError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_defined_header() {
        for more in [false, true] {
            for text in [false, true] {
                let h = Header { more, text };
                assert_eq!(Header::decode(h.encode()), Ok(h));
            }
        }
    }

    #[test]
    fn encodes_the_documented_bits() {
        let byte = |more, text| Header { more, text }.encode();
        assert_eq!(byte(false, false), 0x00);
        assert_eq!(byte(true, false), 0x01);
        assert_eq!(byte(false, true), 0x02);
        assert_eq!(byte(true, true), 0x03);
    }

    #[test]
    fn rejects_version_bits() {
        for byte in [0x04, 0x80, 0xff] {
            assert_eq!(Header::decode(byte), Err(FrameError::UnknownVersion(byte)));
        }
    }
}
