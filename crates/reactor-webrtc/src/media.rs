//! Media tracks and decoded frames.
//!
//! A [`Track`] wraps a native media track (local or remote, audio or video).
//! Local tracks created via [`crate::PeerConnectionFactory::create_video_track`]
//! / [`create_audio_track`](crate::PeerConnectionFactory::create_audio_track)
//! can push frames; remote tracks delivered to
//! [`PeerConnectionObserver::on_track`](crate::PeerConnectionObserver) can have
//! a frame sink attached. Audio send is via the factory ADM
//! ([`crate::PeerConnectionFactory::push_audio_frame`]).

use crate::{Error, FactoryHandle, Result};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const SENDER_META_CAP: usize = 300;

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
    /// Metadata attached by the sender via
    /// [`Track::push_video_frame_with_metadata`], decoded from the packet trailer.
    ///
    /// `None` when no trailer was present — which includes every frame from a peer
    /// that did not negotiate the capability, and every frame on a connection built
    /// with [`RtcConfiguration::frame_metadata`](crate::RtcConfiguration) off.
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
type SenderMetaMap = Mutex<(HashMap<i64, crate::metadata::FrameMetadata>, VecDeque<i64>)>;

/// A [`Track`]'s outgoing metadata, keyed by the frame's capture timestamp.
///
/// Registered in [`crate::sender_meta`] under the native track's identity so that
/// `set_remote_description` can find it when it installs the sender transform.
#[derive(Default)]
pub(crate) struct CaptureTimeMeta(SenderMetaMap);

impl crate::sender_meta::SenderMetaSource for CaptureTimeMeta {
    fn take(&self, frame: &crate::encoded::EncodedFrame) -> Option<crate::metadata::FrameMetadata> {
        // Look up rather than remove, and only with a real timestamp: simulcast
        // layers of one frame share capture_time_ms and each must find the entry.
        if frame.capture_time_ms <= 0 {
            return None;
        }
        self.0
            .lock()
            .ok()
            .and_then(|g| g.0.get(&frame.capture_time_ms).cloned())
    }
}

// receiver: FIFO queue written by the strip transform and drained by
// video_sink_tramp; preserves ordering when there is no packet loss.
use crate::sender_meta::ReceiverMetaQueue;

// Heap-pinned sink state behind the C userdata pointer.
struct VideoSinkState {
    cb: Mutex<VideoSinkCb>,
    // The track's inbound metadata queue, always present. It is simply empty when
    // no strip transform is installed or the sender attached no trailer, so there
    // is no state to switch on and no back-fill ordering to get right.
    receiver_meta: Arc<ReceiverMetaQueue>,
}
struct AudioSinkState {
    cb: Mutex<AudioSinkCb>,
}

extern "C" fn video_sink_tramp(ud: *mut c_void, bgra: *const u8, width: c_int, height: c_int) {
    let st = unsafe { &*(ud as *const VideoSinkState) };
    let len = (width as usize) * (height as usize) * 4;
    let slice = unsafe { std::slice::from_raw_parts(bgra, len) };
    let metadata = st.receiver_meta.lock().ok().and_then(|mut q| q.pop_front());
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
    // Mutex, not a plain field, so on_video_frame/on_audio_frame can take &self:
    // Transceiver.set_track needs to share a Track behind an Arc (for the Python
    // binding's spawn_blocking future), which rules out ever taking &mut self.
    video_sink: Mutex<Option<Box<VideoSinkState>>>,
    audio_sink: Mutex<Option<Box<AudioSinkState>>>,
    // Always present: push_video_frame_with_metadata fills it, and the sender
    // transform the peer connection installs after negotiation reads it. There is
    // nothing to switch on — an empty map simply yields no metadata.
    sender_meta: Arc<CaptureTimeMeta>,
    // Identity of the native track, which is how `sender_meta` is registered and
    // therefore how a transceiver finds it. Cached because Drop needs it.
    native_id: usize,
    // Always present, shared with VideoSinkState and with the strip transform the
    // peer connection installs after negotiation.
    receiver_meta: Arc<ReceiverMetaQueue>,
    // Monotonic per-track frame counter; wraps from u64::MAX to 1 (0 = unset).
    frame_counter: AtomicU64,
    // Last capture_ms allocated by alloc_send_capture_us; ensures strictly-
    // increasing keys in the sender map even when two pushes land in the
    // same millisecond.
    last_send_ms: AtomicI64,
    // Keeps the factory's signaling/worker/network threads alive for as long
    // as this track exists — destroying a track dispatches onto them, whether
    // the track is a local one this factory produced or a remote one the
    // observer delivered over a connection this factory created.
    _factory: Arc<FactoryHandle>,
}

// SAFETY: the native track is internally thread-safe; sink callbacks and sink
// (re)attachment are both guarded by a Mutex, and the other `&self` methods
// (frame push) go through WebRTC's internally-locked broadcaster, so a
// `&Track` may be shared and used concurrently across threads.
unsafe impl Send for Track {}
unsafe impl Sync for Track {}

impl Track {
    pub(crate) fn from_raw(
        raw: *mut reactor_webrtc_sys::MediaStreamTrack,
        kind: MediaKind,
        factory: Arc<FactoryHandle>,
    ) -> Self {
        let sender_meta = Arc::new(CaptureTimeMeta::default());
        let receiver_meta: Arc<ReceiverMetaQueue> = Arc::new(ReceiverMetaQueue::default());
        let native_id = unsafe { reactor_webrtc_sys::reactor_webrtc_media_stream_track_id(raw) };
        // Video only: audio carries no metadata, so registering it would just put
        // an entry in the way of nothing.
        if kind == MediaKind::Video {
            let source: Arc<dyn crate::sender_meta::SenderMetaSource> = sender_meta.clone();
            crate::sender_meta::register(native_id, &source);
            crate::sender_meta::register_receiver(native_id, &receiver_meta);
        }
        Self {
            raw,
            kind,
            video_sink: Mutex::new(None),
            audio_sink: Mutex::new(None),
            sender_meta,
            native_id,
            receiver_meta,
            frame_counter: AtomicU64::new(0),
            last_send_ms: AtomicI64::new(0),
            _factory: factory,
        }
    }

    pub(crate) fn raw(&self) -> *mut reactor_webrtc_sys::MediaStreamTrack {
        self.raw
    }

    /// Identity of the native track, comparable with
    /// `reactor_webrtc_rtp_transceiver_sender_track_id`.
    pub(crate) fn native_id(&self) -> usize {
        self.native_id
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
    ///
    /// Always accepted, whether or not a sender transform is installed and whether
    /// or not the peer declared support. The map is capacity-bounded and evicts in
    /// insertion order, so entries nobody reads are reclaimed rather than
    /// accumulated.
    pub(crate) fn insert_sender_meta(&self, capture_ms: i64, meta: crate::metadata::FrameMetadata) {
        if let Ok(mut guard) = self.sender_meta.0.lock() {
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

    /// Subscribe to decoded frames from a (remote) video track. Replaces any
    /// previous sink. The closure runs on a WebRTC thread.
    ///
    /// [`VideoFrame::metadata`] is populated whenever the sender included a
    /// metadata trailer and the peer connection installed the strip transform —
    /// which it does automatically, on both peers, once the capability has been
    /// negotiated. Nothing here has to be arranged for it.
    pub fn on_video_frame(&self, cb: impl for<'a> FnMut(VideoFrame<'a>) + Send + 'static) {
        let state = Box::new(VideoSinkState {
            cb: Mutex::new(Box::new(cb)),
            receiver_meta: self.receiver_meta.clone(),
        });
        let ud = &*state as *const VideoSinkState as *mut c_void;
        // Held across both the native registration and the Rust-side store: two
        // concurrent callers must not interleave their FFI call with the other's
        // store, or the native side can end up pointing at whichever Box the
        // *other* caller's store just dropped — freed memory the callback thread
        // then reads. Holding the lock over both serializes each caller's pair of
        // steps, so the last one to run leaves a native pointer and a stored Box
        // that always agree with each other.
        let mut guard = self.video_sink.lock().expect("video_sink mutex poisoned");
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_video_track_add_sink(self.raw, ud, video_sink_tramp);
        }
        // Drops the previous sink Box, if any, same as an `Option` field replace.
        *guard = Some(state);
    }

    /// Push interleaved i16 PCM to a local audio track created with
    /// [`PeerConnectionFactory::create_audio_track_with_local_source`]. Delivers
    /// audio directly to the sender's encoder, bypassing the shared ADM. No-op
    /// for tracks backed by the factory ADM or for remote tracks.
    ///
    /// `pcm.len()` must be a multiple of `channels`; a partial trailing frame
    /// is an error. Only one thread should call `push_pcm` at a time for a
    /// given track — concurrent callers produce interleaved, garbage audio.
    pub fn push_pcm(&self, pcm: &[i16], sample_rate: u32, channels: u32) -> Result<()> {
        if channels == 0 {
            return Err(Error::Webrtc("channels must be at least 1".to_owned()));
        }
        if !pcm.len().is_multiple_of(channels as usize) {
            return Err(Error::Webrtc(format!(
                "pcm length ({}) is not a multiple of channels ({})",
                pcm.len(),
                channels
            )));
        }
        let channels = channels as c_int;
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
        Ok(())
    }

    /// Subscribe to decoded PCM from a (remote) audio track. Replaces any
    /// previous sink. The closure runs on a WebRTC thread.
    pub fn on_audio_frame(&self, cb: impl for<'a> FnMut(AudioFrame<'a>) + Send + 'static) {
        let state = Box::new(AudioSinkState {
            cb: Mutex::new(Box::new(cb)),
        });
        let ud = &*state as *const AudioSinkState as *mut c_void;
        // Same reasoning as `on_video_frame`: the lock spans both steps so the
        // two can never interleave across callers.
        let mut guard = self.audio_sink.lock().expect("audio_sink mutex poisoned");
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_audio_track_add_sink(self.raw, ud, audio_sink_tramp);
        }
        // Drops the previous sink Box, if any, same as an `Option` field replace.
        *guard = Some(state);
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        // Guarded on identity inside deregister: an EncodedVideoTrack wrapping this
        // track will have replaced the entry with its own FIFO source, and that one
        // must survive this drop.
        if self.kind == MediaKind::Video {
            let source: Arc<dyn crate::sender_meta::SenderMetaSource> = self.sender_meta.clone();
            crate::sender_meta::deregister(self.native_id, &source);
            crate::sender_meta::deregister_receiver(self.native_id, &self.receiver_meta);
        }
        // Detaches the C++ sink before the sink-state boxes are freed.
        unsafe { reactor_webrtc_sys::reactor_webrtc_media_stream_track_destroy(self.raw) }
    }
}
