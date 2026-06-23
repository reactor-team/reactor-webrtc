//! The peer connection and its associated signaling/data types.
//!
//! Mirrors the slice of LiveKit's `libwebrtc` API the PoC used in
//! `reactor-sdk-core`'s peer transport.

use crate::media::{AudioTrack, MediaKind, VideoTrack};
use crate::Result;

/// SDP description kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpType {
    Offer,
    PrAnswer,
    Answer,
    Rollback,
}

/// A session description (offer/answer).
#[derive(Debug, Clone)]
pub struct SessionDescription {
    pub kind: SdpType,
    pub sdp: String,
}

/// A trickled ICE candidate.
#[derive(Debug, Clone)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

/// Direction of a transceiver / track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransceiverDirection {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

/// Aggregate connection state (maps to the PoC's `PeerConnectionState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

/// A negotiated data channel.
pub struct DataChannel {
    // TODO(M1): read once send()/on_message() are implemented.
    #[allow(dead_code)]
    pub(crate) raw: *mut reactor_webrtc_sys::DataChannel,
}

impl DataChannel {
    /// Send a text/binary message over the channel.
    pub fn send(&self, _data: &[u8], _binary: bool) -> Result<()> {
        unimplemented!("M1: DataChannel::send")
    }

    /// Register a message handler (invoked on a WebRTC thread).
    pub fn on_message(&self, _cb: impl FnMut(&[u8], bool) + Send + 'static) {
        unimplemented!("M1: DataChannel::on_message")
    }
}

/// An `RTCPeerConnection`.
pub struct PeerConnection {
    pub(crate) raw: *mut reactor_webrtc_sys::PeerConnection,
}

// SAFETY (TODO M1): confirm/​document thread-safety once the native layer is wired.
unsafe impl Send for PeerConnection {}
unsafe impl Sync for PeerConnection {}

impl PeerConnection {
    // ── Signaling ────────────────────────────────────────────────────────────
    pub async fn create_offer(&self) -> Result<SessionDescription> {
        unimplemented!("M1: PeerConnection::create_offer")
    }
    pub async fn set_local_description(&self, _sdp: SessionDescription) -> Result<()> {
        unimplemented!("M1: PeerConnection::set_local_description")
    }
    pub async fn set_remote_description(&self, _sdp: SessionDescription) -> Result<()> {
        unimplemented!("M1: PeerConnection::set_remote_description")
    }
    pub fn add_ice_candidate(&self, _candidate: IceCandidate) -> Result<()> {
        unimplemented!("M1: PeerConnection::add_ice_candidate")
    }

    // ── Tracks / transceivers ────────────────────────────────────────────────
    pub fn add_video_track(&self, _name: &str, _dir: TransceiverDirection) -> Result<VideoTrack> {
        unimplemented!("M1: PeerConnection::add_video_track")
    }
    pub fn add_audio_track(&self, _name: &str, _dir: TransceiverDirection) -> Result<AudioTrack> {
        unimplemented!("M1: PeerConnection::add_audio_track")
    }

    // ── Frame injection (sendonly) ───────────────────────────────────────────
    /// Push a BGRA frame (`width*height*4`) into a sendonly video track.
    pub fn push_video_frame(&self, track: &str, bgra: &[u8], width: u32, height: u32) {
        let cname = std::ffi::CString::new(track).unwrap_or_default();
        let ptr = if bgra.is_empty() {
            std::ptr::null()
        } else {
            bgra.as_ptr()
        };
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_push_video_frame(
                self.raw,
                cname.as_ptr(),
                ptr,
                width,
                height,
            )
        }
    }
    /// Push interleaved i16 PCM into a sendonly audio track.
    pub fn push_audio_frame(&self, track: &str, pcm: &[i16], sample_rate: u32, channels: u32) {
        let cname = std::ffi::CString::new(track).unwrap_or_default();
        let ptr = if pcm.is_empty() {
            std::ptr::null()
        } else {
            pcm.as_ptr()
        };
        let spc = (pcm.len() as u32).checked_div(channels).unwrap_or(0);
        unsafe {
            reactor_webrtc_sys::reactor_webrtc_push_audio_frame(
                self.raw,
                cname.as_ptr(),
                ptr,
                spc,
                sample_rate,
                channels,
            )
        }
    }

    // ── Callbacks ────────────────────────────────────────────────────────────
    pub fn on_connection_state_change(
        &self,
        _cb: impl FnMut(PeerConnectionState) + Send + 'static,
    ) {
        unimplemented!("M1: PeerConnection::on_connection_state_change")
    }
    pub fn on_ice_candidate(&self, _cb: impl FnMut(IceCandidate) + Send + 'static) {
        unimplemented!("M1: PeerConnection::on_ice_candidate")
    }
    /// Fired when a remote track arrives; `MediaKind` distinguishes audio/video.
    pub fn on_track(&self, _cb: impl FnMut(MediaKind, String) + Send + 'static) {
        unimplemented!("M1: PeerConnection::on_track")
    }
    pub fn on_data_channel(&self, _cb: impl FnMut(DataChannel) + Send + 'static) {
        unimplemented!("M1: PeerConnection::on_data_channel")
    }

    pub fn close(&self) {
        unimplemented!("M1: PeerConnection::close")
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        unsafe { reactor_webrtc_sys::reactor_webrtc_peer_connection_destroy(self.raw) }
    }
}
