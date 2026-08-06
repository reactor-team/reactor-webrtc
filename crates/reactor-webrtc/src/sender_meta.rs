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
//! their metadata source under that native identity
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

/// What the frame-metadata step does on one side of a transceiver.
enum MetaStep {
    /// Send: append a trailer for frames the source has metadata for.
    Embed {
        source: Arc<dyn SenderMetaSource>,
        gate: crate::metadata::FrameMetadataGate,
    },
    /// Receive: strip the trailer and queue the metadata for the video sink.
    Strip {
        queue: Arc<ReceiverMetaQueue>,
        /// Dedup window over (ssrc, rtp timestamp) — WebRTC can reassemble the
        /// same frame more than once when NACK retransmissions arrive after the
        /// original packets left the jitter buffer. Duplicates still get stripped
        /// but skip the queue push, so it stays 1:1 with decoded frames.
        seen: Mutex<std::collections::VecDeque<(u32, u32)>>,
    },
}

/// One side of one transceiver's encoded-frame pipeline.
///
/// libwebrtc gives a sender and a receiver a single `SetFrameTransformer` slot
/// each, so the crate owns it and runs both things that want it: the caller's
/// [`FrameTransform`](crate::FrameTransform) callback, and the frame-metadata step.
/// Without this, attaching a transform would silently disable metadata (or the
/// reverse), and — since metadata is installed on negotiation — which one you got
/// would depend on what the remote declared.
///
/// Either part may be absent and either may be set later, in any order: whichever
/// arrives first installs the native transformer, and the other is picked up on the
/// next frame.
#[derive(Default)]
pub(crate) struct ComposedSlot {
    caller: Mutex<Option<Arc<Mutex<crate::encoded::EncodedCb>>>>,
    meta: Mutex<Option<MetaStep>>,
    installed: std::sync::atomic::AtomicBool,
}

impl ComposedSlot {
    /// Run the composed pipeline for one frame.
    ///
    /// The caller's callback runs **first in both directions**, so it always sees
    /// exactly the bytes that traverse the network: on send, before the trailer is
    /// appended; on receive, before it is stripped. A caller that wants the payload
    /// without the framing can apply
    /// [`decode_and_strip_trailer`](crate::metadata::decode_and_strip_trailer)
    /// itself. Dropping the frame skips the metadata step entirely — there will be
    /// no decoded frame for it to belong to.
    fn run(&self, frame: &crate::encoded::EncodedFrame) -> crate::encoded::FrameAction {
        let caller = self.caller.lock().ok().and_then(|g| g.clone());
        if let Some(cb) = caller {
            let action = match cb.lock() {
                Ok(mut cb) => cb(frame),
                Err(_) => crate::encoded::FrameAction::Forward,
            };
            if action == crate::encoded::FrameAction::Drop {
                return crate::encoded::FrameAction::Drop;
            }
        }
        if let Ok(meta) = self.meta.lock() {
            match meta.as_ref() {
                Some(MetaStep::Embed { source, gate }) => {
                    // Ask the source even with the gate shut: a FIFO-backed source
                    // has to be drained to stay in step with its frames.
                    let m = source.take(frame);
                    if gate.is_open() {
                        if let Some(ref m) = m {
                            let trailer = crate::metadata::encode_trailer(m);
                            let mut out = frame.data.to_vec();
                            out.extend_from_slice(&trailer);
                            frame.replace_data(&out);
                        }
                    }
                }
                Some(MetaStep::Strip { queue, seen }) => {
                    if let Some((m, stripped)) =
                        crate::metadata::decode_and_strip_trailer(frame.data)
                    {
                        frame.replace_data(&stripped);
                        let key = (frame.ssrc, frame.timestamp);
                        let dup = seen.lock().ok().is_some_and(|mut g| {
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
                        if !dup {
                            if let Ok(mut q) = queue.lock() {
                                if q.len() >= RECEIVER_META_CAP {
                                    q.pop_front();
                                }
                                q.push_back(m);
                            }
                        }
                    }
                }
                None => {}
            }
        }
        crate::encoded::FrameAction::Forward
    }

    fn set_caller(&self, cb: Arc<Mutex<crate::encoded::EncodedCb>>) {
        if let Ok(mut slot) = self.caller.lock() {
            *slot = Some(cb);
        }
    }

    fn set_meta(&self, step: MetaStep) {
        if let Ok(mut slot) = self.meta.lock() {
            *slot = Some(step);
        }
    }

    /// True the first time it is called, so the native transformer is attached once.
    fn claim_install(&self) -> bool {
        !self
            .installed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    }
}

/// Which side of a transceiver a slot belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Side {
    Send,
    Receive,
}

type SlotRegistry = Mutex<HashMap<(usize, Side), Arc<ComposedSlot>>>;

/// Keyed by the *transceiver's* native identity, which is stable from the moment
/// the transceiver exists — before a track is attached and before a mid is
/// assigned. (The handle pointer is not: `transceivers()` allocates a fresh one
/// each call.)
fn slots() -> &'static SlotRegistry {
    static SLOTS: OnceLock<SlotRegistry> = OnceLock::new();
    SLOTS.get_or_init(SlotRegistry::default)
}

fn slot_for(transceiver_id: usize, side: Side) -> Option<Arc<ComposedSlot>> {
    if transceiver_id == 0 {
        return None;
    }
    let mut map = slots().lock().ok()?;
    Some(Arc::clone(map.entry((transceiver_id, side)).or_default()))
}

/// Drop both slots for a transceiver, so a recycled native pointer starts clean.
pub(crate) fn forget_transceiver(transceiver_id: usize) {
    if transceiver_id == 0 {
        return;
    }
    if let Ok(mut map) = slots().lock() {
        map.remove(&(transceiver_id, Side::Send));
        map.remove(&(transceiver_id, Side::Receive));
    }
}

/// Build the native transformer that runs `slot`.
fn native_for(slot: Arc<ComposedSlot>) -> crate::encoded::NativeTransform {
    crate::encoded::NativeTransform::new(move |frame| slot.run(frame))
}

/// Register a caller callback on one side, installing the native transformer if it
/// is not there yet. Returns the transformer to attach, or `None` if the slot was
/// already installed (nothing to attach — the existing one picks the callback up).
pub(crate) fn attach_caller(
    transceiver_id: usize,
    side: Side,
    cb: Arc<Mutex<crate::encoded::EncodedCb>>,
) -> Option<crate::encoded::NativeTransform> {
    let slot = slot_for(transceiver_id, side)?;
    slot.set_caller(cb);
    slot.claim_install().then(|| native_for(slot))
}

/// Configure the send-side metadata step, installing the transformer if needed.
pub(crate) fn attach_embed(
    transceiver_id: usize,
    source: Arc<dyn SenderMetaSource>,
    gate: crate::metadata::FrameMetadataGate,
) -> Option<crate::encoded::NativeTransform> {
    let slot = slot_for(transceiver_id, Side::Send)?;
    slot.set_meta(MetaStep::Embed { source, gate });
    slot.claim_install().then(|| native_for(slot))
}

/// Configure the receive-side metadata step, installing the transformer if needed.
pub(crate) fn attach_strip(
    transceiver_id: usize,
    queue: Arc<ReceiverMetaQueue>,
) -> Option<crate::encoded::NativeTransform> {
    let slot = slot_for(transceiver_id, Side::Receive)?;
    slot.set_meta(MetaStep::Strip {
        queue,
        seen: Mutex::default(),
    });
    slot.claim_install().then(|| native_for(slot))
}
