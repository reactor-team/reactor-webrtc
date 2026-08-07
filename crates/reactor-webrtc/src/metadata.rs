//! Frame metadata — protobuf-encoded trailer appended to encoded video payloads.
//!
//! Wire layout (appended after the codec's encoded bytes):
//!
//! ```text
//! [ encoded payload ][ proto bytes ][ u32 LE: proto_len ][ b"RXMT" ]
//! ```
//!
//! The receiver detects the `"RXMT"` magic at the end of the buffer, reads
//! `proto_len`, decodes the protobuf slice, and calls `replace_data` with the
//! original payload (minus the trailer) before forwarding to the decoder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use prost::Message;

const MAGIC: &[u8; 4] = b"RXMT";
const FRAMING: usize = 4 /* proto_len: u32 */ + 4 /* magic */;

/// The SDP attribute name a peer declares to say it understands the trailer this
/// module defines. Emitted at **session level** as `a=x-reactor-frame-metadata:<version>`.
///
/// A session-level line rather than a per-m-section one because support is a
/// property of a peer's code, not of one of its tracks — no peer understands the
/// trailer on one video track and not another.
///
/// Unregistered, hence the `x-` prefix. RFC 8866 §6 requires a receiver to ignore
/// an attribute it does not recognise, which is what makes the declaration safe to
/// send unconditionally: a peer that has never heard of it is unaffected.
///
/// Note that libwebrtc drops unrecognised `a=` lines when it parses a description,
/// so this is readable from the SDP **string** and not from anything libwebrtc
/// hands back. Everything here reads
/// [`SessionDescription::sdp`](crate::SessionDescription::sdp) directly for that
/// reason.
pub const FRAME_METADATA_ATTRIBUTE: &str = "x-reactor-frame-metadata";

/// Wire version of the trailer format in this module.
///
/// A peer declaring a different version reads as *unsupported*: an incompatible
/// change to the trailer bumps this, and old and new then never agree.
pub const FRAME_METADATA_VERSION: u32 = 1;

/// What the remote peer declared about frame-metadata support, as negotiated.
///
/// A connection's gate is available from
/// [`PeerConnection::frame_metadata_gate`][gate]. It starts **closed** and is
/// armed by [`PeerConnection::set_remote_description`][srd] from whether that
/// description carries [`FRAME_METADATA_ATTRIBUTE`]. Every renegotiation re-arms it, so
/// a peer that drops support closes it again.
///
/// It drives two things, both inside the library:
///
/// * [`create_answer`][ca] mirrors the offer — it declares the capability only when
///   the offer declared it.
/// * The sender metadata transform consults it per frame and appends nothing
///   while it is closed, because handing a trailer to a peer that will not strip
///   it hands the extra bytes to that peer's decoder.
///
/// Callers do not have to consult it: pass `user_data` whenever it is meaningful
/// and let the negotiated state decide whether it reaches the wire. Reading it is
/// still useful for diagnostics — "did this peer agree?" is otherwise invisible.
///
/// [gate]: crate::PeerConnection::frame_metadata_gate
/// [srd]: crate::PeerConnection::set_remote_description
/// [ca]: crate::PeerConnection::create_answer
#[derive(Clone, Debug, Default)]
pub struct FrameMetadataGate(Arc<AtomicBool>);

impl FrameMetadataGate {
    /// A closed gate, not attached to any peer connection. Useful in tests.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether trailers may be appended.
    pub fn is_open(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Arm from whether a remote description declared support.
    pub(crate) fn set(&self, declared: bool) {
        self.0.store(declared, Ordering::Relaxed);
    }
}

/// Per-frame metadata carried alongside an encoded video frame.
///
/// All fields are optional at the wire level — omitted fields decode to their
/// zero value. Set only the fields that are meaningful for your use case.
#[derive(Clone, PartialEq, prost::Message)]
pub struct FrameMetadata {
    /// Application-level frame counter (0 = unset).
    #[prost(uint64, tag = "1")]
    pub frame_id: u64,
    /// Wall-clock timestamp in microseconds (0 = unset).
    #[prost(uint64, tag = "2")]
    pub timestamp: u64,
    /// Arbitrary application payload.
    #[prost(bytes = "vec", tag = "3")]
    pub user_data: Vec<u8>,
}

/// Encode `meta` as a protobuf trailer and return the bytes to append.
///
/// Format: `[ proto bytes ][ u32 LE: proto_len ][ b"RXMT" ]`
pub fn encode_trailer(meta: &FrameMetadata) -> Vec<u8> {
    let proto = meta.encode_to_vec();
    let proto_len = proto.len() as u32;
    let mut trailer = proto;
    trailer.extend_from_slice(&proto_len.to_le_bytes());
    trailer.extend_from_slice(MAGIC);
    trailer
}

/// Detect and decode a trailer from the end of `data`.
///
/// Returns `(metadata, stripped_payload)` on success, `None` if no valid
/// trailer is present (missing magic, truncated length, or decode error).
pub fn decode_and_strip_trailer(data: &[u8]) -> Option<(FrameMetadata, Vec<u8>)> {
    if data.len() < FRAMING {
        return None;
    }
    let tail = data.len();
    if &data[tail - 4..] != MAGIC {
        return None;
    }
    let proto_len = u32::from_le_bytes(data[tail - 8..tail - 4].try_into().ok()?) as usize;
    let proto_start = tail.checked_sub(8 + proto_len)?;
    let meta = FrameMetadata::decode(&data[proto_start..tail - 8]).ok()?;
    let payload = data[..proto_start].to_vec();
    Some((meta, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate a payload with an encoded trailer to form a complete frame buffer.
    fn make_frame(payload: &[u8], meta: &FrameMetadata) -> Vec<u8> {
        let mut frame = payload.to_vec();
        frame.extend_from_slice(&encode_trailer(meta));
        frame
    }

    #[test]
    fn round_trip_all_fields() {
        let meta = FrameMetadata {
            frame_id: 42,
            timestamp: 1_000_000,
            user_data: b"hello".to_vec(),
        };
        let frame = make_frame(b"VIDEO", &meta);
        let (decoded, payload) = decode_and_strip_trailer(&frame).expect("should decode");
        assert_eq!(decoded.frame_id, 42);
        assert_eq!(decoded.timestamp, 1_000_000);
        assert_eq!(decoded.user_data, b"hello");
        assert_eq!(payload, b"VIDEO");
    }

    #[test]
    fn round_trip_default_fields() {
        let meta = FrameMetadata::default();
        let frame = make_frame(b"FRAME", &meta);
        let (decoded, payload) = decode_and_strip_trailer(&frame).expect("should decode");
        assert_eq!(decoded.frame_id, 0);
        assert_eq!(decoded.timestamp, 0);
        assert!(decoded.user_data.is_empty());
        assert_eq!(payload, b"FRAME");
    }

    #[test]
    fn round_trip_large_user_data() {
        let meta = FrameMetadata {
            user_data: vec![0u8; 1024],
            ..Default::default()
        };
        let video = b"BIGFRAME";
        let frame = make_frame(video, &meta);
        let (decoded, payload) = decode_and_strip_trailer(&frame).expect("should decode");
        assert_eq!(decoded.user_data.len(), 1024);
        assert!(decoded.user_data.iter().all(|&b| b == 0));
        assert_eq!(payload, video as &[u8]);
    }

    #[test]
    fn round_trip_empty_payload() {
        let meta = FrameMetadata {
            frame_id: 1,
            timestamp: 2,
            ..Default::default()
        };
        let frame = make_frame(b"", &meta);
        let (decoded, payload) = decode_and_strip_trailer(&frame).expect("should decode");
        assert_eq!(decoded.frame_id, 1);
        assert_eq!(decoded.timestamp, 2);
        assert!(payload.is_empty());
    }

    #[test]
    fn no_magic_returns_none() {
        let data = b"just some raw bytes without any trailer";
        assert!(decode_and_strip_trailer(data).is_none());
    }

    #[test]
    fn wrong_magic_returns_none() {
        let meta = FrameMetadata {
            frame_id: 7,
            ..Default::default()
        };
        let mut frame = make_frame(b"PAY", &meta);
        // Overwrite the last 4 bytes (magic) with something invalid.
        let len = frame.len();
        frame[len - 4..].copy_from_slice(b"XXXX");
        assert!(decode_and_strip_trailer(&frame).is_none());
    }

    #[test]
    fn too_short_returns_none() {
        assert!(decode_and_strip_trailer(b"").is_none());
        assert!(decode_and_strip_trailer(b"SHORT").is_none()); // 5 bytes < FRAMING (8)
        assert!(decode_and_strip_trailer(b"1234567").is_none()); // 7 bytes < FRAMING (8)
    }

    #[test]
    fn truncated_proto_returns_none() {
        // Claim proto_len = 100 but supply only the 8-byte framing with no proto bytes.
        // checked_sub(8 + 100) on a length-8 buffer underflows → None.
        let mut data = Vec::new();
        data.extend_from_slice(&100u32.to_le_bytes()); // proto_len field
        data.extend_from_slice(b"RXMT"); // magic
        assert!(decode_and_strip_trailer(&data).is_none());
    }

    #[test]
    fn trailer_not_confused_with_payload() {
        // A payload that contains b"RXMT" in the middle must not fool the decoder;
        // the real trailer is always at the end.
        let meta = FrameMetadata {
            frame_id: 99,
            ..Default::default()
        };
        let mut payload = b"DATA".to_vec();
        payload.extend_from_slice(b"RXMT"); // spurious magic inside payload
        payload.extend_from_slice(b"MORE");
        let frame = make_frame(&payload, &meta);
        let (decoded, stripped) = decode_and_strip_trailer(&frame).expect("should decode");
        assert_eq!(decoded.frame_id, 99);
        assert_eq!(stripped, payload.as_slice());
    }
}
