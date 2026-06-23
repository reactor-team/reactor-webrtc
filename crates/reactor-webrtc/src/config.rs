//! Peer-connection configuration (mirrors the PoC's `RtcConfiguration`).

/// A single ICE (STUN/TURN) server.
#[derive(Debug, Clone, Default)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub password: String,
}

/// When ICE candidates are gathered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinualGatheringPolicy {
    GatherOnce,
    GatherContinually,
}

/// Which ICE candidate types are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceTransportsType {
    All,
    Relay,
    NoHost,
    None,
}

/// Configuration passed to [`crate::PeerConnectionFactory::create_peer_connection`].
#[derive(Debug, Clone)]
pub struct RtcConfiguration {
    pub ice_servers: Vec<IceServer>,
    pub continual_gathering_policy: ContinualGatheringPolicy,
    pub ice_transport_type: IceTransportsType,
}

impl Default for RtcConfiguration {
    fn default() -> Self {
        Self {
            ice_servers: Vec::new(),
            continual_gathering_policy: ContinualGatheringPolicy::GatherContinually,
            ice_transport_type: IceTransportsType::All,
        }
    }
}
