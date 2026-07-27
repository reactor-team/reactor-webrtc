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
use std::sync::atomic::{AtomicU64, Ordering};
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
type SenderMetaMap =
    Arc<Mutex<(HashMap<i64, crate::metadata::FrameMetadata>, VecDeque<i64>)>>;

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

        // frame_id: monotonic, wraps from u64::MAX back to 1 (0 means unset).
        let raw = self.frame_counter.fetch_add(1, Ordering::Relaxed);
        let frame_id = if raw == u64::MAX { 1 } else { raw + 1 };

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;

        let meta = crate::metadata::FrameMetadata {
            frame_id,
            timestamp,
            user_data: user_data.to_vec(),
        };

        // Sample the clock once and use it for both the HashMap key and the
        // VideoFrame capture timestamp — the transform callback reads
        // CaptureTime()->ms() which equals capture_us/1000.
        let capture_us =
            unsafe { reactor_webrtc_sys::reactor_webrtc_time_micros() };
        let capture_ms = capture_us / 1000;

        if let Some(map) = &self.sender_meta {
            if let Ok(mut guard) = map.lock() {
                let (ref mut hmap, ref mut order) = *guard;
                if order.len() >= SENDER_META_CAP {
                    if let Some(old_key) = order.pop_front() {
                        hmap.remove(&old_key);
                    }
                }
                // No-erase on insert: simulcast layers sharing the same
                // capture_time_ms (same raw frame, multiple encoded layers)
                // must all find the same entry.
                hmap.entry(capture_ms).or_insert(meta);
                order.push_back(capture_ms);
            }
        }

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
        let map: SenderMetaMap =
            Arc::new(Mutex::new((HashMap::new(), VecDeque::new())));
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
    /// Metadata is delivered in FIFO order (one entry per decoded frame).
    /// Under packet loss a dropped encoded frame consumes its metadata slot,
    /// so `VideoFrame::metadata` may occasionally belong to a different frame
    /// than the BGRA buffer it arrives with.
    pub fn receiver_metadata_transform(&mut self) -> crate::encoded::FrameTransform {
        let queue: ReceiverMetaQueue = Arc::new(Mutex::new(VecDeque::new()));
        self.receiver_meta = Some(queue.clone());
        // Back-fill into an already-attached sink so call order doesn't matter.
        if let Some(sink) = &mut self.video_sink {
            sink.receiver_meta = Some(queue.clone());
        }
        crate::encoded::FrameTransform::new(move |frame| {
            if frame.direction != crate::encoded::FrameDirection::Receive {
                return crate::encoded::FrameAction::Forward;
            }
            if let Some((meta, stripped)) =
                crate::metadata::decode_and_strip_trailer(frame.data)
            {
                if let Ok(mut guard) = queue.lock() {
                    if guard.len() >= RECEIVER_META_CAP {
                        guard.pop_front();
                    }
                    guard.push_back(meta);
                }
                frame.replace_data(&stripped);
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
