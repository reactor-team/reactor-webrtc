//! Media tracks and decoded frames.
//!
//! A [`Track`] wraps a native media track (local or remote, audio or video).
//! Local tracks created via [`crate::PeerConnectionFactory::create_video_track`]
//! / [`create_audio_track`](crate::PeerConnectionFactory::create_audio_track)
//! can push frames; remote tracks delivered to
//! [`PeerConnectionObserver::on_track`](crate::PeerConnectionObserver) can have
//! a frame sink attached. Audio send is via the factory ADM
//! ([`crate::PeerConnectionFactory::push_audio_frame`]).

use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::{Arc, Mutex};

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
type MetadataSlot = Arc<Mutex<Option<crate::metadata::FrameMetadata>>>;

// Heap-pinned sink state behind the C userdata pointer.
struct VideoSinkState {
    cb: Mutex<VideoSinkCb>,
    // Populated by receiver_metadata_transform(); None when metadata is not in use.
    receiver_meta: Option<MetadataSlot>,
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
        .and_then(|slot| slot.lock().ok()?.take());
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
    // Set by sender_metadata_transform(); read by push_video_frame_with_metadata.
    sender_meta: Option<MetadataSlot>,
    // Set by receiver_metadata_transform(); shared with the VideoSinkState trampoline.
    receiver_meta: Option<MetadataSlot>,
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

    /// Push a BGRA frame with attached metadata. The metadata is encoded as a
    /// protobuf trailer and appended to the encoded frame by the
    /// [`FrameTransform`](crate::FrameTransform) returned from
    /// [`sender_metadata_transform`](Self::sender_metadata_transform).
    ///
    /// Call `sender_metadata_transform` and attach it to the sender transceiver
    /// before pushing frames with metadata; otherwise this is a no-op for the
    /// metadata (the frame still goes out, just without a trailer).
    pub fn push_video_frame_with_metadata(
        &self,
        bgra: &[u8],
        width: u32,
        height: u32,
        meta: crate::metadata::FrameMetadata,
    ) {
        if let Some(slot) = &self.sender_meta {
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(meta);
            }
        }
        self.push_video_frame(bgra, width, height);
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
        let slot: MetadataSlot = Arc::new(Mutex::new(None));
        self.sender_meta = Some(slot.clone());
        crate::encoded::FrameTransform::new(move |frame| {
            if frame.direction != crate::encoded::FrameDirection::Send {
                return crate::encoded::FrameAction::Forward;
            }
            let meta = slot.lock().ok().and_then(|mut g| g.take());
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
    /// protobuf metadata trailer from received encoded frames and makes the
    /// metadata available in subsequent [`on_video_frame`](Self::on_video_frame)
    /// callbacks via [`VideoFrame::metadata`].
    ///
    /// Attach this to the receiver transceiver with
    /// `Transceiver::set_receiver_transform` **before** the first SDP exchange.
    /// Then call `on_video_frame` on this track; each callback will carry the
    /// metadata decoded from the corresponding encoded frame (or `None` when no
    /// trailer was present).
    pub fn receiver_metadata_transform(&mut self) -> crate::encoded::FrameTransform {
        let slot: MetadataSlot = Arc::new(Mutex::new(None));
        self.receiver_meta = Some(slot.clone());
        crate::encoded::FrameTransform::new(move |frame| {
            if frame.direction != crate::encoded::FrameDirection::Receive {
                return crate::encoded::FrameAction::Forward;
            }
            if let Some((meta, stripped)) = crate::metadata::decode_and_strip_trailer(frame.data) {
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(meta);
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
