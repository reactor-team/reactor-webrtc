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
use std::sync::Mutex;

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

// Heap-pinned sink state behind the C userdata pointer.
struct VideoSinkState {
    cb: Mutex<VideoSinkCb>,
}
struct AudioSinkState {
    cb: Mutex<AudioSinkCb>,
}

extern "C" fn video_sink_tramp(ud: *mut c_void, bgra: *const u8, width: c_int, height: c_int) {
    let st = unsafe { &*(ud as *const VideoSinkState) };
    let len = (width as usize) * (height as usize) * 4;
    let slice = unsafe { std::slice::from_raw_parts(bgra, len) };
    if let Ok(mut cb) = st.cb.lock() {
        cb(VideoFrame {
            bgra: slice,
            width: width as u32,
            height: height as u32,
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

    /// Subscribe to decoded frames from a (remote) video track. Replaces any
    /// previous sink. The closure runs on a WebRTC thread.
    pub fn on_video_frame(&mut self, cb: impl for<'a> FnMut(VideoFrame<'a>) + Send + 'static) {
        let state = Box::new(VideoSinkState {
            cb: Mutex::new(Box::new(cb)),
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
