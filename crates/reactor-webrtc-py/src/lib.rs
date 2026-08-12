//! PyO3 bindings for `reactor-webrtc`.
//!
//! Imported from Python as `import reactor_webrtc`. `PeerConnection`'s
//! signaling methods (`create_offer`, `create_answer`, `set_local_description`,
//! `set_remote_description`, `add_ice_candidate`, `get_stats`) are natively
//! awaitable, backed by a small tokio runtime (see the `#[pymodule]` init)
//! that runs the blocking libwebrtc round-trip via `spawn_blocking`. Every
//! other blocking operation releases the GIL (`py.allow_threads`) so
//! callbacks that re-acquire it can fire without deadlocking.
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

use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// Process-wide guard: libwebrtc starts global threads on factory creation and
// joins them on destruction.  Creating a second factory before the first is
// fully destroyed races those threads and reliably segfaults.  One factory at
// a time is the safe contract; enforce it here so callers get a clear error
// instead of a crash.
static FACTORY_LIVE: AtomicBool = AtomicBool::new(false);

fn claim_factory() -> PyResult<()> {
    FACTORY_LIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .map(|_| ())
        .map_err(|_| {
            PyRuntimeError::new_err(
                "a PeerConnectionFactory is already alive in this process; \
                 drop it before creating another",
            )
        })
}

fn err(e: rw::Error) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Maps a `spawn_blocking` `JoinError` (task panic) to a `PyErr`.
fn join_err(e: tokio::task::JoinError) -> PyErr {
    PyRuntimeError::new_err(format!("task join: {e}"))
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

const ICE_TRANSPORT_TYPES: &str = "all, relay, no_host, none";
const GATHERING_POLICIES: &str = "once, continually";

fn ice_transport_type_to_str(t: rw::IceTransportsType) -> &'static str {
    match t {
        rw::IceTransportsType::All => "all",
        rw::IceTransportsType::Relay => "relay",
        rw::IceTransportsType::NoHost => "no_host",
        rw::IceTransportsType::None => "none",
    }
}

fn parse_ice_transport_type(s: &str) -> PyResult<rw::IceTransportsType> {
    match s {
        "all" => Ok(rw::IceTransportsType::All),
        "relay" => Ok(rw::IceTransportsType::Relay),
        "no_host" => Ok(rw::IceTransportsType::NoHost),
        "none" => Ok(rw::IceTransportsType::None),
        other => Err(PyValueError::new_err(format!(
            "unknown ice_transport_type {other:?}; use one of: {ICE_TRANSPORT_TYPES}"
        ))),
    }
}

fn gathering_policy_to_str(p: rw::ContinualGatheringPolicy) -> &'static str {
    match p {
        rw::ContinualGatheringPolicy::GatherOnce => "once",
        rw::ContinualGatheringPolicy::GatherContinually => "continually",
    }
}

fn parse_gathering_policy(s: &str) -> PyResult<rw::ContinualGatheringPolicy> {
    match s {
        "once" => Ok(rw::ContinualGatheringPolicy::GatherOnce),
        "continually" => Ok(rw::ContinualGatheringPolicy::GatherContinually),
        other => Err(PyValueError::new_err(format!(
            "unknown continual_gathering_policy {other:?}; use one of: {GATHERING_POLICIES}"
        ))),
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

/// A single STUN/TURN server entry.
///
/// All URLs in one entry share `username` and `password`. A `turn:` or `turns:`
/// URL needs both credentials: libwebrtc rejects the whole configuration when
/// either one is empty.
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
///
/// `ice_transport_type` restricts which candidate types ICE may use
/// (`all`, `relay`, `no_host`, `none`). `continual_gathering_policy` selects
/// whether ICE gathers once or keeps gathering (`once`, `continually`).
/// `min_port` and `max_port` bound the UDP port range ICE may allocate;
/// `0` (the default) leaves the OS-assigned ephemeral range unchanged.
#[pyclass]
#[derive(Clone)]
pub struct RtcConfiguration {
    pub ice_servers: Vec<IceServer>,
    ice_transport_type: rw::IceTransportsType,
    continual_gathering_policy: rw::ContinualGatheringPolicy,
    pub min_port: u16,
    pub max_port: u16,
    bundle_policy: rw::BundlePolicy,
    pub ice_connection_receiving_timeout_ms: i32,
    pub ice_check_interval_strong_connectivity_ms: i32,
    tcp_candidate_policy: rw::TcpCandidatePolicy,
    /// Whether this connection takes part in per-frame metadata. `True` by
    /// default; see the class docstring in the stub.
    pub frame_metadata: bool,
}

#[pymethods]
impl RtcConfiguration {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (
        ice_servers=vec![],
        ice_transport_type="all",
        continual_gathering_policy="once",
        min_port=0,
        max_port=0,
        bundle_policy=BundlePolicy::Balanced,
        ice_connection_receiving_timeout_ms=0,
        ice_check_interval_strong_connectivity_ms=0,
        tcp_candidate_policy=TcpCandidatePolicy::Disabled,
        frame_metadata=true,
    ))]
    fn new(
        ice_servers: Vec<IceServer>,
        ice_transport_type: &str,
        continual_gathering_policy: &str,
        min_port: u16,
        max_port: u16,
        bundle_policy: BundlePolicy,
        ice_connection_receiving_timeout_ms: i32,
        ice_check_interval_strong_connectivity_ms: i32,
        tcp_candidate_policy: TcpCandidatePolicy,
        frame_metadata: bool,
    ) -> PyResult<Self> {
        Ok(Self {
            ice_servers,
            ice_transport_type: parse_ice_transport_type(ice_transport_type)?,
            continual_gathering_policy: parse_gathering_policy(continual_gathering_policy)?,
            min_port,
            max_port,
            bundle_policy: rw::BundlePolicy::from(bundle_policy),
            ice_connection_receiving_timeout_ms,
            ice_check_interval_strong_connectivity_ms,
            tcp_candidate_policy: rw::TcpCandidatePolicy::from(tcp_candidate_policy),
            frame_metadata,
        })
    }
    #[getter]
    fn ice_servers(&self) -> Vec<IceServer> {
        self.ice_servers.clone()
    }
    #[setter]
    fn set_ice_servers(&mut self, servers: Vec<IceServer>) {
        self.ice_servers = servers;
    }
    #[getter]
    fn ice_transport_type(&self) -> &'static str {
        ice_transport_type_to_str(self.ice_transport_type)
    }
    #[setter]
    fn set_ice_transport_type(&mut self, value: &str) -> PyResult<()> {
        self.ice_transport_type = parse_ice_transport_type(value)?;
        Ok(())
    }
    #[getter]
    fn continual_gathering_policy(&self) -> &'static str {
        gathering_policy_to_str(self.continual_gathering_policy)
    }
    #[setter]
    fn set_continual_gathering_policy(&mut self, value: &str) -> PyResult<()> {
        self.continual_gathering_policy = parse_gathering_policy(value)?;
        Ok(())
    }
    #[getter]
    fn min_port(&self) -> u16 {
        self.min_port
    }
    #[setter]
    fn set_min_port(&mut self, value: u16) {
        self.min_port = value;
    }
    #[getter]
    fn max_port(&self) -> u16 {
        self.max_port
    }
    #[setter]
    fn set_max_port(&mut self, value: u16) {
        self.max_port = value;
    }

    #[getter]
    fn bundle_policy(&self) -> BundlePolicy {
        BundlePolicy::from(self.bundle_policy)
    }
    #[setter]
    fn set_bundle_policy(&mut self, value: BundlePolicy) {
        self.bundle_policy = rw::BundlePolicy::from(value);
    }

    #[getter]
    fn ice_connection_receiving_timeout_ms(&self) -> i32 {
        self.ice_connection_receiving_timeout_ms
    }
    #[setter]
    fn set_ice_connection_receiving_timeout_ms(&mut self, value: i32) {
        self.ice_connection_receiving_timeout_ms = value;
    }

    #[getter]
    fn ice_check_interval_strong_connectivity_ms(&self) -> i32 {
        self.ice_check_interval_strong_connectivity_ms
    }
    #[setter]
    fn set_ice_check_interval_strong_connectivity_ms(&mut self, value: i32) {
        self.ice_check_interval_strong_connectivity_ms = value;
    }

    #[getter]
    fn tcp_candidate_policy(&self) -> TcpCandidatePolicy {
        TcpCandidatePolicy::from(self.tcp_candidate_policy)
    }
    #[setter]
    fn set_tcp_candidate_policy(&mut self, value: TcpCandidatePolicy) {
        self.tcp_candidate_policy = rw::TcpCandidatePolicy::from(value);
    }

    #[getter]
    fn frame_metadata(&self) -> bool {
        self.frame_metadata
    }
    #[setter]
    fn set_frame_metadata(&mut self, value: bool) {
        self.frame_metadata = value;
    }
}

impl From<&RtcConfiguration> for rw::RtcConfiguration {
    fn from(c: &RtcConfiguration) -> Self {
        rw::RtcConfiguration {
            ice_servers: c.ice_servers.iter().map(Into::into).collect(),
            ice_transport_type: c.ice_transport_type,
            continual_gathering_policy: c.continual_gathering_policy,
            min_port: if c.min_port > 0 {
                Some(c.min_port)
            } else {
                None
            },
            max_port: if c.max_port > 0 {
                Some(c.max_port)
            } else {
                None
            },
            bundle_policy: c.bundle_policy,
            ice_connection_receiving_timeout_ms: if c.ice_connection_receiving_timeout_ms > 0 {
                Some(c.ice_connection_receiving_timeout_ms)
            } else {
                None
            },
            ice_check_interval_strong_connectivity_ms: if c
                .ice_check_interval_strong_connectivity_ms
                > 0
            {
                Some(c.ice_check_interval_strong_connectivity_ms)
            } else {
                None
            },
            tcp_candidate_policy: c.tcp_candidate_policy,
            frame_metadata: c.frame_metadata,
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

    /// The `ice-ufrag` values this description carries, in document order.
    ///
    /// One per m-section: bundled sections repeat the same value, while a
    /// non-BUNDLE description has a distinct ufrag per transport.
    fn ice_ufrags(&self) -> Vec<String> {
        self.as_rust_for_reading()
            .ice_ufrags()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Return a copy with every `ice-ufrag` and `ice-pwd` replaced.
    ///
    /// libwebrtc generates ICE credentials itself and exposes no setter, so an
    /// application that needs to *choose* its ufrag — routing through an edge
    /// relay that demultiplexes on it, for instance — substitutes them in the
    /// description instead. That works because the local description is what
    /// libwebrtc reads the transport's ICE parameters from.
    ///
    /// Call it on the result of `create_offer`/`create_answer` and **before**
    /// `set_local_description`: setting the local description is what creates the
    /// transport and starts gathering, so substituting afterwards acts on nothing.
    ///
    /// Raises if either value is outside RFC 8445's length range (ufrag 4..=256,
    /// password 22..=256) or contains a character outside `ice-char`.
    fn with_ice_credentials(&self, ufrag: &str, pwd: &str) -> PyResult<Self> {
        let out = to_rust_sdp(self)?
            .with_ice_credentials(ufrag, pwd)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(out.into())
    }

    /// Whether this description declares frame-metadata support.
    ///
    /// True when it carries a session-level `a=x-reactor-frame-metadata:<version>`
    /// at a version this build understands. A peer speaking a different trailer
    /// format therefore reads as unsupported rather than as a partial match.
    ///
    /// `set_remote_description` arms the connection's `FrameMetadataGate` from
    /// exactly this.
    fn declares_frame_metadata(&self) -> bool {
        self.as_rust_for_reading().declares_frame_metadata()
    }

    /// Return a copy declaring frame-metadata support, as a session-level attribute.
    ///
    /// `create_offer` already applies this to every offer and `create_answer`
    /// mirrors the offer, so callers using this library's signalling path never need
    /// it. Public for callers that assemble or rewrite SDP themselves. Idempotent.
    fn with_frame_metadata(&self) -> Self {
        // Preserves `kind` verbatim instead of validating it: declaring the
        // capability does not depend on the SDP type, so an unfamiliar `kind` is not
        // a reason to raise here. Same reasoning as `as_rust_for_reading`.
        Self {
            kind: self.kind.clone(),
            sdp: self.as_rust_for_reading().with_frame_metadata().sdp,
        }
    }
}

impl SessionDescription {
    /// The SDP as a Rust description, for read-only queries that do not depend
    /// on `kind`.
    ///
    /// Reading through `to_rust_sdp` instead would turn an unrelated "unknown SDP
    /// kind" into an empty/`None` answer — indistinguishable from a description
    /// that genuinely carries nothing. Python can set `kind` to any string, so
    /// that mistake is one typo away.
    fn as_rust_for_reading(&self) -> rw::SessionDescription {
        rw::SessionDescription {
            kind: rw::SdpType::Offer,
            sdp: self.sdp.clone(),
        }
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

/// How m-sections are bundled onto a single transport.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BundlePolicy {
    /// One transport per m-section that needs one (libwebrtc default).
    Balanced = 0,
    /// All m-sections share one transport — recommended for streaming.
    MaxBundle = 1,
    /// One transport per m-section (maximises legacy stack compatibility).
    MaxCompat = 2,
}

impl From<BundlePolicy> for rw::BundlePolicy {
    fn from(p: BundlePolicy) -> Self {
        match p {
            BundlePolicy::Balanced => rw::BundlePolicy::Balanced,
            BundlePolicy::MaxBundle => rw::BundlePolicy::MaxBundle,
            BundlePolicy::MaxCompat => rw::BundlePolicy::MaxCompat,
        }
    }
}

impl From<rw::BundlePolicy> for BundlePolicy {
    fn from(p: rw::BundlePolicy) -> Self {
        match p {
            rw::BundlePolicy::Balanced => BundlePolicy::Balanced,
            rw::BundlePolicy::MaxBundle => BundlePolicy::MaxBundle,
            rw::BundlePolicy::MaxCompat => BundlePolicy::MaxCompat,
        }
    }
}

/// Whether libwebrtc gathers TCP ICE candidates.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TcpCandidatePolicy {
    /// TCP candidates are not gathered (libwebrtc default).
    Disabled = 0,
    /// TCP candidates are gathered alongside UDP.
    Enabled = 1,
}

impl From<TcpCandidatePolicy> for rw::TcpCandidatePolicy {
    fn from(p: TcpCandidatePolicy) -> Self {
        match p {
            TcpCandidatePolicy::Disabled => rw::TcpCandidatePolicy::Disabled,
            TcpCandidatePolicy::Enabled => rw::TcpCandidatePolicy::Enabled,
        }
    }
}

impl From<rw::TcpCandidatePolicy> for TcpCandidatePolicy {
    fn from(p: rw::TcpCandidatePolicy) -> Self {
        match p {
            rw::TcpCandidatePolicy::Disabled => TcpCandidatePolicy::Disabled,
            rw::TcpCandidatePolicy::Enabled => TcpCandidatePolicy::Enabled,
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

/// A negotiable video codec, for [`Transceiver.set_codec_preferences`].
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Vp8,
    Vp9,
    Av1,
    H264,
    H265,
}

impl From<VideoCodec> for rw::VideoCodec {
    fn from(c: VideoCodec) -> Self {
        match c {
            VideoCodec::Vp8 => rw::VideoCodec::Vp8,
            VideoCodec::Vp9 => rw::VideoCodec::Vp9,
            VideoCodec::Av1 => rw::VideoCodec::Av1,
            VideoCodec::H264 => rw::VideoCodec::H264,
            VideoCodec::H265 => rw::VideoCodec::H265,
        }
    }
}

// ── Stats types ───────────────────────────────────────────────────────────────

/// ICE candidate-pair state (`RTCIceCandidatePairStats::state`).
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IceCandidatePairState {
    Waiting,
    InProgress,
    Failed,
    Succeeded,
    Cancelled,
}

impl From<rw::IceCandidatePairState> for IceCandidatePairState {
    fn from(s: rw::IceCandidatePairState) -> Self {
        match s {
            rw::IceCandidatePairState::Waiting => Self::Waiting,
            rw::IceCandidatePairState::InProgress => Self::InProgress,
            rw::IceCandidatePairState::Failed => Self::Failed,
            rw::IceCandidatePairState::Succeeded => Self::Succeeded,
            rw::IceCandidatePairState::Cancelled => Self::Cancelled,
        }
    }
}

/// Subset of `RTCInboundRtpStreamStats`.
#[pyclass(get_all)]
#[derive(Clone)]
pub struct InboundRtpStats {
    pub ssrc: u32,
    pub packets_received: u32,
    pub bytes_received: u64,
    /// Jitter in seconds.
    pub jitter_s: f64,
    pub packets_lost: i32,
    pub nack_count: u32,
    /// Cumulative decode time in seconds.
    pub total_decode_time_s: f64,
}

#[pymethods]
impl InboundRtpStats {
    fn __repr__(&self) -> String {
        format!("InboundRtpStats(ssrc={})", self.ssrc)
    }
}

impl From<rw::InboundRtpStats> for InboundRtpStats {
    fn from(s: rw::InboundRtpStats) -> Self {
        Self {
            ssrc: s.ssrc,
            packets_received: s.packets_received,
            bytes_received: s.bytes_received,
            jitter_s: s.jitter_s,
            packets_lost: s.packets_lost,
            nack_count: s.nack_count,
            total_decode_time_s: s.total_decode_time_s,
        }
    }
}

/// Subset of `RTCOutboundRtpStreamStats`.
#[pyclass(get_all)]
#[derive(Clone)]
pub struct OutboundRtpStats {
    pub ssrc: u32,
    pub packets_sent: u32,
    pub bytes_sent: u64,
    /// Target encoder bitrate in bps.
    pub target_bitrate_bps: f64,
    /// Round-trip time in seconds; `0.0` if not yet measured.
    pub round_trip_time_s: f64,
    pub retransmitted_packets_sent: u32,
}

#[pymethods]
impl OutboundRtpStats {
    fn __repr__(&self) -> String {
        format!("OutboundRtpStats(ssrc={})", self.ssrc)
    }
}

impl From<rw::OutboundRtpStats> for OutboundRtpStats {
    fn from(s: rw::OutboundRtpStats) -> Self {
        Self {
            ssrc: s.ssrc,
            packets_sent: s.packets_sent,
            bytes_sent: s.bytes_sent,
            target_bitrate_bps: s.target_bitrate_bps,
            round_trip_time_s: s.round_trip_time_s,
            retransmitted_packets_sent: s.retransmitted_packets_sent,
        }
    }
}

/// Subset of `RTCIceCandidatePairStats`.
#[pyclass(get_all)]
#[derive(Clone)]
pub struct IceCandidatePairStats {
    /// Current RTT in seconds; `0.0` if not yet measured.
    pub current_round_trip_time_s: f64,
    pub priority: u64,
    pub state: IceCandidatePairState,
}

#[pymethods]
impl IceCandidatePairStats {
    fn __repr__(&self) -> String {
        format!("IceCandidatePairStats(state={:?})", self.state as i32)
    }
}

impl From<rw::IceCandidatePairStats> for IceCandidatePairStats {
    fn from(s: rw::IceCandidatePairStats) -> Self {
        Self {
            current_round_trip_time_s: s.current_round_trip_time_s,
            priority: s.priority,
            state: IceCandidatePairState::from(s.state),
        }
    }
}

/// Snapshot delivered by `PeerConnection.get_stats()`.
#[pyclass(get_all)]
#[derive(Clone)]
pub struct StatsReport {
    pub inbound_rtp: Vec<InboundRtpStats>,
    pub outbound_rtp: Vec<OutboundRtpStats>,
    pub candidate_pairs: Vec<IceCandidatePairStats>,
}

#[pymethods]
impl StatsReport {
    fn __repr__(&self) -> String {
        format!(
            "StatsReport(inbound={}, outbound={}, pairs={})",
            self.inbound_rtp.len(),
            self.outbound_rtp.len(),
            self.candidate_pairs.len()
        )
    }
}

impl From<rw::StatsReport> for StatsReport {
    fn from(r: rw::StatsReport) -> Self {
        Self {
            inbound_rtp: r.inbound_rtp.into_iter().map(Into::into).collect(),
            outbound_rtp: r.outbound_rtp.into_iter().map(Into::into).collect(),
            candidate_pairs: r.candidate_pairs.into_iter().map(Into::into).collect(),
        }
    }
}

// ── FrameMetadata ─────────────────────────────────────────────────────────────

/// Whether the remote peer has declared that it strips metadata trailers.
///
/// Obtained from `PeerConnection.frame_metadata_gate()`. It starts closed and is
/// armed by `set_remote_description`, which opens it when the remote description
/// declares `FRAME_METADATA_ATTRIBUTE`. The library consults the gate internally
/// when appending trailers; callers do not need to check it before pushing
/// `user_data`.
#[pyclass]
#[derive(Clone, Default)]
pub struct FrameMetadataGate {
    inner: rw::FrameMetadataGate,
}

#[pymethods]
impl FrameMetadataGate {
    /// A closed gate, not attached to any peer connection.
    ///
    /// Useful in tests and for a sender that negotiated support out of band.
    /// Production code takes one from `PeerConnection.frame_metadata_gate()` so
    /// that `set_remote_description` arms it.
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Whether trailers may be appended.
    fn is_open(&self) -> bool {
        self.inner.is_open()
    }

    fn __repr__(&self) -> String {
        format!(
            "FrameMetadataGate({})",
            if self.inner.is_open() {
                "open"
            } else {
                "closed"
            }
        )
    }
}

/// Metadata attached to a video frame via the packet trailer.
///
/// All fields default to zero / empty when not set by the sender.
#[pyclass]
#[derive(Clone, Default)]
pub struct FrameMetadata {
    /// Application-level frame counter (0 = unset).
    #[pyo3(get, set)]
    pub frame_id: u64,
    /// Wall-clock timestamp in microseconds (0 = unset).
    #[pyo3(get, set)]
    pub timestamp: u64,
    /// Arbitrary application payload (bytes).
    pub user_data: Vec<u8>,
}

#[pymethods]
impl FrameMetadata {
    #[new]
    #[pyo3(signature = (frame_id=0, timestamp=0, user_data=vec![]))]
    fn new(frame_id: u64, timestamp: u64, user_data: Vec<u8>) -> Self {
        Self {
            frame_id,
            timestamp,
            user_data,
        }
    }
    /// Returns `user_data` as Python `bytes`.
    #[getter]
    fn user_data<'py>(&self, py: Python<'py>) -> pyo3::Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.user_data)
    }

    /// Sets `user_data` from Python `bytes` or any buffer.
    #[setter]
    fn set_user_data(&mut self, data: Vec<u8>) {
        self.user_data = data;
    }

    fn __repr__(&self) -> String {
        format!(
            "FrameMetadata(frame_id={}, timestamp={}, user_data={} bytes)",
            self.frame_id,
            self.timestamp,
            self.user_data.len()
        )
    }
}

impl From<rw::FrameMetadata> for FrameMetadata {
    fn from(m: rw::FrameMetadata) -> Self {
        Self {
            frame_id: m.frame_id,
            timestamp: m.timestamp,
            user_data: m.user_data,
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
    inner: ManuallyDrop<rw::Track>,
}

#[pymethods]
impl Track {
    fn kind(&self) -> MediaKind {
        MediaKind::from(self.inner.kind())
    }

    /// Push a raw BGRA video frame into a local video track.
    ///
    /// Pass `user_data` (bytes) to embed per-frame metadata in the encoded
    /// packet trailer. `frame_id` and `timestamp` are computed automatically.
    /// Nothing else has to be arranged: the trailer is appended once the peer has
    /// declared that it strips them, and `user_data` is silently dropped while it
    /// has not. See `PeerConnection.frame_metadata_gate()`.
    #[pyo3(signature = (bgra, width, height, user_data=None))]
    fn push_video_frame(
        &self,
        py: Python,
        bgra: &[u8],
        width: u32,
        height: u32,
        user_data: Option<&[u8]>,
    ) -> PyResult<()> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| {
                PyRuntimeError::new_err(format!(
                    "frame dimensions overflow: {width}×{height}×4 exceeds usize"
                ))
            })?;
        if bgra.len() < expected {
            return Err(PyRuntimeError::new_err(format!(
                "bgra buffer too short: need {expected} bytes for {width}×{height}, got {}",
                bgra.len()
            )));
        }
        let owned = bgra.to_vec();
        match user_data {
            Some(ud) => {
                let ud = ud.to_vec();
                py.allow_threads(|| {
                    self.inner
                        .push_video_frame_with_metadata(&owned, width, height, &ud)
                });
            }
            None => {
                py.allow_threads(|| self.inner.push_video_frame(&owned, width, height));
            }
        }
        Ok(())
    }

    /// Register a callback for decoded video frames from a remote track.
    ///
    /// Signature: `callback(bgra: bytes, width: int, height: int, metadata: FrameMetadata | None)`
    ///
    /// For backward compatibility with 3-argument callbacks
    /// `callback(bgra, width, height)`, the 4-argument call is retried as a
    /// 3-argument call on `TypeError` when `metadata` is `None`.
    fn on_video_frame(&mut self, py: Python, callback: PyObject) {
        // Attaching a sink dispatches synchronously to the worker thread, which
        // is also where another track's frame callback re-enters Python. The GIL
        // has to be free across the dispatch or the two deadlock.
        py.allow_threads(|| {
            self.inner.on_video_frame(move |frame| {
                Python::with_gil(|py| {
                    let bytes = PyBytes::new_bound(py, frame.bgra);
                    let meta = frame.metadata.map(|m| {
                        Py::new(py, FrameMetadata::from(m))
                            .map(|p| p.into_any())
                            .unwrap_or_else(|_| py.None())
                    });
                    match meta {
                        Some(m) => {
                            let _ = callback.call1(py, (bytes, frame.width, frame.height, m));
                        }
                        None => {
                            // Try 4-arg (with None); fall back to legacy 3-arg on TypeError.
                            let result = callback
                                .call1(py, (bytes.clone(), frame.width, frame.height, py.None()));
                            if result.is_err() {
                                let _ = callback.call1(py, (bytes, frame.width, frame.height));
                            }
                        }
                    }
                });
            });
        });
    }

    /// Push interleaved signed 16-bit little-endian PCM to a local audio track
    /// created with `factory.create_audio_track_with_local_source()`. `pcm`
    /// must have even byte length (2 bytes per i16 sample). No-op for ADM-
    /// backed or remote tracks.
    fn push_pcm(&self, py: Python, pcm: &[u8], sample_rate: u32, channels: u32) -> PyResult<()> {
        if !pcm.len().is_multiple_of(2) {
            return Err(PyRuntimeError::new_err("pcm byte length must be even"));
        }
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        py.allow_threads(|| self.inner.push_pcm(&samples, sample_rate, channels))
            .map_err(err)
    }

    /// Register `callback(pcm: bytes, sample_rate, channels, frames)` for
    /// decoded audio from a remote track. `pcm` is i16 little-endian.
    fn on_audio_frame(&mut self, py: Python, callback: PyObject) {
        // Same worker-thread dispatch as `on_video_frame`.
        py.allow_threads(|| {
            self.inner.on_audio_frame(move |frame| {
                Python::with_gil(|py| {
                    let raw: Vec<u8> = frame.pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                    let bytes = PyBytes::new_bound(py, &raw);
                    let _ = callback
                        .call1(py, (bytes, frame.sample_rate, frame.channels, frame.frames));
                });
            });
        });
    }
}

impl Track {
    fn from_rust(track: rw::Track) -> Self {
        Self {
            inner: ManuallyDrop::new(track),
        }
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        // Releasing the native track dispatches to the thread that owns it,
        // where a frame or candidate callback may be inside Python. Release the
        // GIL so that callback can finish and the dispatch can be serviced.
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };
        Python::with_gil(|py| py.allow_threads(|| drop(inner)));
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
    inner: ManuallyDrop<rw::EncodedVideoTrack>,
}

impl Drop for EncodedVideoTrack {
    fn drop(&mut self) {
        // Wraps a native track; same dispatch-on-release as `Track`.
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };
        Python::with_gil(|py| py.allow_threads(|| drop(inner)));
    }
}

#[pymethods]
impl EncodedVideoTrack {
    /// Push a compressed video frame.
    ///
    /// Push a compressed video frame.
    ///
    /// `data` — Annex-B H.264 or VP8/VP9 payload.
    /// Pass `width=0`, `height=0`, `rtp_timestamp=0` to inherit from the
    /// track's configured resolution.
    /// Pass `user_data` (bytes) to embed per-frame metadata in the encoded
    /// packet trailer (same mechanism as `Track.push_video_frame`). Metadata
    /// is only sent when the peer has negotiated support; otherwise `user_data`
    /// is silently dropped.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (data, is_key_frame=false, width=0, height=0, rtp_timestamp=0, user_data=None))]
    fn push_encoded_frame(
        &self,
        py: Python,
        data: &[u8],
        is_key_frame: bool,
        width: u32,
        height: u32,
        rtp_timestamp: u32,
        user_data: Option<&[u8]>,
    ) {
        let frame = rw::EncodedVideoFrame {
            data: data.to_vec(),
            is_key_frame,
            width,
            height,
            rtp_timestamp,
        };
        py.allow_threads(|| match user_data {
            Some(ud) => self.inner.push_encoded_frame_with_metadata(frame, ud),
            None => self.inner.push_encoded_frame(frame),
        });
    }

    /// Add this track to a peer connection via `add_track` (creates a sendrecv
    /// transceiver automatically).
    fn add_to_peer_connection(&self, py: Python, pc: &PeerConnection) -> PyResult<()> {
        let track = self.inner.track();
        py.allow_threads(|| pc.pc().add_track(track)).map_err(err)
    }

    /// Add a transceiver of the given `direction` for this track. Returns the
    /// `Transceiver` (its `mid` is set after SDP exchange).
    fn add_transceiver(
        &self,
        py: Python,
        pc: &PeerConnection,
        direction: TransceiverDirection,
    ) -> PyResult<Transceiver> {
        let track = self.inner.track();
        let t = py
            .allow_threads(|| {
                pc.pc().add_transceiver(
                    rw::MediaKind::Video,
                    rw::TransceiverDirection::from(direction),
                )
            })
            .map_err(err)?;
        py.allow_threads(|| t.set_track(track)).map_err(err)?;
        Ok(Transceiver {
            inner: Some(Arc::new(t)),
        })
    }
}

// ── FrameAction ───────────────────────────────────────────────────────────────

/// What a FrameTransform callback should do with the frame.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    /// Forward the frame downstream (send it / hand it to the decoder).
    Forward = 0,
    /// Drop the frame: on receive this bypasses the decoder; on send nothing
    /// is transmitted.
    Drop = 1,
}

impl From<rw::FrameAction> for FrameAction {
    fn from(a: rw::FrameAction) -> Self {
        match a {
            rw::FrameAction::Forward => Self::Forward,
            rw::FrameAction::Drop => Self::Drop,
        }
    }
}

impl From<FrameAction> for rw::FrameAction {
    fn from(a: FrameAction) -> Self {
        match a {
            FrameAction::Forward => Self::Forward,
            FrameAction::Drop => Self::Drop,
        }
    }
}

// ── EncodedFrame ──────────────────────────────────────────────────────────────

/// A snapshot of an encoded frame passed to a `FrameTransform` callback.
///
/// `data` is a Python `bytes` object. Call `replace_data(new_bytes)` inside
/// the callback to substitute the payload; the new bytes are forwarded
/// downstream when the callback returns `FrameAction.Forward`.
#[pyclass]
pub struct EncodedFrame {
    data: Vec<u8>,
    is_key_frame: bool,
    ssrc: u32,
    timestamp: u32,
    capture_time_ms: i64,
    // Replacement written by replace_data(); read back by the transform
    // closure after the Python callback returns.
    replacement: Arc<Mutex<Option<Vec<u8>>>>,
}

#[pymethods]
impl EncodedFrame {
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.data)
    }
    #[getter]
    fn is_key_frame(&self) -> bool {
        self.is_key_frame
    }
    #[getter]
    fn ssrc(&self) -> u32 {
        self.ssrc
    }
    #[getter]
    fn timestamp(&self) -> u32 {
        self.timestamp
    }
    #[getter]
    fn capture_time_ms(&self) -> i64 {
        self.capture_time_ms
    }
    /// Replace this frame's encoded payload. Must be called inside the
    /// FrameTransform callback.
    fn replace_data(&self, new_data: Vec<u8>) {
        if let Ok(mut g) = self.replacement.lock() {
            *g = Some(new_data);
        }
    }
}

// ── FrameTransform ────────────────────────────────────────────────────────────

/// An encoded-frame transformer. Attach to a transceiver's sender or receiver
/// via `Transceiver.set_sender_transform` / `set_receiver_transform`.
///
/// Create from a Python callable with `FrameTransform(callback)` and attach
/// via `Transceiver.set_sender_transform()` / `set_receiver_transform()`.
#[pyclass]
pub struct FrameTransform {
    inner: rw::FrameTransform,
}

#[pymethods]
impl FrameTransform {
    /// Create a transform from a Python callable.
    ///
    /// Signature: `callback(frame: EncodedFrame) -> FrameAction`
    ///
    /// The callback runs on a WebRTC thread; acquire the GIL automatically.
    /// Call `frame.replace_data(bytes)` inside the callback to substitute the
    /// encoded payload before returning `FrameAction.Forward`.
    #[new]
    fn new(cb: PyObject) -> Self {
        let inner = rw::FrameTransform::new(move |frame| {
            Python::with_gil(|py| {
                let replacement = Arc::new(Mutex::new(None::<Vec<u8>>));
                let py_frame = match Py::new(
                    py,
                    EncodedFrame {
                        data: frame.data.to_vec(),
                        is_key_frame: frame.is_key_frame,
                        ssrc: frame.ssrc,
                        timestamp: frame.timestamp,
                        capture_time_ms: frame.capture_time_ms,
                        replacement: replacement.clone(),
                    },
                ) {
                    Ok(f) => f,
                    Err(_) => return rw::FrameAction::Forward,
                };
                let action = cb
                    .call1(py, (py_frame,))
                    .ok()
                    .and_then(|r| r.extract::<FrameAction>(py).ok())
                    .unwrap_or(FrameAction::Forward);
                if let Some(new_data) = replacement.lock().ok().and_then(|mut g| g.take()) {
                    frame.replace_data(&new_data);
                }
                rw::FrameAction::from(action)
            })
        });
        Self { inner }
    }
}

// ── Transceiver ───────────────────────────────────────────────────────────────

/// An RTP transceiver (one m-section in the SDP).
#[pyclass]
pub struct Transceiver {
    // Arc, not ManuallyDrop, so set_direction/set_codec_preferences can clone
    // a handle into a spawn_blocking future (which must be 'static) while
    // this Python object keeps its own handle. set_track stays on plain
    // allow_threads: it also needs Track's/EncodedVideoTrack's own inner
    // Arc-wrapped the same way to be awaitable
    // safely, which is out of scope here.
    inner: Option<Arc<rw::Transceiver>>,
}

impl Drop for Transceiver {
    fn drop(&mut self) {
        // Holds native sender and receiver references; releasing them dispatches
        // to the signaling thread, which may be inside Python. Taking the Arc out
        // and dropping it with the GIL released is correct whether or not this is
        // the last handle: on a refcount-only decrement it is just cheap, and on
        // the real native release it is what avoids deadlocking a callback that
        // re-enters Python.
        let inner = self.inner.take();
        Python::with_gil(|py| py.allow_threads(|| drop(inner)));
    }
}

impl Transceiver {
    /// Borrows the native transceiver. `inner` is only ever `None` after this
    /// object's own `Drop` has run, so every live call through Python sees `Some`.
    fn tc(&self) -> PyResult<&Arc<rw::Transceiver>> {
        self.inner
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Transceiver used after being dropped"))
    }
}

#[pymethods]
impl Transceiver {
    /// The `mid`, set after `set_local_description`.
    fn mid(&self, py: Python) -> PyResult<Option<String>> {
        let native = Arc::clone(self.tc()?);
        Ok(py.allow_threads(|| native.mid()))
    }

    /// Media kind (Audio or Video).
    fn kind(&self, py: Python) -> PyResult<MediaKind> {
        let native = Arc::clone(self.tc()?);
        Ok(MediaKind::from(py.allow_threads(|| native.kind())))
    }

    /// Attach a local track to the sender slot. Accepts either a `Track` or an
    /// `EncodedVideoTrack`.
    fn set_track(&self, py: Python, track: &Bound<'_, PyAny>) -> PyResult<()> {
        // Reaches the sender through several signaling-thread proxy calls, so the
        // GIL has to be free for the duration.
        let inner = Arc::clone(self.tc()?);
        if let Ok(t) = track.downcast::<Track>() {
            let t = t.borrow();
            // The borrow guard carries a GIL token, so the native reference has
            // to be taken out of it before the GIL is released.
            let native: &rw::Track = &t.inner;
            return py.allow_threads(|| inner.set_track(native)).map_err(err);
        }
        if let Ok(enc) = track.downcast::<EncodedVideoTrack>() {
            let enc = enc.borrow();
            let native: &rw::Track = enc.inner.track();
            return py.allow_threads(|| inner.set_track(native)).map_err(err);
        }
        Err(PyTypeError::new_err(
            "track must be a Track or EncodedVideoTrack",
        ))
    }

    /// Set the transceiver direction (SendOnly, RecvOnly, SendRecv, Inactive).
    /// Must be called before `create_answer()`/`create_offer()` for the change
    /// to appear in the SDP. Natively awaitable.
    fn set_direction<'py>(
        &self,
        py: Python<'py>,
        direction: TransceiverDirection,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.tc()?);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || {
                inner.set_direction(rw::TransceiverDirection::from(direction))
            })
            .await
            .map_err(join_err)?
            .map_err(err)
        })
    }

    /// Reorder this video transceiver's codec preferences: `codecs`, most
    /// preferred first, sort ahead of every other codec the endpoint
    /// supports; nothing is dropped. Must be called before
    /// `create_answer()`/`create_offer()` for the change to appear in the
    /// SDP. Raises if this transceiver carries audio, not video.
    ///
    /// Once negotiation completes, `PeerConnection.set_local_description`/
    /// `set_remote_description` also make this transceiver's own sender
    /// actually *encode* with whichever preferred codec was negotiated —
    /// SDP negotiation and a sender's codec selection are separate
    /// mechanisms in libwebrtc, and only the first is driven by preference
    /// order, so this is handled automatically rather than requiring a
    /// second call. Natively awaitable.
    fn set_codec_preferences<'py>(
        &self,
        py: Python<'py>,
        codecs: Vec<VideoCodec>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let native: Vec<rw::VideoCodec> = codecs.into_iter().map(rw::VideoCodec::from).collect();
        let inner = Arc::clone(self.tc()?);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || inner.set_codec_preferences(&native))
                .await
                .map_err(join_err)?
                .map_err(err)
        })
    }

    /// Attach a `FrameTransform` to the sender path of this transceiver.
    /// The transform runs after the encoder, before RTP packetization.
    fn set_sender_transform(&self, py: Python, transform: &FrameTransform) -> PyResult<()> {
        let native = Arc::clone(self.tc()?);
        py.allow_threads(|| native.set_sender_transform(&transform.inner))
            .map_err(err)
    }

    /// Attach a `FrameTransform` to the receiver path of this transceiver.
    /// The transform runs after RTP depacketization, before the decoder.
    fn set_receiver_transform(&self, py: Python, transform: &FrameTransform) -> PyResult<()> {
        let native = Arc::clone(self.tc()?);
        py.allow_threads(|| native.set_receiver_transform(&transform.inner))
            .map_err(err)
    }
}

// ── DataChannel ───────────────────────────────────────────────────────────────

/// An SCTP data channel for binary or text messaging.
#[pyclass]
pub struct DataChannel {
    inner: ManuallyDrop<rw::DataChannel>,
}

impl Drop for DataChannel {
    fn drop(&mut self) {
        // DataChannel callbacks (on_close, on_state_change) call Python::with_gil.
        // Release the GIL before dropping so those callbacks can fire without deadlocking.
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };
        Python::with_gil(|py| py.allow_threads(|| drop(inner)));
    }
}

#[pymethods]
impl DataChannel {
    fn label(&self, py: Python) -> String {
        py.allow_threads(|| self.inner.label())
    }
    fn state(&self, py: Python) -> DataChannelState {
        DataChannelState::from(py.allow_threads(|| self.inner.state()))
    }
    fn buffered_amount(&self, py: Python) -> u64 {
        py.allow_threads(|| self.inner.buffered_amount())
    }

    /// Send bytes. `binary=True` for binary SCTP messages, `False` for text.
    #[pyo3(signature = (data, binary = true))]
    fn send(&self, py: Python, data: &[u8], binary: bool) -> PyResult<()> {
        py.allow_threads(|| self.inner.send(data, binary))
            .map_err(err)
    }

    /// Register `callback(data: bytes, binary: bool)` for incoming messages.
    fn on_message(&mut self, py: Python, callback: PyObject) {
        // Registering re-registers the native observer, which dispatches to the
        // thread that delivers messages into Python. The GIL has to be free.
        py.allow_threads(|| {
            self.inner.on_message(move |data, binary| {
                Python::with_gil(|py| {
                    let bytes = PyBytes::new_bound(py, data);
                    let _ = callback.call1(py, (bytes, binary));
                });
            });
        });
    }

    /// Register `callback(state: DataChannelState)` for state transitions.
    fn on_state_change(&mut self, py: Python, callback: PyObject) {
        py.allow_threads(|| {
            self.inner.on_state_change(move |s| {
                Python::with_gil(|py| {
                    let _ = callback.call1(py, (DataChannelState::from(s),));
                });
            });
        });
    }

    /// Fire `callback()` once when the channel opens.
    fn on_open(&mut self, py: Python, callback: PyObject) {
        py.allow_threads(|| {
            self.inner.on_open(move || {
                Python::with_gil(|py| {
                    let _ = callback.call0(py);
                });
            });
        });
    }

    /// Fire `callback()` once when the channel closes.
    fn on_close(&mut self, py: Python, callback: PyObject) {
        py.allow_threads(|| {
            self.inner.on_close(move || {
                Python::with_gil(|py| {
                    let _ = callback.call0(py);
                });
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
                Python::with_gil(|py| match Py::new(py, IceCandidate::from(cand)) {
                    Ok(py_cand) => {
                        let _ = cb.call1(py, (py_cand,));
                    }
                    Err(e) => e.restore(py),
                });
            });
        }
        if let Some(cb) = &self.on_track {
            let cb = cb.clone_ref(py);
            obs = obs.on_track(move |kind, track| {
                Python::with_gil(|py| match Py::new(py, Track::from_rust(track)) {
                    Ok(py_track) => {
                        let _ = cb.call1(py, (MediaKind::from(kind), py_track));
                    }
                    Err(e) => e.restore(py),
                });
            });
        }
        if let Some(cb) = &self.on_data_channel {
            let cb = cb.clone_ref(py);
            obs = obs.on_data_channel(move |dc| {
                Python::with_gil(|py| {
                    match Py::new(
                        py,
                        DataChannel {
                            inner: ManuallyDrop::new(dc),
                        },
                    ) {
                        Ok(py_dc) => {
                            let _ = cb.call1(py, (py_dc,));
                        }
                        Err(e) => e.restore(py),
                    }
                });
            });
        }
        obs
    }
}

// ── PeerConnection ────────────────────────────────────────────────────────────

/// An RTCPeerConnection.
///
/// Signaling methods (`create_offer`, `create_answer`, `set_local_description`,
/// `set_remote_description`, `add_ice_candidate`, `get_stats`, `set_bitrate`,
/// `transceivers`) are natively awaitable — `await` them directly from an
/// async context, no executor wrapping needed. Every other method is a fast
/// synchronous call.
#[pyclass]
pub struct PeerConnection {
    inner: Option<Arc<rw::PeerConnection>>,
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        // pc->Close() dispatches synchronously to the signaling thread, which fires
        // on_connection_state_change(Closed) and tries Python::with_gil. If the GIL
        // is held by the Python GC thread that invoked this drop, both threads deadlock.
        // Release the GIL first so those callbacks can complete. Harmless when an
        // in-flight signaling call still holds another Arc clone — this then just
        // decrements the refcount; the real close happens when that clone drops
        // later, off the GIL entirely, on a tokio blocking-pool thread.
        let inner = self.inner.take();
        Python::with_gil(|py| py.allow_threads(|| drop(inner)));
    }
}

impl PeerConnection {
    /// Borrows the native connection. `inner` is only ever `None` after this
    /// object's own `Drop` has run — which means it no longer exists to call
    /// a method on, so every live call through Python sees `Some`.
    fn pc(&self) -> &Arc<rw::PeerConnection> {
        self.inner
            .as_ref()
            .expect("PeerConnection used after being dropped")
    }
}

#[pymethods]
impl PeerConnection {
    fn create_offer<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.pc());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || inner.create_offer())
                .await
                .map_err(join_err)?
                .map(SessionDescription::from)
                .map_err(err)
        })
    }
    fn create_answer<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.pc());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || inner.create_answer())
                .await
                .map_err(join_err)?
                .map(SessionDescription::from)
                .map_err(err)
        })
    }
    fn set_local_description<'py>(
        &self,
        py: Python<'py>,
        sdp: &SessionDescription,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.pc());
        let rust = to_rust_sdp(sdp)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || inner.set_local_description(&rust))
                .await
                .map_err(join_err)?
                .map_err(err)
        })
    }
    /// Apply the remote description, and arm this connection's
    /// `FrameMetadataGate` from it.
    ///
    /// The gate opens when `sdp` declares `FRAME_METADATA_ATTRIBUTE` and closes
    /// when it does not, on every call — so a renegotiation in which the peer
    /// drops support closes it again.
    fn set_remote_description<'py>(
        &self,
        py: Python<'py>,
        sdp: &SessionDescription,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.pc());
        let rust = to_rust_sdp(sdp)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || inner.set_remote_description(&rust))
                .await
                .map_err(join_err)?
                .map_err(err)
        })
    }

    /// A handle to this connection's frame-metadata gate.
    ///
    /// Cheap and shareable. It stays closed until `set_remote_description` sees
    /// a remote description that declares support. Reading it is diagnostic —
    /// the library consults it internally when answering and when appending
    /// trailers, so callers do not need to check it before pushing `user_data`.
    fn frame_metadata_gate(&self) -> FrameMetadataGate {
        FrameMetadataGate {
            inner: self.pc().frame_metadata_gate(),
        }
    }
    fn add_ice_candidate<'py>(
        &self,
        py: Python<'py>,
        candidate: &IceCandidate,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.pc());
        let rust = rw::IceCandidate::from(candidate);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || inner.add_ice_candidate(&rust))
                .await
                .map_err(join_err)?
                .map_err(err)
        })
    }
    fn add_track(&self, py: Python, track: &Track) -> PyResult<()> {
        py.allow_threads(|| self.pc().add_track(&track.inner))
            .map_err(err)
    }
    fn add_transceiver(
        &self,
        py: Python,
        kind: MediaKind,
        direction: TransceiverDirection,
    ) -> PyResult<Transceiver> {
        let rust_kind = match kind {
            MediaKind::Audio => rw::MediaKind::Audio,
            MediaKind::Video => rw::MediaKind::Video,
            MediaKind::Unknown => return Err(PyRuntimeError::new_err("need Audio or Video")),
        };
        py.allow_threads(|| {
            self.pc()
                .add_transceiver(rust_kind, rw::TransceiverDirection::from(direction))
        })
        .map(|t| Transceiver {
            inner: Some(Arc::new(t)),
        })
        .map_err(err)
    }
    fn create_data_channel(&self, py: Python, label: &str) -> PyResult<DataChannel> {
        py.allow_threads(|| self.pc().create_data_channel(label))
            .map(|inner| DataChannel {
                inner: ManuallyDrop::new(inner),
            })
            .map_err(err)
    }

    /// All transceivers on this peer connection, in offer m-section order after
    /// `set_remote_description`. Use this (together with `Transceiver.set_track`)
    /// to attach local tracks to the transceivers auto-created from the remote
    /// offer's recvonly m-sections. Natively awaitable.
    fn transceivers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.pc());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let ts = tokio::task::spawn_blocking(move || inner.transceivers())
                .await
                .map_err(join_err)?;
            Ok(ts
                .into_iter()
                .map(|t| Transceiver {
                    inner: Some(Arc::new(t)),
                })
                .collect::<Vec<_>>())
        })
    }

    /// Collect a stats snapshot from the WebRTC engine. Natively awaitable.
    /// Returns a `StatsReport` with inbound/outbound RTP streams and ICE
    /// candidate-pair metrics.
    fn get_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.pc());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || inner.get_stats())
                .await
                .map_err(join_err)?
                .map(StatsReport::from)
                .map_err(err)
        })
    }

    /// Adjust the bandwidth estimate limits and starting point. Natively awaitable.
    ///
    /// Pass `None` to leave a value at its libwebrtc default. A value of 0 is
    /// treated as `None` (unset). Units are bits-per-second.
    #[pyo3(signature = (min_bps=None, start_bps=None, max_bps=None))]
    fn set_bitrate<'py>(
        &self,
        py: Python<'py>,
        min_bps: Option<i32>,
        start_bps: Option<i32>,
        max_bps: Option<i32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(self.pc());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::task::spawn_blocking(move || inner.set_bitrate(min_bps, start_bps, max_bps))
                .await
                .map_err(join_err)?
                .map_err(err)
        })
    }
}

// ── PeerConnectionFactory ─────────────────────────────────────────────────────

/// Entry point — creates peer connections and media tracks.
///
/// Uses the **synthetic** (push-based) ADM by default (no audio hardware).
/// Pass `platform_adm=True` for real mic/speaker on desktop.
///
/// Only one `PeerConnectionFactory` may exist per process at a time.
/// Creating a second one while the first is alive raises `RuntimeError`.
#[pyclass]
pub struct PeerConnectionFactory {
    inner: rw::PeerConnectionFactory,
}

impl Drop for PeerConnectionFactory {
    fn drop(&mut self) {
        FACTORY_LIVE.store(false, Ordering::SeqCst);
    }
}

#[pymethods]
impl PeerConnectionFactory {
    #[new]
    #[pyo3(signature = (
        platform_adm=false,
        echo_canceller=false,
        noise_suppression=false,
        agc=false,
        high_pass_filter=false,
    ))]
    fn new(
        platform_adm: bool,
        echo_canceller: bool,
        noise_suppression: bool,
        agc: bool,
        high_pass_filter: bool,
    ) -> PyResult<Self> {
        claim_factory()?;
        let adm = if platform_adm {
            rw::AdmMode::Platform
        } else {
            rw::AdmMode::Synthetic
        };
        let apm = rw::ApmConfig {
            echo_canceller,
            noise_suppression,
            agc,
            high_pass_filter,
        };
        rw::PeerConnectionFactory::with_adm_apm(adm, apm)
            .map(|inner| Self { inner })
            .map_err(|e| {
                FACTORY_LIVE.store(false, Ordering::SeqCst);
                err(e)
            })
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
        // Creating a peer connection dispatches synchronously to the signaling
        // thread, which is also where another connection's observer callbacks
        // (an ICE candidate, say) re-enter Python. Holding the GIL across the
        // dispatch deadlocks the two: this thread waits for the signaling
        // thread, which waits for the GIL.
        py.allow_threads(|| self.inner.create_peer_connection(&rust_config, rust_obs))
            .map(|inner| PeerConnection {
                inner: Some(Arc::new(inner)),
            })
            .map_err(err)
    }

    /// Create a local video track (push frames via `Track.push_video_frame`).
    fn create_video_track(&self, py: Python, id: &str) -> PyResult<Track> {
        py.allow_threads(|| self.inner.create_video_track(id))
            .map(Track::from_rust)
            .map_err(err)
    }

    /// Create a local audio track. Feed samples via `push_audio_frame`.
    fn create_audio_track(&self, py: Python, id: &str) -> PyResult<Track> {
        py.allow_threads(|| self.inner.create_audio_track(id))
            .map(Track::from_rust)
            .map_err(err)
    }

    /// Create a local audio track with a per-track audio source.
    /// Feed samples via `track.push_pcm(pcm_bytes, sample_rate, channels)`.
    /// Each call returns an independent track — different audio can be pushed
    /// to different peer connections.
    fn create_audio_track_with_local_source(&self, py: Python, id: &str) -> PyResult<Track> {
        py.allow_threads(|| self.inner.create_audio_track_with_local_source(id))
            .map(Track::from_rust)
            .map_err(err)
    }

    /// Push interleaved signed 16-bit little-endian PCM to the synthetic ADM.
    /// `pcm` must have even length (2 bytes per i16 sample).
    fn push_audio_frame(
        &self,
        py: Python,
        pcm: &[u8],
        sample_rate: u32,
        channels: u32,
    ) -> PyResult<()> {
        if !pcm.len().is_multiple_of(2) {
            return Err(PyRuntimeError::new_err("pcm byte length must be even"));
        }
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        py.allow_threads(|| self.inner.push_audio_frame(&samples, sample_rate, channels));
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
        claim_factory()?;
        rw::PeerConnectionFactory::with_encoded_video_track(track_id, width, height)
            .map(|(f, t)| {
                (
                    Self { inner: f },
                    EncodedVideoTrack {
                        inner: ManuallyDrop::new(t),
                    },
                )
            })
            .map_err(|e| {
                FACTORY_LIVE.store(false, Ordering::SeqCst);
                err(e)
            })
    }

    fn set_adm_playout_enabled(&self, enabled: bool) {
        self.inner.set_adm_playout_enabled(enabled);
    }
}

// ── Module ────────────────────────────────────────────────────────────────────

#[pymodule]
fn reactor_webrtc(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Backs the awaitables PeerConnection's signaling methods return. Must be
    // multi_thread: nothing in this extension module ever calls block_on, so a
    // current_thread runtime would never be driven and spawned futures would
    // hang. One worker is enough — it only polls spawn_blocking join handles.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1);
    pyo3_async_runtimes::tokio::init(builder);

    m.add_class::<IceServer>()?;
    m.add_class::<BundlePolicy>()?;
    m.add_class::<TcpCandidatePolicy>()?;
    m.add_class::<RtcConfiguration>()?;
    m.add_class::<IceCandidate>()?;
    m.add_class::<SessionDescription>()?;
    m.add_class::<PeerConnectionState>()?;
    m.add_class::<IceGatheringState>()?;
    m.add_class::<DataChannelState>()?;
    m.add_class::<MediaKind>()?;
    m.add_class::<TransceiverDirection>()?;
    m.add_class::<VideoCodec>()?;
    m.add_class::<IceCandidatePairState>()?;
    m.add_class::<InboundRtpStats>()?;
    m.add_class::<OutboundRtpStats>()?;
    m.add_class::<IceCandidatePairStats>()?;
    m.add_class::<StatsReport>()?;
    m.add_class::<FrameMetadata>()?;
    m.add_class::<FrameMetadataGate>()?;
    m.add_class::<FrameAction>()?;
    m.add_class::<EncodedFrame>()?;
    m.add_class::<FrameTransform>()?;
    m.add_class::<Track>()?;
    m.add_class::<EncodedVideoTrack>()?;
    m.add_class::<Transceiver>()?;
    m.add_class::<DataChannel>()?;
    m.add_class::<PeerConnectionObserver>()?;
    m.add_class::<PeerConnection>()?;
    m.add_class::<PeerConnectionFactory>()?;

    // The SDP attribute peers declare frame-metadata support with, and the trailer
    // version this wheel speaks. Exposed so that a caller inspecting or building SDP
    // by hand does not have to hardcode either.
    m.add("FRAME_METADATA_ATTRIBUTE", rw::FRAME_METADATA_ATTRIBUTE)?;
    m.add("FRAME_METADATA_VERSION", rw::FRAME_METADATA_VERSION)?;
    Ok(())
}
