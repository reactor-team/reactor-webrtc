//! Where the frame-metadata transforms find the state they need, and how they are
//! built.
//!
//! [`PeerConnection::set_remote_description`](crate::PeerConnection::set_remote_description)
//! installs the sender transform once the remote peer has declared that it strips
//! trailers. At that point it has a transceiver, and it needs the metadata queued
//! by whichever [`Track`](crate::Track) or
//! [`EncodedVideoTrack`](crate::EncodedVideoTrack) the caller attached to it —
//! state that lives in the caller's own object.
//!
//! There is no path from a transceiver back to that object. `Transceiver` carries
//! only a raw pointer, its handles are recreated per `transceivers()` call so
//! per-handle state does not persist, and wrapping the sender's native track in a
//! fresh [`Track`](crate::Track) would produce something with its own empty state
//! — the opposite of what is wanted.
//!
//! What the two *do* share is the native track they wrap. So tracks register
//! their metadata source in [`REGISTRY`] under that native identity
//! (`reactor_webrtc_rtp_transceiver_sender_track_id`), and the install step looks
//! it up. Entries are weak and tracks deregister on drop, so a registered track
//! going away leaves nothing behind and cannot resurrect a stale queue.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::metadata::FrameMetadata;

/// How a track hands the metadata for one outgoing encoded frame to the sender
/// transform.
///
/// The two implementations correlate differently, which is the reason this is a
/// trait rather than one concrete queue:
///
/// * A [`Track`](crate::Track) keys by `capture_time_ms`. Frames are pushed as
///   raw BGRA and the encoder preserves the capture timestamp, so it survives as
///   a join key — and simulcast layers of one frame share it, which is why the
///   lookup does not erase.
/// * An [`EncodedVideoTrack`](crate::EncodedVideoTrack) uses FIFO order. Its
///   frames arrive already encoded and `VideoStreamEncoder::OnFrame` clamps
///   future capture timestamps back to post-time, so two pushes in the same
///   millisecond would collide on a timestamp key.
pub(crate) trait SenderMetaSource: Send + Sync {
    /// The metadata for `frame`, if any was queued for it.
    fn take(&self, frame: &crate::encoded::EncodedFrame) -> Option<FrameMetadata>;
}

type Registry = Mutex<HashMap<usize, Weak<dyn SenderMetaSource>>>;

/// Native-track identity → the metadata source of the Rust track wrapping it.
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

/// The mirror of [`registry`] for the receive direction: native-track identity →
/// the queue the strip transform pushes into and the video sink drains.
///
/// Same reasoning as the send side. The queue has to be the one belonging to the
/// remote [`Track`](crate::Track) that `on_track` handed to the caller, because
/// that is the object whose `on_video_frame` will read from it, and
/// `set_remote_description` has only a transceiver to go on.
type RecvRegistry = Mutex<HashMap<usize, Weak<ReceiverMetaQueue>>>;

/// FIFO of metadata stripped from inbound frames, in arrival order.
pub(crate) type ReceiverMetaQueue = Mutex<std::collections::VecDeque<FrameMetadata>>;

fn recv_registry() -> &'static RecvRegistry {
    static REGISTRY: OnceLock<RecvRegistry> = OnceLock::new();
    REGISTRY.get_or_init(RecvRegistry::default)
}

/// Register `queue` as the inbound metadata queue for the native track `track_id`.
pub(crate) fn register_receiver(track_id: usize, queue: &Arc<ReceiverMetaQueue>) {
    if track_id == 0 {
        return;
    }
    if let Ok(mut map) = recv_registry().lock() {
        map.insert(track_id, Arc::downgrade(queue));
    }
}

/// Drop the receive entry for `track_id`, if `queue` is still the registered one.
pub(crate) fn deregister_receiver(track_id: usize, queue: &Arc<ReceiverMetaQueue>) {
    if track_id == 0 {
        return;
    }
    if let Ok(mut map) = recv_registry().lock() {
        let ours = map
            .get(&track_id)
            .is_some_and(|weak| std::ptr::addr_eq(Weak::as_ptr(weak), Arc::as_ptr(queue)));
        if ours {
            map.remove(&track_id);
        }
    }
}

/// The inbound metadata queue registered for `track_id`, if its track is alive.
pub(crate) fn lookup_receiver(track_id: usize) -> Option<Arc<ReceiverMetaQueue>> {
    if track_id == 0 {
        return None;
    }
    let mut map = recv_registry().lock().ok()?;
    match map.get(&track_id).and_then(Weak::upgrade) {
        Some(queue) => Some(queue),
        None => {
            map.remove(&track_id);
            None
        }
    }
}

/// Register `source` as the metadata source for the native track `track_id`.
///
/// Replaces any previous entry: an [`EncodedVideoTrack`](crate::EncodedVideoTrack)
/// wraps a [`Track`](crate::Track) that has already registered its own
/// timestamp-keyed source, and the FIFO one has to win for that native track.
pub(crate) fn register(track_id: usize, source: &Arc<dyn SenderMetaSource>) {
    if track_id == 0 {
        return;
    }
    if let Ok(mut map) = registry().lock() {
        map.insert(track_id, Arc::downgrade(source));
    }
}

/// Drop the entry for `track_id`, if it is still the one `source` registered.
///
/// The guard matters because registration is keyed by native identity and
/// `EncodedVideoTrack` deliberately overwrites its inner `Track`'s entry: when
/// that inner track drops it must not remove the FIFO source that replaced it.
pub(crate) fn deregister(track_id: usize, source: &Arc<dyn SenderMetaSource>) {
    if track_id == 0 {
        return;
    }
    if let Ok(mut map) = registry().lock() {
        let ours = map
            .get(&track_id)
            .is_some_and(|weak| std::ptr::addr_eq(Weak::as_ptr(weak), Arc::as_ptr(source)));
        if ours {
            map.remove(&track_id);
        }
    }
}

/// The metadata source registered for `track_id`, if a live track still holds it.
pub(crate) fn lookup(track_id: usize) -> Option<Arc<dyn SenderMetaSource>> {
    if track_id == 0 {
        return None;
    }
    let mut map = registry().lock().ok()?;
    match map.get(&track_id).and_then(Weak::upgrade) {
        Some(source) => Some(source),
        None => {
            // The track is gone; stop holding the dead weak entry.
            map.remove(&track_id);
            None
        }
    }
}

const RECEIVER_META_CAP: usize = 300;

/// Native track ids whose sender slot a caller has claimed with
/// [`Transceiver::set_sender_transform`](crate::Transceiver::set_sender_transform).
///
/// `SetFrameTransformer` is a single slot, so installing the metadata transform
/// over a caller's own would silently disable a documented feature — and, because
/// the install happens on negotiation, it would do so only against peers that
/// declared support. Whether your transform survives would depend on the far end.
/// A claimed slot is left alone instead; such a caller owns the trailer too, and
/// can append one with [`crate::metadata::encode_trailer`].
fn claimed_sender_slots() -> &'static Mutex<std::collections::HashSet<usize>> {
    static CLAIMED: OnceLock<Mutex<std::collections::HashSet<usize>>> = OnceLock::new();
    CLAIMED.get_or_init(Mutex::default)
}

/// Record that the caller owns the sender slot for `track_id`.
///
/// A no-op for id 0, which is what a transceiver with no track yet reports: there
/// is nothing to key the claim on, so a caller that attaches a sender transform
/// before its track loses the guard. Attach after `set_track` to keep it.
pub(crate) fn claim_sender_slot(track_id: usize) {
    if track_id == 0 {
        return;
    }
    if let Ok(mut set) = claimed_sender_slots().lock() {
        set.insert(track_id);
    }
}

pub(crate) fn sender_slot_claimed(track_id: usize) -> bool {
    claimed_sender_slots()
        .lock()
        .map(|set| set.contains(&track_id))
        .unwrap_or(false)
}

/// Forget the claim for `track_id`, so a recycled native pointer cannot inherit it.
pub(crate) fn release_sender_slot(track_id: usize) {
    if track_id == 0 {
        return;
    }
    if let Ok(mut set) = claimed_sender_slots().lock() {
        set.remove(&track_id);
    }
}

/// Build the sender transform: append a trailer to each outgoing encoded frame
/// for which `source` has metadata, while `gate` is open.
///
/// Installed by
/// [`PeerConnection::set_remote_description`](crate::PeerConnection::set_remote_description)
/// once the remote has declared that it strips trailers. The gate is still
/// consulted per frame rather than trusted at install time, because a
/// renegotiation can close it under a transform that is already attached.
pub(crate) fn sender_transform(
    source: Arc<dyn SenderMetaSource>,
    gate: crate::metadata::FrameMetadataGate,
) -> crate::encoded::FrameTransform {
    crate::encoded::FrameTransform::new(move |frame| {
        if frame.direction != crate::encoded::FrameDirection::Send {
            return crate::encoded::FrameAction::Forward;
        }
        // Ask the source unconditionally, even with the gate shut: a FIFO-backed
        // source has to be drained to stay in step with the frames it belongs to.
        let meta = source.take(frame);
        if gate.is_open() {
            if let Some(ref m) = meta {
                let trailer = crate::metadata::encode_trailer(m);
                let mut new_data = frame.data.to_vec();
                new_data.extend_from_slice(&trailer);
                frame.replace_data(&new_data);
            }
        }
        crate::encoded::FrameAction::Forward
    })
}

/// Build the receiver transform: strip the trailer from each inbound encoded
/// frame and push the metadata onto `queue`, which the track's video sink drains
/// one entry per decoded frame.
///
/// Metadata is delivered in FIFO order. The native transformer fires once per
/// fully-reassembled encoded frame, so packet loss skips the metadata push and the
/// decoded frame together and the queue does not drift. A mismatch needs a
/// decoder-level edge case — a synthesised concealment frame, or H.264
/// non-reference frame discard.
pub(crate) fn receiver_transform(queue: Arc<ReceiverMetaQueue>) -> crate::encoded::FrameTransform {
    // Dedup window: WebRTC can reassemble the same frame more than once when NACK
    // retransmissions arrive after the original packets left the jitter buffer.
    // Duplicates still get stripped, but skip the push so the queue stays 1:1 with
    // decoded frames.
    let seen: Mutex<std::collections::VecDeque<(u32, u32)>> = Mutex::default();
    crate::encoded::FrameTransform::new(move |frame| {
        if frame.direction != crate::encoded::FrameDirection::Receive {
            return crate::encoded::FrameAction::Forward;
        }
        if let Some((meta, stripped)) = crate::metadata::decode_and_strip_trailer(frame.data) {
            frame.replace_data(&stripped);
            let key = (frame.ssrc, frame.timestamp);
            let is_dup = seen.lock().ok().is_some_and(|mut g| {
                if g.contains(&key) {
                    true
                } else {
                    if g.len() >= 32 {
                        g.pop_front();
                    }
                    g.push_back(key);
                    false
                }
            });
            if !is_dup {
                if let Ok(mut guard) = queue.lock() {
                    if guard.len() >= RECEIVER_META_CAP {
                        guard.pop_front();
                    }
                    guard.push_back(meta);
                }
            }
        }
        crate::encoded::FrameAction::Forward
    })
}
