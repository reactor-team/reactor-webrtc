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

/// Where an audio track's samples come from
/// ([`create_audio_track_with_options`](crate::PeerConnectionFactory::create_audio_track_with_options)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioTrackSource {
    /// The factory's ADM: the platform microphone + speaker (real devices on
    /// desktop) or the shared synthetic pipe fed by
    /// [`PeerConnectionFactory::push_audio_frame`](crate::PeerConnectionFactory::push_audio_frame).
    /// Every ADM-sourced track carries the **same** signal.
    ///
    /// There is exactly one ADM per factory — per-track choice is "tap it or
    /// bypass it", not "which ADM".
    ///
    /// On the **platform** device, the render path is fully automatic: every
    /// inbound audio track decodes to the speaker by default — you cannot
    /// route one remote track to the speaker and another elsewhere through
    /// plumbing. `on_frame` taps them, it doesn't divert. Nothing plays
    /// automatically through the synthetic device.
    #[default]
    Adm,
    /// A per-track push source, fully independent of the ADM: feed it with
    /// [`Track::push_pcm`]. The way to send per-track audio — e.g. a music
    /// track next to the mic — or to route different audio to different
    /// peers from one factory.
    LocalPush,
}

/// Options for
/// [`PeerConnectionFactory::create_audio_track_with_options`].
///
/// All fields optional; `Default::default()` produces exactly what
/// [`PeerConnectionFactory::create_audio_track`] produces. The struct is
/// `#[non_exhaustive]` — construct via `Default` and assign fields.
///
/// The processing flags are **constraints refining what this track asks of
/// the factory's APM chain** (the one
/// [`PeerConnectionFactoryBuilder::with_apm`] built — default: none):
/// `None` inherits the chain's state, `Some(v)` forces the stage on/off for
/// this track's source. They act **one level above the flags list** — with
/// libwebrtc semantics this crate honors: a flag set per constraint only
/// takes effect by engaging part of an APM the factory already has.
/// **On a factory without an APM (the default) they are inert** — there is
/// nothing in the capture pipeline for them to toggle; that is not a bug,
/// the DSP chain genuinely doesn't exist. They constrain the capture/send
/// side only — received audio is never processed.
///
/// A [`LocalPush`](AudioTrackSource::LocalPush) source bypasses the ADM
/// device path entirely, so these flags only ever matter on
/// [`AudioTrackSource::Adm`] — pushed audio has no echo to cancel.
///
/// ```rust,ignore
/// let mic = factory.create_audio_track_with_options("mic", {
///     let mut o = AudioTrackOptions::default();
///     o.echo_cancellation = Some(true);      // target the APM chain
///     o.noise_suppression = Some(true);
///     o
/// })?;
/// let music = factory.create_audio_track_with_options("music", {
///     let mut o = AudioTrackOptions::default();
///     o.source = AudioTrackSource::LocalPush;
///     o
/// })?;
/// ```
#[derive(Default)]
#[non_exhaustive]
pub struct AudioTrackOptions {
    /// Where this track's samples come from. Default: [`AudioTrackSource::Adm`].
    pub source: AudioTrackSource,
    /// AEC3 echo cancellation for this track's source.
    pub echo_cancellation: Option<bool>,
    /// Noise suppression for this track's source.
    pub noise_suppression: Option<bool>,
    /// Automatic gain control for this track's source.
    pub auto_gain_control: Option<bool>,
    /// High-pass filter for this track's source.
    pub high_pass_filter: Option<bool>,
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
    // Tracks delivered by the observer are inbound-only: pushing frames onto
    // them is an error (surfaced by the media wrappers).
    is_remote: bool,
    // Per-track frame-metadata switch: off tracks never get a trailer writer
    // installed and drop `user_data` on pushes (the frame itself still flows).
    // Defaults from the factory kill switch; overridable per track via
    // `VideoTrackOptions::frame_metadata`.
    metadata_enabled: bool,
    // The factory's encoder registry for local tracks — dropped slots must be
    // retracted when the track dies ([`EncoderRegistry::retract`]). None for
    // remote tracks: their slots never existed.
    registry: Option<Arc<crate::encoded::EncoderRegistry>>,
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
        is_remote: bool,
        metadata_enabled: bool,
    ) -> Self {
        Self::from_raw_with_registry(raw, kind, factory, is_remote, metadata_enabled, None)
    }

    pub(crate) fn from_raw_with_registry(
        raw: *mut reactor_webrtc_sys::MediaStreamTrack,
        kind: MediaKind,
        factory: Arc<FactoryHandle>,
        is_remote: bool,
        metadata_enabled: bool,
        registry: Option<Arc<crate::encoded::EncoderRegistry>>,
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
            crate::sender_meta::register_allowed(native_id, metadata_enabled);
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
            is_remote,
            metadata_enabled,
            registry,
        }
    }

    /// Whether this track came from the remote side of a peer connection.
    /// Remote tracks only receive — pushing frames onto one is an error.
    pub fn is_remote(&self) -> bool {
        self.is_remote
    }

    /// Whether this track carries a frame-metadata trailer (`user_data`
    /// survives the push and reaches the receiver).
    pub(crate) fn metadata_enabled(&self) -> bool {
        self.metadata_enabled
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

    /// Push a BGRA frame captured *now* — the untimestamped native path.
    /// Called by [`VideoTrack::push_frame`]. No-op for non-video tracks.
    pub(crate) fn push_video_frame_now(&self, bgra: &[u8], width: u32, height: u32) {
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

    /// Shared body of [`VideoTrack::push_frame_with_metadata` and `_at`]:
    /// `capture_time_us` is the caller's declared time, or `None` for the clock.
    ///
    /// `capture_time_us` does two jobs, and they want different things from it.
    /// The trailer carries it **exactly**, so the receiver reads the number the
    /// caller put on the frame — that is the caller's own timeline, and rounding
    /// it would be this library editing a value it does not own. Playout
    /// synchronisation gets it too, so a frame the model annotated still lines up
    /// with the audio produced alongside it: the trailer must not cost the caller
    /// its timestamp.
    ///
    /// Synchronisation is where the rounding lives. The trailer is matched to its
    /// frame by capture millisecond, so two frames stamped inside the same
    /// millisecond would collide; the second is nudged to the following one —
    /// far below the resolution any synchronisation cares about, and only
    /// reachable above 1000 fps. Neither the truncation nor the nudge reaches the
    /// trailer, so the value delivered is the value passed, and several tracks
    /// pushed with one stamp deliver that one stamp.
    pub(crate) fn push_metadata_frame(
        &self,
        bgra: &[u8],
        width: u32,
        height: u32,
        user_data: &[u8],
        capture_time_us: Option<i64>,
    ) {
        if self.kind != MediaKind::Video {
            return;
        }
        // One reading, two uses. The trailer carries it exactly as it is, because
        // that is the caller's own timeline and rounding it would be this library
        // editing a number it does not own. The join key and the capture
        // timestamp libwebrtc gets are derived from the same value, and the
        // millisecond truncation and uniqueness nudge they need stop there.
        let declared = capture_time_us.unwrap_or_else(crate::time_micros);
        let capture_us = self.alloc_send_capture_us(Some(declared));
        let meta = crate::metadata::FrameMetadata {
            frame_id: self.next_frame_id(),
            // The wire field is unsigned and 0 is its "unset", so a negative
            // reading has nowhere to land and says nothing instead of a wrapped
            // number that would read as 584 thousand years from now.
            capture_time_us: declared.max(0) as u64,
            user_data: user_data.to_vec(),
        };
        self.insert_sender_meta(capture_us / 1000, meta);
        self.push_video_frame_ts(bgra, width, height, capture_us);
    }

    /// Return a strictly-increasing capture timestamp in microseconds for use
    /// as the sender-map key and `push_video_frame_ts` argument.
    ///
    /// `requested` is the caller's own capture time, or `None` to read the
    /// clock. Either way two frames in the same millisecond would produce the
    /// same `capture_ms` key and share one metadata entry, so the counter
    /// advances by 1 ms when the source hasn't moved, guaranteeing unique keys
    /// regardless of call frequency.
    pub(crate) fn alloc_send_capture_us(&self, requested: Option<i64>) -> i64 {
        let raw_ms = match requested {
            Some(us) => us / 1000,
            None => crate::time_micros() / 1000,
        };
        next_unique_capture_ms(&self.last_send_ms, raw_ms) * 1000
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

    /// Register a decoded-frame sink, replacing any previous one. Called by
    /// [`VideoTrack::on_frame`].
    ///
    /// [`VideoFrame::metadata`] is populated whenever the sender included a
    /// metadata trailer and the peer connection installed the strip transform —
    /// which it does automatically, on both peers, once the capability has been
    /// negotiated. Nothing here has to be arranged for it.
    pub(crate) fn attach_video_sink(
        &self,
        cb: impl for<'a> FnMut(VideoFrame<'a>) + Send + 'static,
    ) {
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

    /// Shared body of [`AudioTrack::push_frame`] / [`push_frame_at`]:
    /// `capture_time_ms = 0` means "unknown / stamp at arrival".
    pub(crate) fn push_pcm_inner(
        &self,
        pcm: &[i16],
        sample_rate: u32,
        channels: u32,
        capture_time_ms: i64,
    ) -> Result<()> {
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
            reactor_webrtc_sys::reactor_webrtc_audio_track_push_pcm_ts(
                self.raw,
                pcm.as_ptr(),
                samples_per_channel,
                sample_rate as c_int,
                channels,
                capture_time_ms,
            );
        }
        Ok(())
    }

    /// Register a decoded-PCM sink, replacing any previous one. Called by
    /// [`AudioTrack::on_frame`].
    pub(crate) fn attach_audio_sink(
        &self,
        cb: impl for<'a> FnMut(AudioFrame<'a>) + Send + 'static,
    ) {
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
            crate::sender_meta::deregister_allowed(self.native_id);
            crate::encoded::deregister_feedback_binding(self.native_id);
        }
        // Video-only: this track may have reserved encoder slots — reclaim
        // them so nothing else can be routed through the dead lane.
        if let Some(registry) = &self.registry {
            if self.kind == MediaKind::Video {
                registry.retract(self.native_id());
            }
        }
        // Detaches the C++ sink before the sink-state boxes are freed.
        unsafe { reactor_webrtc_sys::reactor_webrtc_media_stream_track_destroy(self.raw) }
    }
}

// ── Media-typed wrappers ─────────────────────────────────────────────────────
//
// `Track` is the untyped core: the native handle, lifetime, metadata plumbing
// and the transceiver/FFI surface. The public frame API lives on these
// wrappers instead — pushing a video frame onto an audio track is a compile
// error, not a runtime no-op. Everything derefs to `&Track`, so
// `transceiver.set_track(&wrapper)` just works.

use std::ops::Deref;

/// A video [`Track`] — local (raw) or remote. `on_frame` receives decoded
/// video; `push_frame` family feeds a local track (`Err` on a remote track).
pub struct VideoTrack(Track);

/// An audio [`Track`] — local or remote. `on_frame` receives decoded PCM;
/// `push_frame` family feeds a local [`AudioTrackSource::LocalPush`] track
/// (`Err` on a remote track; no-op semantics on ADM-backed locals match the
/// native ADM).
pub struct AudioTrack(Track);

impl VideoTrack {
    pub(crate) fn wrap(track: Track) -> Self {
        debug_assert_eq!(track.kind(), MediaKind::Video);
        Self(track)
    }

    /// Subscribe to decoded frames. Replaces any previous sink. The closure
    /// runs on a WebRTC thread.
    pub fn on_frame(&self, cb: impl for<'a> FnMut(VideoFrame<'a>) + Send + 'static) {
        self.0.attach_video_sink(cb);
    }

    /// Push a BGRA frame (`width*height*4` bytes), captured now — on a local
    /// track only (a remote track returns `Err`). What a
    /// [`TrackVideoEncoder::Inline`](crate::TrackVideoEncoder) callback sees on
    /// its slot comes straight from these pushes.
    pub fn push_frame(&self, frame: VideoFrame<'_>) -> Result<()> {
        self.guard_remote()?;
        self.0
            .push_video_frame_now(frame.bgra, frame.width, frame.height);
        Ok(())
    }

    /// Push a BGRA frame captured at `capture_time_us` ([`time_micros`]'s
    /// epoch) rather than whenever it reaches the encoder.
    ///
    /// A track's RTP timestamp otherwise counts only the samples handed to it,
    /// which says how much media exists but not when it was captured.
    /// Supplying the same timestamp used for the audio/video produced
    /// alongside it is what lets the receiver play them together.
    pub fn push_frame_at(&self, frame: VideoFrame<'_>, capture_time_us: i64) -> Result<()> {
        self.guard_remote()?;
        self.0
            .push_video_frame_ts(frame.bgra, frame.width, frame.height, capture_time_us);
        Ok(())
    }

    /// Push a BGRA frame with arbitrary `user_data` embedded as a protobuf
    /// trailer, captured now. `frame_id` (monotonic) and `capture_time_us`
    /// are filled in internally.
    pub fn push_frame_with_metadata(&self, frame: VideoFrame<'_>, user_data: &[u8]) -> Result<()> {
        self.guard_remote()?;
        if self.0.metadata_enabled() {
            self.0
                .push_metadata_frame(frame.bgra, frame.width, frame.height, user_data, None);
        } else {
            // user_data dropped by contract; the frame itself still flows.
            self.0
                .push_video_frame_now(frame.bgra, frame.width, frame.height);
        }
        Ok(())
    }

    /// Same as [`push_frame_with_metadata`](Self::push_frame_with_metadata)
    /// with the capture time declared by the caller. The trailer carries the
    /// value **exactly**; the millisecond-uniqueness nudge lives elsewhere and
    /// never reaches the wire value.
    pub fn push_frame_with_metadata_at(
        &self,
        frame: VideoFrame<'_>,
        user_data: &[u8],
        capture_time_us: i64,
    ) -> Result<()> {
        self.guard_remote()?;
        if self.0.metadata_enabled() {
            self.0.push_metadata_frame(
                frame.bgra,
                frame.width,
                frame.height,
                user_data,
                Some(capture_time_us),
            );
        } else {
            // user_data dropped by contract; the frame still flows, stamped.
            self.0
                .push_video_frame_ts(frame.bgra, frame.width, frame.height, capture_time_us);
        }
        Ok(())
    }

    fn guard_remote(&self) -> Result<()> {
        if self.0.is_remote() {
            return Err(Error::Webrtc(
                "cannot push frames onto a remote track".to_owned(),
            ));
        }
        Ok(())
    }

    /// Listen for encoder feedback
    /// ([`EncoderFeedback::RateUpdate`](crate::EncoderFeedback::RateUpdate)) on
    /// a track created with [`TrackVideoEncoder::Inline`]. Inline tracks see
    /// keyframe demands inside the [`RawVideoFrame`] their callback receives
    /// (`request_key_frame`); the other feedback — rate control from the
    /// congestion controller — surfaces here. Latest registration wins.
    ///
    /// Only meaningful on inline-encoder tracks: returns an error on raw
    /// (builtin-encoder) tracks, where BWE adaptation is internal to
    /// libwebrtc, and on remote tracks.
    pub fn on_encoder_feedback(
        &self,
        cb: impl FnMut(crate::encoded::EncoderFeedback) + Send + 'static,
    ) -> Result<()> {
        self.guard_remote()?;
        let Some(listeners) = crate::encoded::feedback_binding(self.0.native_id()) else {
            return Err(Error::Webrtc(
                "on_encoder_feedback is only valid on tracks created with                  TrackVideoEncoder::Inline — builtin-encoder tracks adapt internally"
                    .to_owned(),
            ));
        };
        listeners.set(Box::new(cb));
        Ok(())
    }
}

impl AudioTrack {
    pub(crate) fn wrap(track: Track) -> Self {
        debug_assert_eq!(track.kind(), MediaKind::Audio);
        Self(track)
    }

    /// Subscribe to decoded PCM. Replaces any previous sink. The closure runs
    /// on a WebRTC thread.
    pub fn on_frame(&self, cb: impl for<'a> FnMut(AudioFrame<'a>) + Send + 'static) {
        self.0.attach_audio_sink(cb);
    }

    /// Push interleaved i16 PCM, timestamped on arrival. Only meaningful on a
    /// local [`AudioTrackSource::LocalPush`] track — it's a no-op on
    /// ADM-backed locals and an `Err` on remote tracks. A remote→relay use
    /// case passes the received [`AudioFrame`] straight back in — same struct
    /// both ways.
    ///
    /// `pcm.len()` must be `frames * channels`; a partial trailing frame is an
    /// error. Only one thread should call this at a time for a given track —
    /// concurrent callers produce interleaved, garbage audio.
    pub fn push_frame(&self, frame: AudioFrame<'_>) -> Result<()> {
        self.guard_remote()?;
        self.0
            .push_pcm_inner(frame.pcm, frame.sample_rate, frame.channels, 0)
    }

    /// Push PCM captured at `capture_time_us` ([`time_micros`]'s epoch) rather
    /// than whenever it reaches the encoder — see
    /// [`VideoTrack::push_frame_at`] for why the shared stamp matters.
    pub fn push_frame_at(&self, frame: AudioFrame<'_>, capture_time_us: i64) -> Result<()> {
        self.guard_remote()?;
        self.0.push_pcm_inner(
            frame.pcm,
            frame.sample_rate,
            frame.channels,
            capture_time_us / 1000,
        )
    }

    fn guard_remote(&self) -> Result<()> {
        if self.0.is_remote() {
            return Err(Error::Webrtc(
                "cannot push frames onto a remote track".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Deref for VideoTrack {
    type Target = Track;
    fn deref(&self) -> &Track {
        &self.0
    }
}
impl Deref for AudioTrack {
    type Target = Track;
    fn deref(&self) -> &Track {
        &self.0
    }
}

// SAFETY: wraps a Track; its own fields add no state.
unsafe impl Send for VideoTrack {}
unsafe impl Sync for VideoTrack {}
unsafe impl Send for AudioTrack {}
unsafe impl Sync for AudioTrack {}

/// A track delivered by [`PeerConnectionObserver::on_track`] — media-typed,
/// so `RemoteTrack::Video(v).on_frame(...)` and `RemoteTrack::Audio(a)
/// .on_frame(...)` each take the matching frame type and nothing else.
pub enum RemoteTrack {
    /// A remote video track.
    Video(VideoTrack),
    /// A remote audio track.
    Audio(AudioTrack),
}

impl RemoteTrack {
    /// Audio vs video.
    pub fn kind(&self) -> MediaKind {
        match self {
            Self::Video(_) => MediaKind::Video,
            Self::Audio(_) => MediaKind::Audio,
        }
    }

    /// `Some` when this is a video track.
    pub fn as_video(&self) -> Option<&VideoTrack> {
        match self {
            Self::Video(t) => Some(t),
            Self::Audio(_) => None,
        }
    }

    /// `Some` when this is an audio track.
    pub fn as_audio(&self) -> Option<&AudioTrack> {
        match self {
            Self::Audio(t) => Some(t),
            Self::Video(_) => None,
        }
    }

    /// Unwrap a video track (panics on audio tracks).
    pub fn into_video(self) -> VideoTrack {
        match self {
            Self::Video(t) => t,
            Self::Audio(_) => panic!("into_video on an audio track"),
        }
    }

    /// Unwrap an audio track (panics on video tracks).
    pub fn into_audio(self) -> AudioTrack {
        match self {
            Self::Audio(t) => t,
            Self::Video(_) => panic!("into_audio on a video track"),
        }
    }
}

impl VideoFrame<'_> {
    /// A push-side frame: BGRA pixels, no metadata (outgoing trailers go
    /// through [`VideoTrack::push_frame_with_metadata(_at)`] with `user_data`).
    /// The same struct `on_frame` delivers — relay use cases repack nothing.
    /// `metadata` is populated only on receive.
    pub const fn new(bgra: &'_ [u8], width: u32, height: u32) -> VideoFrame<'_> {
        VideoFrame {
            bgra,
            width,
            height,
            metadata: None,
        }
    }
}

impl AudioFrame<'_> {
    /// A push-side frame: interleaved i16 PCM + format. `frames` is derived
    /// (`pcm.len() / channels`) — the push validates it against `pcm.len()`.
    /// The same struct `on_frame` delivers.
    pub fn new(pcm: &'_ [i16], sample_rate: u32, channels: u32) -> AudioFrame<'_> {
        AudioFrame {
            pcm,
            sample_rate,
            channels,
            frames: if channels == 0 {
                0
            } else {
                (pcm.len() / channels as usize) as u32
            },
        }
    }
}

/// Claim `raw_ms` as a capture millisecond, advancing past `last` when the
/// source has not moved.
///
/// The sender metadata map is keyed by capture millisecond, so two frames
/// claiming the same one would share a trailer. Nudging the second forward
/// keeps keys unique whether the millisecond came from the clock or from a
/// caller stamping its own capture time.
fn next_unique_capture_ms(last: &AtomicI64, raw_ms: i64) -> i64 {
    let mut prev = last.load(Ordering::Relaxed);
    loop {
        let next = raw_ms.max(prev + 1);
        match last.compare_exchange_weak(prev, next, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(actual) => prev = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caller_timestamp_is_used_as_given() {
        let last = AtomicI64::new(0);
        assert_eq!(next_unique_capture_ms(&last, 1_700), 1_700);
    }

    #[test]
    fn a_second_frame_in_the_same_millisecond_is_nudged_forward() {
        let last = AtomicI64::new(0);
        assert_eq!(next_unique_capture_ms(&last, 1_700), 1_700);
        assert_eq!(next_unique_capture_ms(&last, 1_700), 1_701);
        assert_eq!(next_unique_capture_ms(&last, 1_700), 1_702);
    }

    #[test]
    fn a_timestamp_past_the_nudge_wins() {
        let last = AtomicI64::new(0);
        next_unique_capture_ms(&last, 1_700);
        next_unique_capture_ms(&last, 1_700);
        // 40 ms later — a real frame interval — lands where the caller asked.
        assert_eq!(next_unique_capture_ms(&last, 1_740), 1_740);
    }

    #[test]
    fn keys_stay_unique_across_threads() {
        let last = Arc::new(AtomicI64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let last = Arc::clone(&last);
            let seen = Arc::clone(&seen);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    let ms = next_unique_capture_ms(&last, 5_000);
                    seen.lock().unwrap().push(ms);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut all = seen.lock().unwrap().clone();
        all.sort_unstable();
        let before = all.len();
        all.dedup();
        assert_eq!(all.len(), before, "capture milliseconds collided");
    }
}
