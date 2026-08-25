//! PeerConnection observer: ergonomic Rust closures bridged to the sys
//! callback (function-pointer + userdata) ABI.
//!
//! Callbacks fire on WebRTC's signaling thread (serialized); each closure is
//! guarded by a `Mutex` so the bridge can call it as `FnMut` safely.

use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, Mutex};

use crate::media::{AudioTrack, MediaKind, RemoteTrack, Track, VideoTrack};
use crate::peer_connection::{DataChannel, IceCandidate, IceGatheringState, PeerConnectionState};
use crate::FactoryHandle;

type StateCb = Box<dyn FnMut(PeerConnectionState) + Send>;
type GatheringCb = Box<dyn FnMut(IceGatheringState) + Send>;
type IceCb = Box<dyn FnMut(IceCandidate) + Send>;
type TrackCb = Box<dyn FnMut(RemoteTrack) + Send>;
type DataChannelCb = Box<dyn FnMut(DataChannel) + Send>;

/// Builder for the closures invoked over a peer connection's lifetime. Unset
/// callbacks are simply not delivered.
#[derive(Default)]
pub struct PeerConnectionObserver {
    on_connection_state_change: Option<StateCb>,
    on_ice_gathering_change: Option<GatheringCb>,
    on_ice_candidate: Option<IceCb>,
    on_track: Option<TrackCb>,
    on_data_channel: Option<DataChannelCb>,
}

impl PeerConnectionObserver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_connection_state_change(
        mut self,
        cb: impl FnMut(PeerConnectionState) + Send + 'static,
    ) -> Self {
        self.on_connection_state_change = Some(Box::new(cb));
        self
    }
    pub fn on_ice_gathering_change(
        mut self,
        cb: impl FnMut(IceGatheringState) + Send + 'static,
    ) -> Self {
        self.on_ice_gathering_change = Some(Box::new(cb));
        self
    }
    pub fn on_ice_candidate(mut self, cb: impl FnMut(IceCandidate) + Send + 'static) -> Self {
        self.on_ice_candidate = Some(Box::new(cb));
        self
    }
    pub fn on_track(mut self, cb: impl FnMut(RemoteTrack) + Send + 'static) -> Self {
        self.on_track = Some(Box::new(cb));
        self
    }
    pub fn on_data_channel(mut self, cb: impl FnMut(DataChannel) + Send + 'static) -> Self {
        self.on_data_channel = Some(Box::new(cb));
        self
    }

    pub(crate) fn into_state(self, factory: Arc<FactoryHandle>) -> Box<ObserverState> {
        Box::new(ObserverState {
            conn: self.on_connection_state_change.map(Mutex::new),
            gathering: self.on_ice_gathering_change.map(Mutex::new),
            ice: self.on_ice_candidate.map(Mutex::new),
            track: self.on_track.map(Mutex::new),
            data_channel: self.on_data_channel.map(Mutex::new),
            factory,
        })
    }
}

/// Heap-pinned closure state addressed by the sys `userdata` pointer. Held alive
/// by the owning [`crate::PeerConnection`].
pub(crate) struct ObserverState {
    conn: Option<Mutex<StateCb>>,
    gathering: Option<Mutex<GatheringCb>>,
    ice: Option<Mutex<IceCb>>,
    track: Option<Mutex<TrackCb>>,
    data_channel: Option<Mutex<DataChannelCb>>,
    // Cloned into every remote `Track` the `on_track` trampoline hands off, so
    // a caller that detaches a remote track and drops the peer connection
    // still keeps the factory's threads alive for as long as that track does.
    factory: Arc<FactoryHandle>,
}

impl ObserverState {
    /// Build the sys callbacks struct, wiring only the trampolines for the
    /// closures that were set. `self` must outlive the returned struct's use.
    pub(crate) fn callbacks(&self) -> reactor_webrtc_sys::PeerConnectionCallbacks {
        reactor_webrtc_sys::PeerConnectionCallbacks {
            userdata: self as *const ObserverState as *mut c_void,
            on_signaling_change: None,
            on_connection_change: self.conn.as_ref().map(|_| tramp_conn as _),
            on_ice_gathering_change: self.gathering.as_ref().map(|_| tramp_gathering as _),
            on_ice_candidate: self.ice.as_ref().map(|_| tramp_ice as _),
            on_data_channel: self.data_channel.as_ref().map(|_| tramp_data_channel as _),
            on_renegotiation_needed: None,
            on_track: self.track.as_ref().map(|_| tramp_track as _),
        }
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

extern "C" fn tramp_conn(ud: *mut c_void, state: c_int) {
    let st = unsafe { &*(ud as *const ObserverState) };
    if let Some(m) = &st.conn {
        if let Ok(mut cb) = m.lock() {
            cb(PeerConnectionState::from_raw(state));
        }
    }
}

extern "C" fn tramp_gathering(ud: *mut c_void, state: c_int) {
    let st = unsafe { &*(ud as *const ObserverState) };
    if let Some(m) = &st.gathering {
        if let Ok(mut cb) = m.lock() {
            cb(IceGatheringState::from_raw(state));
        }
    }
}

extern "C" fn tramp_ice(
    ud: *mut c_void,
    sdp_mid: *const c_char,
    sdp_mline_index: c_int,
    candidate: *const c_char,
) {
    let st = unsafe { &*(ud as *const ObserverState) };
    if let Some(m) = &st.ice {
        if let Ok(mut cb) = m.lock() {
            cb(IceCandidate {
                candidate: cstr(candidate),
                sdp_mid: Some(cstr(sdp_mid)),
                sdp_mline_index: Some(sdp_mline_index as u16),
            });
        }
    }
}

extern "C" fn tramp_track(ud: *mut c_void, track: *mut reactor_webrtc_sys::MediaStreamTrack) {
    let st = unsafe { &*(ud as *const ObserverState) };
    let kind = MediaKind::from_raw(unsafe {
        reactor_webrtc_sys::reactor_webrtc_media_stream_track_kind(track)
    });
    // Take ownership of the handle; if there's no handler, dropping it frees it.
    let t = Track::from_raw(track, kind, Arc::clone(&st.factory), true, true);
    let remote = match kind {
        MediaKind::Video => RemoteTrack::Video(VideoTrack::wrap(t)),
        _ => RemoteTrack::Audio(AudioTrack::wrap(t)),
    };
    if let Some(m) = &st.track {
        if let Ok(mut cb) = m.lock() {
            cb(remote);
        }
    }
}

extern "C" fn tramp_data_channel(ud: *mut c_void, dc: *mut reactor_webrtc_sys::DataChannel) {
    let st = unsafe { &*(ud as *const ObserverState) };
    let channel = DataChannel::from_raw(dc, Arc::clone(&st.factory));
    if let Some(m) = &st.data_channel {
        if let Ok(mut cb) = m.lock() {
            cb(channel);
        }
    }
}
