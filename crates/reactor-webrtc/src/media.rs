//! Media tracks and raw frames (mirrors the PoC's video/audio source + frame
//! types). Decoded remote frames are surfaced as owned buffers; sendonly frames
//! are pushed via [`crate::PeerConnection::push_video_frame`] /
//! [`push_audio_frame`](crate::PeerConnection::push_audio_frame).

/// Audio vs. video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
}

/// A decoded video frame, canonicalized to BGRA (`width*height*4`, B-G-R-A).
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A chunk of decoded audio: interleaved signed 16-bit PCM.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub data: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u32,
}

/// Handle to a sendonly/recvonly video track.
pub struct VideoTrack {
    // TODO(M1): read once on_frame()/track ops are implemented.
    #[allow(dead_code)]
    pub(crate) raw: *mut reactor_webrtc_sys::MediaStreamTrack,
    pub(crate) name: String,
}

impl VideoTrack {
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Subscribe to decoded frames from a recvonly track (BGRA).
    pub fn on_frame(&self, _cb: impl FnMut(VideoFrame) + Send + 'static) {
        unimplemented!("M1: VideoTrack::on_frame (native video sink)")
    }
}

/// Handle to a sendonly/recvonly audio track.
pub struct AudioTrack {
    // TODO(M1): read once on_frame()/track ops are implemented.
    #[allow(dead_code)]
    pub(crate) raw: *mut reactor_webrtc_sys::MediaStreamTrack,
    pub(crate) name: String,
}

impl AudioTrack {
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Subscribe to decoded PCM from a recvonly track.
    pub fn on_frame(&self, _cb: impl FnMut(AudioFrame) + Send + 'static) {
        unimplemented!("M1: AudioTrack::on_frame (native audio sink)")
    }
}
