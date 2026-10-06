//! Chunking, reassembly and send pacing for WebRTC data channels.
//!
//! A data-channel message larger than a few packets is slow over dcsctp: it
//! sends at most `max_burst` packets per `send()` and per SACK, so a large
//! message ramps 4 → 8 → 16 … packets per round trip. libwebrtc also closes a
//! channel when more than 16 MiB wait in its send buffer. This crate lets a
//! sender split a message into frames, feed them to the channel only as fast
//! as it drains, and lets the receiver put the message back together.
//!
//! It is sans-I/O: no threads, no sockets, no libwebrtc. The platform layer
//! owns the channel and drives these state machines — reactor-webrtc for
//! native peers, reactor-wasm for browsers — so both sides speak exactly the
//! same wire format.
//!
//! - [`frame`] — the one-byte header every frame carries.
//! - [`sdp`] — the session-level attribute both peers declare.
//! - [`send`] — the per-channel queue that turns messages into frames.
//! - [`recv`] — the per-channel reassembler that turns frames into messages.

pub mod frame;
pub mod recv;
pub mod sdp;
pub mod send;

pub use frame::{FrameError, Header};
pub use recv::{Delivery, Reassembler};
pub use sdp::Params;
pub use send::{ConfigError, SendConfig, SendError, SendQueue};

/// Largest message a chunking side accepts by default (64 MiB).
pub const DEFAULT_MAX_MESSAGE_SIZE: u64 = 64 * 1024 * 1024;
/// Bytes a channel may queue beyond the native send buffer by default (128 MiB).
pub const DEFAULT_SEND_BUFFER_LIMIT: u64 = 128 * 1024 * 1024;
/// Frame size on the wire, header included, for native senders (64 KiB).
pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
/// Frame size for browser senders (4 KiB): with upstream dcsctp's
/// `max_burst` of 4, a frame this size leaves in a single burst.
pub const BROWSER_CHUNK_SIZE: usize = 4 * 1024;
/// Stop feeding the native channel above this many buffered bytes (8 MiB).
pub const DEFAULT_HIGH_WATER: u64 = 8 * 1024 * 1024;
/// Resume feeding below this many buffered bytes (4 MiB); the platform sets
/// the native low-threshold callback to this value.
pub const DEFAULT_LOW_WATER: u64 = 4 * 1024 * 1024;
/// libwebrtc closes a data channel when its send buffer would pass this.
pub const NATIVE_SEND_BUFFER_LIMIT: u64 = 16 * 1024 * 1024;
/// Message size limit of a peer that does not advertise one: the 256 KiB a
/// plain data channel allows.
pub const LEGACY_MAX_MESSAGE_SIZE: u64 = 256 * 1024;
