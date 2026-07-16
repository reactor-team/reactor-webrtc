//! PyO3 bindings for `reactor-webrtc`.
//!
//! Imported from Python as `import reactor_webrtc`. All blocking operations
//! release the GIL (`py.allow_threads`) so callbacks that re-acquire it can
//! fire without deadlocking.
//!
//! Build with Maturin:
//!
//! ```sh
//! cd crates/reactor-webrtc-py
//! REACTOR_WEBRTC_PREBUILT_URL=... maturin develop
//! ```

// `#[pymethods]` generates identity `into()` calls on `PyErr` that clippy
// incorrectly flags as useless conversions — they are a macro artifact.
#![allow(clippy::useless_conversion)]

// Alias the external crate to avoid name collision with the #[pymodule]
// function named `reactor_webrtc` that this file also defines.
use ::reactor_webrtc as rw;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

fn err(e: rw::Error) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

fn sdp_type_to_str(kind: rw::SdpType) -> &'static str {
    match kind {
        rw::SdpType::Offer => "offer",
        rw::SdpType::PrAnswer => "pranswer",
        rw::SdpType::Answer => "answer",
        rw::SdpType::Rollback => "rollback",
    }
}

fn parse_sdp_type(s: &str) -> Option<rw::SdpType> {
    match s {
        "offer" => Some(rw::SdpType::Offer),
        "pranswer" => Some(rw::SdpType::PrAnswer),
        "answer" => Some(rw::SdpType::Answer),
        "rollback" => Some(rw::SdpType::Rollback),
        _ => None,
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

/// A single STUN/TURN server entry.
#[pyclass(get_all, set_all)]
#[derive(Clone, Default)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub password: String,
}

#[pymethods]
impl IceServer {
    #[new]
    #[pyo3(signature = (urls=vec![], username=String::new(), password=String::new()))]
    fn new(urls: Vec<String>, username: String, password: String) -> Self {
        Self {
            urls,
            username,
            password,
        }
    }
    fn __repr__(&self) -> String {
        format!("IceServer(urls={:?})", self.urls)
    }
}

impl From<&IceServer> for rw::IceServer {
    fn from(s: &IceServer) -> Self {
        rw::IceServer {
            urls: s.urls.clone(),
            username: s.username.clone(),
            password: s.password.clone(),
        }
    }
}

/// Peer-connection ICE + transport configuration.
#[pyclass]
#[derive(Clone)]
pub struct RtcConfiguration {
    pub ice_servers: Vec<IceServer>,
}

#[pymethods]
impl RtcConfiguration {
    #[new]
    #[pyo3(signature = (ice_servers=vec![]))]
    fn new(ice_servers: Vec<IceServer>) -> Self {
        Self { ice_servers }
    }
    #[getter]
    fn ice_servers(&self) -> Vec<IceServer> {
        self.ice_servers.clone()
    }
    #[setter]
    fn set_ice_servers(&mut self, servers: Vec<IceServer>) {
        self.ice_servers = servers;
    }
}

impl From<&RtcConfiguration> for rw::RtcConfiguration {
    fn from(c: &RtcConfiguration) -> Self {
        rw::RtcConfiguration {
            ice_servers: c.ice_servers.iter().map(Into::into).collect(),
            ..Default::default()
        }
    }
}

// ── Signaling ─────────────────────────────────────────────────────────────────

/// A trickled ICE candidate received from the remote peer.
#[pyclass(get_all)]
#[derive(Clone)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

#[pymethods]
impl IceCandidate {
    #[new]
    #[pyo3(signature = (candidate, sdp_mid=None, sdp_mline_index=None))]
    fn new(candidate: String, sdp_mid: Option<String>, sdp_mline_index: Option<u16>) -> Self {
        Self {
            candidate,
            sdp_mid,
            sdp_mline_index,
        }
    }
    fn __repr__(&self) -> String {
        format!("IceCandidate(mid={:?})", self.sdp_mid)
    }
}

impl From<rw::IceCandidate> for IceCandidate {
    fn from(c: rw::IceCandidate) -> Self {
        Self {
            candidate: c.candidate,
            sdp_mid: c.sdp_mid,
            sdp_mline_index: c.sdp_mline_index,
        }
    }
}
impl From<&IceCandidate> for rw::IceCandidate {
    fn from(c: &IceCandidate) -> Self {
        rw::IceCandidate {
            candidate: c.candidate.clone(),
            sdp_mid: c.sdp_mid.clone(),
            sdp_mline_index: c.sdp_mline_index,
        }
    }
}

/// An SDP offer or answer.
#[pyclass(get_all)]
#[derive(Clone)]
pub struct SessionDescription {
    /// `"offer"`, `"answer"`, `"pranswer"`, or `"rollback"`.
    pub kind: String,
    pub sdp: String,
}

#[pymethods]
impl SessionDescription {
    #[new]
    fn new(kind: String, sdp: String) -> Self {
        Self { kind, sdp }
    }
    fn __repr__(&self) -> String {
        format!("SessionDescription(kind={:?})", self.kind)
    }
}

impl From<rw::SessionDescription> for SessionDescription {
    fn from(s: rw::SessionDescription) -> Self {
        Self {
            kind: sdp_type_to_str(s.kind).to_owned(),
            sdp: s.sdp,
        }
    }
}
fn to_rust_sdp(s: &SessionDescription) -> PyResult<rw::SessionDescription> {
    let kind = parse_sdp_type(&s.kind)
        .ok_or_else(|| PyRuntimeError::new_err(format!("unknown SDP kind: {}", s.kind)))?;
    Ok(rw::SessionDescription {
        kind,
        sdp: s.sdp.clone(),
    })
}

// ── Enums ─────────────────────────────────────────────────────────────────────

/// Overall peer connection state.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PeerConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

impl From<rw::PeerConnectionState> for PeerConnectionState {
    fn from(s: rw::PeerConnectionState) -> Self {
        match s {
            rw::PeerConnectionState::New => Self::New,
            rw::PeerConnectionState::Connecting => Self::Connecting,
            rw::PeerConnectionState::Connected => Self::Connected,
            rw::PeerConnectionState::Disconnected => Self::Disconnected,
            rw::PeerConnectionState::Failed => Self::Failed,
            rw::PeerConnectionState::Closed => Self::Closed,
        }
    }
}

/// ICE gathering phase.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IceGatheringState {
    New,
    Gathering,
    Complete,
}

impl From<rw::IceGatheringState> for IceGatheringState {
    fn from(s: rw::IceGatheringState) -> Self {
        match s {
            rw::IceGatheringState::New => Self::New,
            rw::IceGatheringState::Gathering => Self::Gathering,
            rw::IceGatheringState::Complete => Self::Complete,
        }
    }
}

/// Data channel readiness state.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DataChannelState {
    Connecting,
    Open,
    Closing,
    Closed,
}

impl From<rw::DataChannelState> for DataChannelState {
    fn from(s: rw::DataChannelState) -> Self {
        match s {
            rw::DataChannelState::Connecting => Self::Connecting,
            rw::DataChannelState::Open => Self::Open,
            rw::DataChannelState::Closing => Self::Closing,
            rw::DataChannelState::Closed => Self::Closed,
        }
    }
}

/// Audio vs. video.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
    Unknown,
}

impl From<rw::MediaKind> for MediaKind {
    fn from(k: rw::MediaKind) -> Self {
        match k {
            rw::MediaKind::Audio => Self::Audio,
            rw::MediaKind::Video => Self::Video,
            rw::MediaKind::Unknown => Self::Unknown,
        }
    }
}

/// RTP transceiver direction.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransceiverDirection {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl From<TransceiverDirection> for rw::TransceiverDirection {
    fn from(d: TransceiverDirection) -> Self {
        match d {
            TransceiverDirection::SendRecv => rw::TransceiverDirection::SendRecv,
            TransceiverDirection::SendOnly => rw::TransceiverDirection::SendOnly,
            TransceiverDirection::RecvOnly => rw::TransceiverDirection::RecvOnly,
            TransceiverDirection::Inactive => rw::TransceiverDirection::Inactive,
        }
    }
}

// ── Track ─────────────────────────────────────────────────────────────────────

/// A media track — local (push frames) or remote (attach a sink).
///
/// Video frames are delivered as BGRA bytes (`width * height * 4`).
/// Audio frames are interleaved signed 16-bit little-endian PCM bytes.
#[pyclass]
pub struct Track {
    inner: rw::Track,
}

#[pymethods]
impl Track {
    fn kind(&self) -> MediaKind {
        MediaKind::from(self.inner.kind())
    }

    /// Push a raw BGRA video frame into a local video track.
    fn push_video_frame(&self, bgra: &[u8], width: u32, height: u32) {
        self.inner.push_video_frame(bgra, width, height);
    }

    /// Register `callback(bgra: bytes, width: int, height: int)` for decoded
    /// video frames from a remote track. Fires on a WebRTC thread.
    fn on_video_frame(&mut self, callback: PyObject) {
        self.inner.on_video_frame(move |frame| {
            Python::with_gil(|py| {
                let bytes = PyBytes::new_bound(py, frame.bgra);
                let _ = callback.call1(py, (bytes, frame.width, frame.height));
            });
        });
    }

    /// Register `callback(pcm: bytes, sample_rate, channels, frames)` for
    /// decoded audio from a remote track. `pcm` is i16 little-endian.
    fn on_audio_frame(&mut self, callback: PyObject) {
        self.inner.on_audio_frame(move |frame| {
            Python::with_gil(|py| {
                let raw: Vec<u8> = frame.pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                let bytes = PyBytes::new_bound(py, &raw);
                let _ =
                    callback.call1(py, (bytes, frame.sample_rate, frame.channels, frame.frames));
            });
        });
    }
}

impl Track {
    fn from_rust(track: rw::Track) -> Self {
        Self { inner: track }
    }
}

// ── EncodedVideoTrack ─────────────────────────────────────────────────────────

/// An H.264/VP8/VP9 pre-encoded video track.
///
/// Obtain from `PeerConnectionFactory.with_encoded_video_track()`. Push
/// compressed frames with `push_encoded_frame`. Use `add_to_peer_connection`
/// or `add_transceiver` to wire it into a `PeerConnection`.
#[pyclass]
pub struct EncodedVideoTrack {
    inner: rw::EncodedVideoTrack,
}

#[pymethods]
impl EncodedVideoTrack {
    /// Push a compressed video frame.
    ///
    /// `data` — Annex-B H.264 or VP8/VP9 payload.
    /// Pass `width=0`, `height=0`, `rtp_timestamp=0` to inherit from the
    /// track's configured resolution.
    #[pyo3(signature = (data, is_key_frame=false, width=0, height=0, rtp_timestamp=0))]
    fn push_encoded_frame(
        &self,
        data: &[u8],
        is_key_frame: bool,
        width: u32,
        height: u32,
        rtp_timestamp: u32,
    ) {
        self.inner.push_encoded_frame(rw::EncodedVideoFrame {
            data: data.to_vec(),
            is_key_frame,
            width,
            height,
            rtp_timestamp,
        });
    }

    /// Add this track to a peer connection via `add_track` (creates a sendrecv
    /// transceiver automatically).
    fn add_to_peer_connection(&self, pc: &PeerConnection) -> PyResult<()> {
        pc.inner.add_track(self.inner.track()).map_err(err)
    }

    /// Add a transceiver of the given `direction` for this track. Returns the
    /// `Transceiver` (its `mid` is set after SDP exchange).
    fn add_transceiver(
        &self,
        pc: &PeerConnection,
        direction: TransceiverDirection,
    ) -> PyResult<Transceiver> {
        let t = pc
            .inner
            .add_transceiver(
                rw::MediaKind::Video,
                rw::TransceiverDirection::from(direction),
            )
            .map_err(err)?;
        t.set_track(self.inner.track()).map_err(err)?;
        Ok(Transceiver { inner: t })
    }
}

// ── Transceiver ───────────────────────────────────────────────────────────────

/// An RTP transceiver (one m-section in the SDP).
#[pyclass]
pub struct Transceiver {
    inner: rw::Transceiver,
}

#[pymethods]
impl Transceiver {
    /// The `mid`, set after `set_local_description`.
    fn mid(&self) -> Option<String> {
        self.inner.mid()
    }

    /// Attach a local track to the sender slot.
    fn set_track(&self, track: &Track) -> PyResult<()> {
        self.inner.set_track(&track.inner).map_err(err)
    }
}

// ── DataChannel ───────────────────────────────────────────────────────────────

/// An SCTP data channel for binary or text messaging.
#[pyclass]
pub struct DataChannel {
    inner: rw::DataChannel,
}

#[pymethods]
impl DataChannel {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn state(&self) -> DataChannelState {
        DataChannelState::from(self.inner.state())
    }
    fn buffered_amount(&self) -> u64 {
        self.inner.buffered_amount()
    }

    /// Send bytes. `binary=True` for binary SCTP messages, `False` for text.
    fn send(&self, data: &[u8], binary: bool) -> PyResult<()> {
        self.inner.send(data, binary).map_err(err)
    }

    /// Register `callback(data: bytes, binary: bool)` for incoming messages.
    fn on_message(&mut self, callback: PyObject) {
        self.inner.on_message(move |data, binary| {
            Python::with_gil(|py| {
                let bytes = PyBytes::new_bound(py, data);
                let _ = callback.call1(py, (bytes, binary));
            });
        });
    }

    /// Register `callback(state: DataChannelState)` for state transitions.
    fn on_state_change(&mut self, callback: PyObject) {
        self.inner.on_state_change(move |s| {
            Python::with_gil(|py| {
                let _ = callback.call1(py, (DataChannelState::from(s),));
            });
        });
    }

    /// Fire `callback()` once when the channel opens.
    fn on_open(&mut self, callback: PyObject) {
        self.inner.on_open(move || {
            Python::with_gil(|py| {
                let _ = callback.call0(py);
            });
        });
    }

    /// Fire `callback()` once when the channel closes.
    fn on_close(&mut self, callback: PyObject) {
        self.inner.on_close(move || {
            Python::with_gil(|py| {
                let _ = callback.call0(py);
            });
        });
    }
}

// ── Observer ──────────────────────────────────────────────────────────────────

/// Callback holder for peer-connection events.
///
/// Set attributes to Python callables before passing to
/// `PeerConnectionFactory.create_peer_connection()`.
///
/// All callbacks fire on WebRTC internal threads — keep them fast or hand off
/// heavy work to a thread pool / asyncio executor.
///
/// Example::
///
///     obs = PeerConnectionObserver()
///     obs.on_ice_candidate = lambda c: relay_to_peer(c)
///     obs.on_connection_state_change = lambda s: print("state:", s)
#[pyclass]
#[derive(Default)]
pub struct PeerConnectionObserver {
    on_connection_state_change: Option<PyObject>,
    on_ice_gathering_change: Option<PyObject>,
    on_ice_candidate: Option<PyObject>,
    on_track: Option<PyObject>,
    on_data_channel: Option<PyObject>,
}

#[pymethods]
impl PeerConnectionObserver {
    #[new]
    fn new() -> Self {
        Self::default()
    }

    #[setter]
    fn set_on_connection_state_change(&mut self, cb: PyObject) {
        self.on_connection_state_change = Some(cb);
    }
    #[getter]
    fn get_on_connection_state_change(&self) -> Option<&PyObject> {
        self.on_connection_state_change.as_ref()
    }

    #[setter]
    fn set_on_ice_gathering_change(&mut self, cb: PyObject) {
        self.on_ice_gathering_change = Some(cb);
    }
    #[getter]
    fn get_on_ice_gathering_change(&self) -> Option<&PyObject> {
        self.on_ice_gathering_change.as_ref()
    }

    #[setter]
    fn set_on_ice_candidate(&mut self, cb: PyObject) {
        self.on_ice_candidate = Some(cb);
    }
    #[getter]
    fn get_on_ice_candidate(&self) -> Option<&PyObject> {
        self.on_ice_candidate.as_ref()
    }

    #[setter]
    fn set_on_track(&mut self, cb: PyObject) {
        self.on_track = Some(cb);
    }
    #[getter]
    fn get_on_track(&self) -> Option<&PyObject> {
        self.on_track.as_ref()
    }

    #[setter]
    fn set_on_data_channel(&mut self, cb: PyObject) {
        self.on_data_channel = Some(cb);
    }
    #[getter]
    fn get_on_data_channel(&self) -> Option<&PyObject> {
        self.on_data_channel.as_ref()
    }
}

impl PeerConnectionObserver {
    fn build_rust_observer(&self, py: Python) -> rw::PeerConnectionObserver {
        let mut obs = rw::PeerConnectionObserver::new();

        if let Some(cb) = &self.on_connection_state_change {
            let cb = cb.clone_ref(py);
            obs = obs.on_connection_state_change(move |s| {
                Python::with_gil(|py| {
                    let _ = cb.call1(py, (PeerConnectionState::from(s),));
                });
            });
        }
        if let Some(cb) = &self.on_ice_gathering_change {
            let cb = cb.clone_ref(py);
            obs = obs.on_ice_gathering_change(move |s| {
                Python::with_gil(|py| {
                    let _ = cb.call1(py, (IceGatheringState::from(s),));
                });
            });
        }
        if let Some(cb) = &self.on_ice_candidate {
            let cb = cb.clone_ref(py);
            obs = obs.on_ice_candidate(move |cand| {
                Python::with_gil(|py| {
                    let py_cand = Py::new(py, IceCandidate::from(cand)).unwrap();
                    let _ = cb.call1(py, (py_cand,));
                });
            });
        }
        if let Some(cb) = &self.on_track {
            let cb = cb.clone_ref(py);
            obs = obs.on_track(move |kind, track| {
                Python::with_gil(|py| {
                    let py_track = Py::new(py, Track::from_rust(track)).unwrap();
                    let _ = cb.call1(py, (MediaKind::from(kind), py_track));
                });
            });
        }
        if let Some(cb) = &self.on_data_channel {
            let cb = cb.clone_ref(py);
            obs = obs.on_data_channel(move |dc| {
                Python::with_gil(|py| {
                    let py_dc = Py::new(py, DataChannel { inner: dc }).unwrap();
                    let _ = cb.call1(py, (py_dc,));
                });
            });
        }
        obs
    }
}

// ── PeerConnection ────────────────────────────────────────────────────────────

/// An RTCPeerConnection.
///
/// Signaling methods (`create_offer`, `create_answer`, etc.) block for up to
/// ~5 ms while the WebRTC engine responds. Wrap in `asyncio.to_thread()` when
/// calling from an async context.
#[pyclass]
pub struct PeerConnection {
    inner: rw::PeerConnection,
}

#[pymethods]
impl PeerConnection {
    fn create_offer(&self, py: Python) -> PyResult<SessionDescription> {
        py.allow_threads(|| self.inner.create_offer())
            .map(SessionDescription::from)
            .map_err(err)
    }
    fn create_answer(&self, py: Python) -> PyResult<SessionDescription> {
        py.allow_threads(|| self.inner.create_answer())
            .map(SessionDescription::from)
            .map_err(err)
    }
    fn set_local_description(&self, py: Python, sdp: &SessionDescription) -> PyResult<()> {
        let rust = to_rust_sdp(sdp)?;
        py.allow_threads(|| self.inner.set_local_description(&rust))
            .map_err(err)
    }
    fn set_remote_description(&self, py: Python, sdp: &SessionDescription) -> PyResult<()> {
        let rust = to_rust_sdp(sdp)?;
        py.allow_threads(|| self.inner.set_remote_description(&rust))
            .map_err(err)
    }
    fn add_ice_candidate(&self, py: Python, candidate: &IceCandidate) -> PyResult<()> {
        let rust = rw::IceCandidate::from(candidate);
        py.allow_threads(|| self.inner.add_ice_candidate(&rust))
            .map_err(err)
    }
    fn add_track(&self, track: &Track) -> PyResult<()> {
        self.inner.add_track(&track.inner).map_err(err)
    }
    fn add_transceiver(
        &self,
        kind: MediaKind,
        direction: TransceiverDirection,
    ) -> PyResult<Transceiver> {
        let rust_kind = match kind {
            MediaKind::Audio => rw::MediaKind::Audio,
            MediaKind::Video => rw::MediaKind::Video,
            MediaKind::Unknown => return Err(PyRuntimeError::new_err("need Audio or Video")),
        };
        self.inner
            .add_transceiver(rust_kind, rw::TransceiverDirection::from(direction))
            .map(|t| Transceiver { inner: t })
            .map_err(err)
    }
    fn create_data_channel(&self, label: &str) -> PyResult<DataChannel> {
        self.inner
            .create_data_channel(label)
            .map(|inner| DataChannel { inner })
            .map_err(err)
    }
}

// ── PeerConnectionFactory ─────────────────────────────────────────────────────

/// Entry point — creates peer connections and media tracks.
///
/// Uses the **synthetic** (push-based) ADM by default (no audio hardware).
/// Pass `platform_adm=True` for real mic/speaker on desktop.
#[pyclass]
pub struct PeerConnectionFactory {
    inner: rw::PeerConnectionFactory,
}

#[pymethods]
impl PeerConnectionFactory {
    #[new]
    #[pyo3(signature = (platform_adm=false))]
    fn new(platform_adm: bool) -> PyResult<Self> {
        let adm = if platform_adm {
            rw::AdmMode::Platform
        } else {
            rw::AdmMode::Synthetic
        };
        rw::PeerConnectionFactory::with_adm(adm)
            .map(|inner| Self { inner })
            .map_err(err)
    }

    /// Create a `PeerConnection` with `config` and `observer`.
    fn create_peer_connection(
        &self,
        py: Python,
        config: &RtcConfiguration,
        observer: &PeerConnectionObserver,
    ) -> PyResult<PeerConnection> {
        let rust_config = rw::RtcConfiguration::from(config);
        let rust_obs = observer.build_rust_observer(py);
        self.inner
            .create_peer_connection(&rust_config, rust_obs)
            .map(|inner| PeerConnection { inner })
            .map_err(err)
    }

    /// Create a local video track (push frames via `Track.push_video_frame`).
    fn create_video_track(&self, id: &str) -> PyResult<Track> {
        self.inner
            .create_video_track(id)
            .map(Track::from_rust)
            .map_err(err)
    }

    /// Create a local audio track. Feed samples via `push_audio_frame`.
    fn create_audio_track(&self, id: &str) -> PyResult<Track> {
        self.inner
            .create_audio_track(id)
            .map(Track::from_rust)
            .map_err(err)
    }

    /// Push interleaved signed 16-bit little-endian PCM to the synthetic ADM.
    /// `pcm` must have even length (2 bytes per i16 sample).
    fn push_audio_frame(&self, pcm: &[u8], sample_rate: u32, channels: u32) -> PyResult<()> {
        if !pcm.len().is_multiple_of(2) {
            return Err(PyRuntimeError::new_err("pcm byte length must be even"));
        }
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        self.inner.push_audio_frame(&samples, sample_rate, channels);
        Ok(())
    }

    /// Create a factory + pre-encoded video track pair.
    ///
    /// Returns `(factory, encoded_track)`. Call
    /// `encoded_track.add_to_peer_connection(pc)` or
    /// `encoded_track.add_transceiver(pc, direction)` to wire it up.
    #[staticmethod]
    fn with_encoded_video_track(
        track_id: &str,
        width: u32,
        height: u32,
    ) -> PyResult<(Self, EncodedVideoTrack)> {
        rw::PeerConnectionFactory::with_encoded_video_track(track_id, width, height)
            .map(|(f, t)| (Self { inner: f }, EncodedVideoTrack { inner: t }))
            .map_err(err)
    }

    fn set_adm_playout_enabled(&self, enabled: bool) {
        self.inner.set_adm_playout_enabled(enabled);
    }
}

// ── Module ────────────────────────────────────────────────────────────────────

#[pymodule]
fn reactor_webrtc(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<IceServer>()?;
    m.add_class::<RtcConfiguration>()?;
    m.add_class::<IceCandidate>()?;
    m.add_class::<SessionDescription>()?;
    m.add_class::<PeerConnectionState>()?;
    m.add_class::<IceGatheringState>()?;
    m.add_class::<DataChannelState>()?;
    m.add_class::<MediaKind>()?;
    m.add_class::<TransceiverDirection>()?;
    m.add_class::<Track>()?;
    m.add_class::<EncodedVideoTrack>()?;
    m.add_class::<Transceiver>()?;
    m.add_class::<DataChannel>()?;
    m.add_class::<PeerConnectionObserver>()?;
    m.add_class::<PeerConnection>()?;
    m.add_class::<PeerConnectionFactory>()?;
    Ok(())
}
