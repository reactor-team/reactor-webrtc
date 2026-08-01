//! Media tracks and decoded frames.
//!
//! A [`Track`] wraps a native media track (local or remote, audio or video).
//! Local tracks created via [`crate::PeerConnectionFactory::create_video_track`]
//! / [`create_audio_track`](crate::PeerConnectionFactory::create_audio_track)
//! can push frames; remote tracks delivered to
//! [`PeerConnectionObserver::on_track`](crate::PeerConnectionObserver) can have
//! a frame sink attached. Audio send is via the factory ADM
//! ([`crate::PeerConnectionFactory::push_audio_frame`]).

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const SENDER_META_CAP: usize = 300;
const RECEIVER_META_CAP: usize = 300;

/// Audio vs. video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
    Unknown,
}

impl MediaKind {
    pub(crate) fn from_raw(kind: c_int) -> Self {
        match kind {
            0 => MediaKind::Audio,
            1 => MediaKind::Video,
            _ => MediaKind::Unknown,
        }
    }
}

/// A decoded video frame, borrowed for the duration of the callback. The buffer
/// is BGRA (`width*height*4`, B-G-R-A); copy it if you need to keep it.
pub struct VideoFrame<'a> {
    pub bgra: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// Metadata attached by the sender via [`Track::push_video_frame_with_metadata`]
    /// or [`Track::sender_metadata_transform`], decoded from the packet trailer.
    /// `None` when no trailer was present in the encoded frame.
    pub metadata: Option<crate::metadata::FrameMetadata>,
}

/// A chunk of decoded audio, borrowed for the duration of the callback:
/// interleaved signed 16-bit PCM (`frames * channels` samples).
pub struct AudioFrame<'a> {
    pub pcm: &'a [i16],
    pub sample_rate: u32,
    pub channels: u32,
    pub frames: u32,
}

type VideoSinkCb = Box<dyn for<'a> FnMut(VideoFrame<'a>) + Send>;
type AudioSinkCb = Box<dyn for<'a> FnMut(AudioFrame<'a>) + Send>;

// sender: HashMap keyed by capture_time_ms (no-erase for simulcast), evicted
// FIFO when the 300-entry cap is reached.
type SenderMetaMap = Arc<Mutex<(HashMap<i64, crate::metadata::FrameMetadata>, VecDeque<i64>)>>;

// receiver: FIFO queue written by the receiver FrameTransform and drained by
// video_sink_tramp; preserves ordering when there is no packet loss.
type ReceiverMetaQueue = Arc<Mutex<VecDeque<crate::metadata::FrameMetadata>>>;

// Heap-pinned sink state behind the C userdata pointer.
struct VideoSinkState {
    cb: Mutex<VideoSinkCb>,
    // Populated by receiver_metadata_transform(); None when metadata is not in use.
    receiver_meta: Option<ReceiverMetaQueue>,
}
struct AudioSinkState {
    cb: Mutex<AudioSinkCb>,
}

extern "C" fn video_sink_tramp(ud: *mut c_void, bgra: *const u8, width: c_int, height: c_int) {
    let st = unsafe { &*(ud as *const VideoSinkState) };
    let len = (width as usize) * (height as usize) * 4;
    let slice = unsafe { std::slice::from_raw_parts(bgra, len) };
    let metadata = st
        .receiver_meta
        .as_ref()
        .and_then(|q| q.lock().ok()?.pop_front());
    if let Ok(mut cb) = st.cb.lock() {
        cb(VideoFrame {
            bgra: slice,
            width: width as u32,
            height: height as u32,
            metadata,
        });
    }
}

extern "C" fn audio_sink_tramp(
    ud: *mut c_void,
    pcm: *const i16,
    sample_rate: c_int,
    channels: c_int,
    frames: c_int,
) {
    let st = unsafe { &*(ud as *const AudioSinkState) };
    let len = (frames as usize) * (channels as usize);
    let slice = unsafe { std::slice::from_raw_parts(pcm, len) };
    if let Ok(mut cb) = st.cb.lock() {
        cb(AudioFrame {
            pcm: slice,
            sample_rate: sample_rate as u32,
            channels: channels as u32,
            frames: frames as u32,
        });
    }
}

/// A media track (local or remote). Dropping it detaches any sink and releases
/// the native track.
pub struct Track {
    raw: *mut reactor_webrtc_sys::MediaStreamTrack,
    kind: MediaKind,
    video_sink: Option<Box<VideoSinkState>>,
    audio_sink: Option<Box<AudioSinkState>>,
    // Set by sender_metadata_transform(); populated by push_video_frame_with_metadata.
    sender_meta: Option<SenderMetaMap>,
    // Set by receiver_metadata_transform(); shared with VideoSinkState.
    receiver_meta: Option<ReceiverMetaQueue>,
    // Monotonic per-track frame counter; wraps from u64::MAX to 1 (0 = unset).
    frame_counter: AtomicU64,
    // Last capture_ms allocated by alloc_send_capture_us; ensures strictly-
    // increasing keys in the sender map even when two pushes land in the
    // same millisecond.
    last_send_ms: AtomicI64,
}

// SAFETY: the native track is internally thread-safe; sink callbacks are
// guarded by a Mutex, and the `&self` methods (frame push) go through WebRTC's
// internally-locked broadcaster, so a `&Track` may be shared across threads.
// Sink (re)attachment takes `&mut self`, so it cannot race a shared push.
unsafe impl Send for Track {}
unsafe impl Sync for Track {}

impl Track {
    pub(crate) fn from_raw(
        raw: *mut reactor_webrtc_sys::MediaStreamTrack,
        kind: MediaKind,
    ) -> Self {
        Self {
            raw,
            kind,
            video_sink: None,
            audio_sink: None,
            sender_meta: None,
            receiver_meta: None,
            frame_counter: AtomicU64::new(0),
            last_send_ms: AtomicI64::new(0),
        }
    }

    pub(crate) fn raw(&self) -> *mut reactor_webrtc_sys::MediaStreamTrack {
        self.raw
    }

    pub fn kind(&self) -> MediaKind {
        self.kind
    }

    /// Push a BGRA frame (`width*height*4` bytes) into a local video track.
    /// No-op for non-video tracks.
    pub fn push_video_frame(&self, bgra: &[u8], width: u32, height: u32) {
        if self.kind != MediaKind::Video {
            return;
        }
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_track_push_frame(
                self.raw,
                bgra.as_ptr(),
                width as c_int,
                height as c_int,
            );
        }
    }

    /// Push a BGRA frame with arbitrary `user_data` embedded in a protobuf
    /// trailer. `frame_id` (monotonic counter) and `timestamp` (Unix epoch µs)
    /// are computed internally.
    ///
    /// Requires [`sender_metadata_transform`](Self::sender_metadata_transform)
    /// to be attached to the sender transceiver; otherwise the frame goes out
    /// without a trailer.
    ///
    /// No-op for non-video tracks.
    pub fn push_video_frame_with_metadata(
        &self,
        bgra: &[u8],
        width: u32,
        height: u32,
        user_data: &[u8],
    ) {
        if self.kind != MediaKind::Video {
            return;
        }
        let capture_us = self.alloc_send_capture_us();
        let meta = crate::metadata::FrameMetadata {
            frame_id: self.next_frame_id(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
            user_data: user_data.to_vec(),
        };
        self.insert_sender_meta(capture_us / 1000, meta);
        self.push_video_frame_ts(bgra, width, height, capture_us);
    }

    /// Return a strictly-increasing capture timestamp in microseconds for use
    /// as the sender-map key and `push_video_frame_ts` argument.
    ///
    /// Two calls in the same millisecond would produce the same `capture_ms`
    /// key, causing both frames to share the same metadata entry. This method
    /// advances the counter by 1 ms if the real clock hasn't moved, guaranteeing
    /// unique keys regardless of call frequency.
    pub(crate) fn alloc_send_capture_us(&self) -> i64 {
        let raw_ms = unsafe { reactor_webrtc_sys::reactor_webrtc_time_micros() } / 1000;
        let mut prev = self.last_send_ms.load(Ordering::Relaxed);
        loop {
            let next = raw_ms.max(prev + 1);
            match self.last_send_ms.compare_exchange_weak(
                prev,
                next,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next * 1000,
                Err(actual) => prev = actual,
            }
        }
    }

    /// Advance the per-track frame counter and return the new value (1-based,
    /// wraps from `u64::MAX` back to 1; 0 is reserved to mean "unset").
    pub(crate) fn next_frame_id(&self) -> u64 {
        let raw = self.frame_counter.fetch_add(1, Ordering::Relaxed);
        if raw == u64::MAX {
            1
        } else {
            raw + 1
        }
    }

    /// Insert a metadata entry keyed by `capture_ms` into the sender map.
    /// No-op when [`sender_metadata_transform`](Self::sender_metadata_transform)
    /// has not been called.
    pub(crate) fn insert_sender_meta(&self, capture_ms: i64, meta: crate::metadata::FrameMetadata) {
        if let Some(map) = &self.sender_meta {
            if let Ok(mut guard) = map.lock() {
                let (ref mut hmap, ref mut order) = *guard;
                if order.len() >= SENDER_META_CAP {
                    if let Some(old_key) = order.pop_front() {
                        hmap.remove(&old_key);
                    }
                }
                // No-erase on insert: simulcast layers sharing the same
                // capture_time_ms must all find the same entry.
                hmap.entry(capture_ms).or_insert(meta);
                order.push_back(capture_ms);
            }
        }
    }

    /// Push a BGRA frame with an explicit WebRTC capture timestamp (µs).
    /// Used internally to correlate metadata with the sender FrameTransform.
    pub(crate) fn push_video_frame_ts(
        &self,
        bgra: &[u8],
        width: u32,
        height: u32,
        capture_us: i64,
    ) {
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_track_push_frame_ts(
                self.raw,
                bgra.as_ptr(),
                width as c_int,
                height as c_int,
                capture_us,
            );
        }
    }

    /// Return a [`FrameTransform`](crate::FrameTransform) that appends a
    /// protobuf metadata trailer to each encoded frame on the send path.
    ///
    /// Attach this to the sender transceiver with
    /// `Transceiver::set_sender_transform` **before** the first SDP exchange.
    /// Then call [`push_video_frame_with_metadata`](Self::push_video_frame_with_metadata)
    /// to include metadata with each frame.
    ///
    /// If `push_video_frame` is called (no metadata), the transform forwards
    /// the frame unchanged.
    pub fn sender_metadata_transform(&mut self) -> crate::encoded::FrameTransform {
        let map: SenderMetaMap = Arc::new(Mutex::new((HashMap::new(), VecDeque::new())));
        self.sender_meta = Some(map.clone());
        crate::encoded::FrameTransform::new(move |frame| {
            if frame.direction != crate::encoded::FrameDirection::Send {
                return crate::encoded::FrameAction::Forward;
            }
            // Look up by the frame's capture timestamp (ms precision). No-erase
            // so simulcast layers sharing the same capture_time_ms all find it.
            let meta = if frame.capture_time_ms > 0 {
                map.lock()
                    .ok()
                    .and_then(|g| g.0.get(&frame.capture_time_ms).cloned())
            } else {
                None
            };
            if let Some(ref m) = meta {
                let trailer = crate::metadata::encode_trailer(m);
                let mut new_data = frame.data.to_vec();
                new_data.extend_from_slice(&trailer);
                frame.replace_data(&new_data);
            }
            crate::encoded::FrameAction::Forward
        })
    }

    /// Return a [`FrameTransform`](crate::FrameTransform) that strips the
    /// protobuf metadata trailer from received encoded frames and delivers the
    /// metadata to subsequent [`on_video_frame`](Self::on_video_frame) callbacks
    /// via [`VideoFrame::metadata`].
    ///
    /// Can be called before or after `on_video_frame` — the shared queue is
    /// back-filled into an already-attached sink automatically.
    ///
    /// Metadata is delivered in FIFO order. The `FrameTransformerInterface`
    /// fires once per fully-reassembled encoded frame, so packet loss causes
    /// both the metadata push and the decoded BGRA to be skipped together —
    /// the queue does not drift under normal packet loss. A mismatch can only
    /// occur in rare decoder-level edge cases (e.g. synthesised concealment
    /// frames or H.264 non-reference frame discard).
    pub fn receiver_metadata_transform(&mut self) -> crate::encoded::FrameTransform {
        let queue: ReceiverMetaQueue = Arc::new(Mutex::new(VecDeque::new()));
        self.receiver_meta = Some(queue.clone());
        // Back-fill into an already-attached sink so call order doesn't matter.
        if let Some(sink) = &mut self.video_sink {
            sink.receiver_meta = Some(queue.clone());
        }
        // Dedup window: WebRTC can reassemble the same frame multiple times when
        // NACK retransmissions arrive after the original packets were already
        // consumed from the jitter buffer.  Track the last 32 (ssrc, rtp_ts)
        // pairs; duplicates still have their trailer stripped but skip the
        // metadata push so the queue stays in 1:1 sync with decoded frames.
        let seen: Arc<Mutex<VecDeque<(u32, u32)>>> = Arc::new(Mutex::new(VecDeque::new()));
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

    /// Subscribe to decoded frames from a (remote) video track. Replaces any
    /// previous sink. The closure runs on a WebRTC thread.
    ///
    /// If [`receiver_metadata_transform`](Self::receiver_metadata_transform) was
    /// called and its transform is attached to the receiver transceiver,
    /// [`VideoFrame::metadata`] is populated whenever the sender included a
    /// metadata trailer.
    pub fn on_video_frame(&mut self, cb: impl for<'a> FnMut(VideoFrame<'a>) + Send + 'static) {
        let state = Box::new(VideoSinkState {
            cb: Mutex::new(Box::new(cb)),
            receiver_meta: self.receiver_meta.clone(),
        });
        let ud = &*state as *const VideoSinkState as *mut c_void;
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_track_add_sink(self.raw, ud, video_sink_tramp);
        }
        self.video_sink = Some(state);
    }

    /// Push interleaved i16 PCM to a local audio track created with
    /// [`PeerConnectionFactory::create_audio_track_with_local_source`]. Delivers
    /// audio directly to the sender's encoder, bypassing the shared ADM. No-op
    /// for tracks backed by the factory ADM or for remote tracks.
    pub fn push_pcm(&self, pcm: &[i16], sample_rate: u32, channels: u32) {
        let channels = channels.max(1) as c_int;
        let samples_per_channel = (pcm.len() / channels as usize) as c_int;
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_audio_track_push_pcm(
                self.raw,
                pcm.as_ptr(),
                samples_per_channel,
                sample_rate as c_int,
                channels,
            );
        }
    }

    /// Subscribe to decoded PCM from a (remote) audio track. Replaces any
    /// previous sink. The closure runs on a WebRTC thread.
    pub fn on_audio_frame(&mut self, cb: impl for<'a> FnMut(AudioFrame<'a>) + Send + 'static) {
        let state = Box::new(AudioSinkState {
            cb: Mutex::new(Box::new(cb)),
        });
        let ud = &*state as *const AudioSinkState as *mut c_void;
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_audio_track_add_sink(self.raw, ud, audio_sink_tramp);
        }
        self.audio_sink = Some(state);
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        // Detaches the C++ sink before the sink-state boxes are freed.
        unsafe { reactor_webrtc_sys::reactor_webrtc_media_stream_track_destroy(self.raw) }
    }
}
